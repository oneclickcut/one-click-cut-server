//! 管理接口。**全部要求 `role=admin`**。
//!
//! 这是唯一能跨用户操作的身份，所以每个越权动作都会写审计日志
//! （谁、什么时候、动了谁的哪个资源）。

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::{self, AdminCaller};
use super::error::{AppError, AppResult};
use crate::domain::{identity, maintenance, plans, quota, space};
use crate::storage::keys::{ParentId, Uid};
use crate::storage::{Cf, keys, records};
use crate::util;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users))
        .routes(routes!(patch_user))
        .routes(routes!(stats))
        .routes(routes!(run_gc))
        .routes(routes!(run_compact))
        .routes(routes!(list_canvases))
        .routes(routes!(create_checkpoint))
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct AdminUserDto {
    pub auto_share_gallery: bool,
    pub can_disable_auto_share_gallery: bool,
    pub uid: String,
    pub username: String,
    pub role: String,
    pub bytes_used: u64,
    /// 0 表示不限。
    pub quota_bytes: u64,
    pub disabled: bool,
    pub created_at: u64,
    #[schema(value_type = Option<String>)]
    pub plan: Option<plans::Plan>,
    pub plan_name: Option<&'static str>,
    pub plan_started_at: Option<u64>,
    pub monthly_push_limit: Option<u64>,
    pub push_used: Option<u64>,
    pub push_remaining: Option<u64>,
    pub period_start: Option<u64>,
    pub next_reset_at: Option<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UserListResponse {
    pub users: Vec<AdminUserDto>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PatchUser {
    #[schema(value_type = Option<String>)]
    pub plan: Option<plans::Plan>,
    /// Unix 秒，UTC。只能设为当前或过去。
    pub plan_started_at: Option<u64>,
    /// 配额上限字节数。0 表示不限。
    pub quota_bytes: Option<u64>,
    /// 禁用账户。**立即生效**：已签发的 token 与它的 agent token 都会失效。
    pub disabled: Option<bool>,
    /// 重设密码。响应里不回显。
    pub password: Option<String>,
    /// 重新统计该用户的实际占用（对账）。
    pub recount: Option<bool>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatsResponse {
    /// 持久化生成完成幂等回执数量，随项目删除清理。
    pub gallery_completion_receipts: u64,
    pub users: u64,
    pub canvases: u64,
    pub files: u64,
    /// 画布内目录记录数。不参与配额与容量统计，只为观察规模。
    pub canvas_dirs: u64,
    /// 用户视角的逻辑总量（全部画布 `bytes` 之和）。
    pub logical_bytes: u64,
    /// 去重后的内容份数。
    pub blobs: u64,
    pub chunks: u64,
    /// 数据目录实际占用。
    pub disk_bytes: u64,
    /// 逻辑量 / 物理量。大于 1 说明去重在起作用。
    pub dedup_ratio: f64,
    /// 待删队列深度。
    pub gc_pending: u64,
    /// 未 finalize 的上传会话数。
    pub upload_sessions: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GcRunResponse {
    pub blobs_deleted: u64,
    pub chunks_deleted: u64,
    /// 到期但因引用复活而跳过的项。
    pub revived: u64,
    /// 对账发现并清理的孤儿块。
    pub orphans_purged: u64,
    /// 对账发现的悬挂 blobmeta。**非零表示数据损坏，需要人工介入。**
    pub dangling_blobs: u64,
    pub agent_tokens_purged: u64,
    pub upload_sessions_purged: u64,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct GcQuery {
    /// 是否同时跑一次对账（扫全部内容键，较慢）。默认 true。
    pub audit: Option<bool>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct CanvasQuery {
    /// 要查看的用户 uid。
    pub uid: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AdminCanvasDto {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub visibility: String,
    pub file_count: u64,
    pub bytes: u64,
    pub updated_at: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CanvasListResponse {
    pub canvases: Vec<AdminCanvasDto>,
}

// ---------------------------------------------------------------------------
// 接口
// ---------------------------------------------------------------------------

/// 用户列表与各自容量占用
#[utoipa::path(
    summary = "列出用户和容量用量",
    get,
    path = "/admin/users",
    tag = "admin",
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "全部用户", body = UserListResponse),
        (status = 403, description = "需要管理员身份", body = super::error::ErrorBody),
    ),
)]
async fn list_users(
    State(state): State<AppState>,
    _admin: AdminCaller,
) -> AppResult<Json<UserListResponse>> {
    let rows = state
        .store
        .scan_prefix_limit(Cf::Meta, keys::user_prefix(), usize::MAX)
        .await?;

    let mut users = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let Ok(uid) = keys::parse_user_key(&key) else {
            tracing::warn!(len = key.len(), "用户记录键异常，跳过");
            continue;
        };
        let user: records::User = records::decode(&value)?;
        users.push(user_dto(&state, uid, &user).await?);
    }
    Ok(Json(UserListResponse { users }))
}

/// 调整配额、禁用账户、重设密码、对账
#[utoipa::path(
    summary = "修改用户状态或配额",
    patch,
    path = "/admin/users/{uid}",
    tag = "admin",
    params(("uid" = String, Path, description = "用户 uid")),
    request_body = PatchUser,
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "已更新", body = AdminUserDto),
        (status = 400, description = "密码不合规", body = super::error::ErrorBody),
        (status = 403, description = "需要管理员身份", body = super::error::ErrorBody),
        (status = 404, description = "用户不存在", body = super::error::ErrorBody),
    ),
)]
async fn patch_user(
    State(state): State<AppState>,
    admin: AdminCaller,
    Path(uid): Path<String>,
    Json(body): Json<PatchUser>,
) -> AppResult<Json<AdminUserDto>> {
    let _guard = state.operation_lock.lock().await;
    let target = Uid::parse(&uid).map_err(|_| AppError::NotFound)?;
    let mut user = identity::find_by_uid(&state.store, target)
        .await?
        .ok_or(AppError::NotFound)?;

    // 这是唯一能跨用户操作的身份，动作必须留痕。
    auth::audit_admin_action(admin.uid, "patch_user", target, &user.username);

    if user.role.is_admin() && (body.plan.is_some() || body.plan_started_at.is_some()) {
        return Err(AppError::BadRequest("管理员账号不使用套餐".into()));
    }
    if let Some(start) = body.plan_started_at {
        if start > util::now_secs() {
            return Err(AppError::BadRequest("开通时间不能晚于当前时间".into()));
        }
    }

    let mut plan_update = None;
    let mut plan_change = None;
    if !user.role.is_admin() && (body.plan.is_some() || body.plan_started_at.is_some()) {
        let mut current = plans::get(&state.store, target, user.created_at).await?;
        let new_plan = body.plan.unwrap_or(current.plan);
        let new_start = body.plan_started_at.unwrap_or_else(|| {
            if new_plan != current.plan {
                util::now_secs()
            } else {
                current.plan_started_at
            }
        });
        if new_plan != current.plan || new_start != current.plan_started_at {
            plan_change = Some((current.plan, current.plan_started_at, new_plan, new_start));
            let (period_start, _) = plans::period(new_start, util::now_secs())?;
            current.plan = new_plan;
            current.plan_started_at = new_start;
            current.push_period_start = period_start;
            current.push_used = 0;
            user.quota_bytes = new_plan.limits(&state.config.plans).storage_bytes;
            plan_update = Some(current);
        }
    }

    if let Some(limit) = body.quota_bytes {
        if body.plan.is_some() || body.plan_started_at.is_some() {
            return Err(AppError::BadRequest("不能同时指定套餐和独立配额".into()));
        }
        user.quota_bytes = limit;
    }
    if let Some(disabled) = body.disabled {
        user.disabled = disabled;
    }
    if let Some(pw) = &body.password {
        identity::validate_password(pw).map_err(|e| AppError::BadRequest(e.to_string()))?;
        user.pwd_hash = identity::hash_password(pw).map_err(AppError::Internal)?;
        // 重设密码不撤销已签发 JWT；需要立即阻断时禁用账号。
        tracing::warn!(audit = true, admin = %admin.uid, target = %target, "管理员重设了密码");
    }
    if let Some(plan_state) = plan_update {
        let mut batch = crate::storage::Batch::new();
        batch.put(Cf::Meta, keys::user_key(target), records::encode(&user)?);
        batch.put(
            Cf::Meta,
            keys::user_plan_key(target),
            plans::encode(&plan_state)?,
        );
        state.store.write(batch).await?;
        if let Some((old_plan, old_start, new_plan, new_start)) = plan_change {
            tracing::info!(audit = true, admin = %admin.uid, target = %target,
                old_plan = ?old_plan, new_plan = ?new_plan,
                old_start, new_start, "管理员修改套餐或开通时间");
        }
    } else {
        identity::save(&state.store, target, &user).await?;
    }

    if body.recount.unwrap_or(false) {
        quota::recount(&state.store, target).await?;
        user = identity::find_by_uid(&state.store, target)
            .await?
            .ok_or(AppError::NotFound)?;
    }

    Ok(Json(user_dto(&state, target, &user).await?))
}

async fn user_dto(state: &AppState, uid: Uid, user: &records::User) -> AppResult<AdminUserDto> {
    let plan = if user.role.is_admin() {
        None
    } else {
        Some(
            plans::get(&state.store, uid, user.created_at)
                .await?
                .current(util::now_secs())?,
        )
    };
    let (plan_code, plan_name, started, limit, used, remaining, period_start, next) =
        if let Some((p, next)) = plan {
            let limit = p.plan.limits(&state.config.plans).monthly_push_limit;
            (
                Some(p.plan),
                Some(p.plan.name()),
                Some(p.plan_started_at),
                Some(limit),
                Some(p.push_used),
                Some(limit.saturating_sub(p.push_used)),
                Some(p.push_period_start),
                Some(next),
            )
        } else {
            (None, None, None, None, None, None, None, None)
        };
    let pref: Option<crate::domain::gallery_policy::Preference> =
        crate::domain::gallery_policy::read_json(&state.store, keys::gallery_preference_key(uid))
            .await?;
    let customizable = user.role.is_admin() || plan_code != Some(plans::Plan::Basic);
    Ok(AdminUserDto {
        auto_share_gallery: !customizable || pref.is_none_or(|p| p.auto_share_gallery),
        can_disable_auto_share_gallery: customizable && !user.disabled,
        uid: uid.to_text(),
        username: user.username.clone(),
        role: if user.role.is_admin() {
            "admin"
        } else {
            "user"
        }
        .to_owned(),
        bytes_used: user.bytes_used,
        quota_bytes: user.quota_bytes,
        disabled: user.disabled,
        created_at: user.created_at,
        plan: plan_code,
        plan_name,
        plan_started_at: started,
        monthly_push_limit: limit,
        push_used: used,
        push_remaining: remaining,
        period_start,
        next_reset_at: next,
    })
}

/// 全局统计
///
/// 走全表扫描，别放进任何高频路径或监控轮询。
#[utoipa::path(
    summary = "获取服务存储统计",
    get,
    path = "/admin/stats",
    tag = "admin",
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "全局统计", body = StatsResponse),
        (status = 403, description = "需要管理员身份", body = super::error::ErrorBody),
    ),
)]
async fn stats(
    State(state): State<AppState>,
    _admin: AdminCaller,
) -> AppResult<Json<StatsResponse>> {
    let s = maintenance::collect_stats(&state.store).await?;
    Ok(Json(StatsResponse {
        gallery_completion_receipts: s.gallery_completion_receipts,
        users: s.users,
        canvases: s.canvases,
        files: s.files,
        canvas_dirs: s.canvas_dirs,
        logical_bytes: s.logical_bytes,
        blobs: s.blobs,
        chunks: s.chunks,
        disk_bytes: s.disk_bytes,
        dedup_ratio: s.dedup_ratio(),
        gc_pending: s.gc_pending,
        upload_sessions: s.upload_sessions,
    }))
}

/// 手动触发一轮 GC 与对账
#[utoipa::path(
    summary = "执行垃圾回收",
    post,
    path = "/admin/gc/run",
    tag = "admin",
    params(GcQuery),
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "一轮结果", body = GcRunResponse),
        (status = 403, description = "需要管理员身份", body = super::error::ErrorBody),
    ),
)]
async fn run_gc(
    State(state): State<AppState>,
    admin: AdminCaller,
    Query(q): Query<GcQuery>,
) -> AppResult<Json<GcRunResponse>> {
    let _guard = state.operation_lock.lock().await;
    tracing::info!(audit = true, admin = %admin.uid, "手动触发 GC");
    let now = util::now_secs();

    let report = crate::storage::gc::run_once(&state.store, now, 100_000).await?;
    let purge = maintenance::purge_credentials(&state.store, now).await?;

    let (orphans_purged, dangling) = if q.audit.unwrap_or(true) {
        let audit = crate::storage::gc::audit(&state.store, usize::MAX).await?;
        let purged = crate::storage::gc::purge_orphans(&state.store, &audit.orphan_chunks).await?;
        // 悬挂 blobmeta 只报数不自动处理——那是数据损坏，清理等于承认数据丢了。
        (purged, audit.dangling_blobs.len() as u64)
    } else {
        (0, 0)
    };

    Ok(Json(GcRunResponse {
        blobs_deleted: report.blobs_deleted,
        chunks_deleted: report.chunks_deleted,
        revived: report.revived,
        orphans_purged,
        dangling_blobs: dangling,
        agent_tokens_purged: purge.agent_tokens,
        upload_sessions_purged: purge.upload_sessions,
    }))
}

/// 手动触发 compaction
///
/// 删除大量数据后要跑一次：RocksDB 的删除只是写 tombstone，**空间回收是异步的**。
/// 不跑这一步，磁盘不会真的空出来。
#[utoipa::path(
    summary = "执行数据库压缩",
    post,
    path = "/admin/compact",
    tag = "admin",
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "compaction 前后的目录占用", body = CompactResponse),
        (status = 403, description = "需要管理员身份", body = super::error::ErrorBody),
    ),
)]
async fn run_compact(
    State(state): State<AppState>,
    admin: AdminCaller,
) -> AppResult<Json<CompactResponse>> {
    tracing::info!(audit = true, admin = %admin.uid, "手动触发 compaction");

    let before = maintenance::collect_stats(&state.store).await?.disk_bytes;
    state.store.flush().await?;
    state.store.compact_all().await?;
    let after = maintenance::collect_stats(&state.store).await?.disk_bytes;

    Ok(Json(CompactResponse {
        disk_bytes_before: before,
        disk_bytes_after: after,
        reclaimed: before.saturating_sub(after),
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CompactResponse {
    pub disk_bytes_before: u64,
    pub disk_bytes_after: u64,
    pub reclaimed: u64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CheckpointRequest {
    /// 快照目标目录。**必须不存在**——不覆盖已有备份。
    ///
    /// 省略时用 `{data_dir}/backup/{UTC 时间戳}`。
    pub target: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CheckpointResponse {
    pub target: String,
    /// 快照里的文件数。硬链接，所以实际占用远小于表观大小。
    pub files: u64,
    pub apparent_bytes: u64,
}

/// 生成一致性快照
///
/// 单机单盘是这套架构最大的风险，**备份是唯一防线**。checkpoint 是原子的：
/// 不会拍到「元数据新、内容旧」的中间状态，也不阻塞写入。
#[utoipa::path(
    summary = "创建数据库检查点",
    post,
    path = "/admin/checkpoint",
    tag = "admin",
    request_body = CheckpointRequest,
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "快照已生成", body = CheckpointResponse),
        (status = 400, description = "目标已存在或不可写", body = super::error::ErrorBody),
        (status = 403, description = "需要管理员身份", body = super::error::ErrorBody),
    ),
)]
async fn create_checkpoint(
    State(state): State<AppState>,
    admin: AdminCaller,
    Json(body): Json<CheckpointRequest>,
) -> AppResult<Json<CheckpointResponse>> {
    let target = match body.target {
        Some(t) => std::path::PathBuf::from(t),
        None => {
            // 时间戳用 UTC 秒：跨时区不会撞，也不需要额外依赖。
            state
                .config
                .backup_dir()
                .join(format!("cp-{}", util::now_secs()))
        }
    };

    tracing::info!(audit = true, admin = %admin.uid, target = %target.display(), "生成快照");

    state
        .store
        .checkpoint(&target)
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    let (files, bytes) = count_dir(&target);
    tracing::info!(target = %target.display(), files, "快照完成");

    Ok(Json(CheckpointResponse {
        target: target.display().to_string(),
        files,
        apparent_bytes: bytes,
    }))
}

/// 数一个目录里的文件数与表观字节数。
fn count_dir(path: &std::path::Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return (0, 0);
    };
    let mut files = 0u64;
    let mut bytes = 0u64;
    for e in entries.filter_map(Result::ok) {
        match e.file_type() {
            Ok(t) if t.is_dir() => {
                let (f, b) = count_dir(&e.path());
                files += f;
                bytes += b;
            }
            Ok(_) => {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
            Err(_) => {}
        }
    }
    (files, bytes)
}

/// 查看任意用户的画布列表
#[utoipa::path(
    summary = "列出全部用户的画布",
    get,
    path = "/admin/canvases",
    tag = "admin",
    params(CanvasQuery),
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "该用户的画布", body = CanvasListResponse),
        (status = 403, description = "需要管理员身份", body = super::error::ErrorBody),
        (status = 404, description = "用户不存在", body = super::error::ErrorBody),
    ),
)]
async fn list_canvases(
    State(state): State<AppState>,
    admin: AdminCaller,
    Query(q): Query<CanvasQuery>,
) -> AppResult<Json<CanvasListResponse>> {
    let target = Uid::parse(&q.uid).map_err(|_| AppError::NotFound)?;
    if identity::find_by_uid(&state.store, target).await?.is_none() {
        return Err(AppError::NotFound);
    }
    auth::audit_admin_action(admin.uid, "list_canvases", target, "all");

    // 递归收集整棵树里的画布。管理员看的是「这个用户有哪些画布」，
    // 不关心它们在哪个文件夹下。
    let mut canvases = Vec::new();
    let mut stack = vec![ParentId::Root];
    while let Some(parent) = stack.pop() {
        for node in space::list_children(&state.store, target, parent, usize::MAX).await? {
            match node {
                space::Node::Folder { id, .. } => stack.push(ParentId::Folder(id)),
                space::Node::Project { id, .. } => {
                    for (id, canvas) in
                        space::list_project_canvases(&state.store, target, id).await?
                    {
                        if canvas.role != records::CanvasRole::Production {
                            continue;
                        }
                        canvases.push(AdminCanvasDto {
                            id: id.to_text(),
                            name: canvas.name,
                            slug: canvas.slug,
                            visibility: match canvas.visibility {
                                records::Visibility::Public => "public",
                                records::Visibility::Private => "private",
                            }
                            .to_owned(),
                            file_count: canvas.file_count,
                            bytes: canvas.bytes,
                            updated_at: canvas.updated_at,
                        });
                    }
                }
            }
        }
    }

    Ok(Json(CanvasListResponse { canvases }))
}
