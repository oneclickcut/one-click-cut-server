//! 画布内文件的上传、删除、列表与 manifest。
//!
//! # path 在这一层就规范化干净
//!
//! 禁 `..`、绝对路径、空段、控制字符，长度 ≤512 字节、段数 ≤16。
//! **进到 domain 层的 path 一定是干净的**——存储层的 `keys::validate_path`
//! 是最后一道兜底，不是主防线。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::Caller;
use super::error::{AppError, AppResult};
use crate::domain::canvas::{self, CanvasError};
use crate::domain::space;
use crate::storage::keys::{self, CanvasId};
use crate::util;

pub fn router(max_single_bytes: usize) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_files))
        .routes(routes!(get_manifest))
        .routes(routes!(delete_files_batch))
        // ==== 为什么这两条不用 routes! 宏 ====
        //
        // `routes!` 把 `#[utoipa::path]` 里的 path 原样当成 axum 路由，而
        // `{path}` 在 axum 里**只匹配一段**——`scene01/f001.jpg` 会 404。
        // 多段通配在 axum 0.8 里是 `{*path}`，但那个写法放进 OpenAPI 文档就
        // 变成一个名叫 `*path` 的参数，不符合规范。
        //
        // 所以分开写：文档用规范的 `{path}`（在 `openapi::ApiDoc` 的
        // `paths(...)` 里登记），axum 路由用 `{*path}`。两边都正确。
        .route(
            "/canvases/{id}/files/{*path}",
            axum::routing::put(put_file)
                .delete(delete_file)
                // axum 默认只收 2MB body，对这个项目远远不够——一个 jpg 素材
                // 就 200KB，mp4 更是几十 MB。超过上限返回 413，调用方改走
                // S7 的分块上传通道。
                .layer(axum::extract::DefaultBodyLimit::max(max_single_bytes)),
        )
}

/// 给别的模块复用（批量上传也要映射同一套错误）。
pub fn map_canvas_error_pub(e: CanvasError) -> AppError {
    map_canvas_error(e)
}

fn map_canvas_error(e: CanvasError) -> AppError {
    match e {
        // 画布不存在与不属于调用者合并成 404，与树接口一致。
        CanvasError::NotFound | CanvasError::FileNotFound => AppError::NotFound,
        CanvasError::TooManyFiles => AppError::Conflict(format!(
            "单个画布最多 {} 个文件",
            canvas::MAX_FILES_PER_CANVAS
        )),
        // 不变量 1：同一个路径不能同时是文件和目录。
        CanvasError::PathConflict(c) => AppError::Conflict(match c {
            canvas::PathConflict::TargetIsDir => "这个路径已经是一个目录".to_owned(),
            canvas::PathConflict::AncestorIsFile => {
                "路径上有一段已经是文件，同一个名字不能既是文件又是目录".to_owned()
            }
        }),
        // 507 Insufficient Storage：接口文档第 9 节指定的状态码。
        CanvasError::QuotaExceeded { used, limit } => {
            AppError::InsufficientStorage(format!("超出配额：已用 {used} 字节，上限 {limit} 字节"))
        }
    }
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct FileDto {
    #[schema(example = "scene01/f001.jpg")]
    pub path: String,
    pub size: u64,
    #[schema(example = "image/jpeg")]
    pub mime: String,
    /// 内容的完整 BLAKE3，64 字符 hex。也是 ETag 的取值。
    pub hash: String,
    pub updated_at: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FileListResponse {
    /// 按路径字典序返回。
    pub files: Vec<FileDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PutFileResponse {
    pub path: String,
    pub hash: String,
    pub size: u64,
    /// 带 `?v=` 的相对 URL，拼在画布的 `base_url` 后面即可。
    #[schema(example = "scene01/f001.jpg?v=1a2b3c4d")]
    pub url: String,
    /// 是否覆盖了同路径的旧文件。
    pub replaced: bool,
    /// 内容命中去重，没有写入新字节。
    pub deduplicated: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeletePaths {
    pub paths: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteFilesResponse {
    /// 实际删除的条数。不存在的 path 被跳过，不算失败。
    pub deleted: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ManifestFileDto {
    pub path: String,
    /// 相对 `base_url`，**播放器必须用这个而不是自己拼路径**——
    /// `?v=` 是缓存正确性的唯一保证。
    pub url: String,
    pub size: u64,
    pub mime: String,
    pub hash: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ManifestResponse {
    pub canvas_id: String,
    pub name: String,
    pub visibility: String,
    #[schema(example = "/f/k7Qm3xB9pLdR/")]
    pub base_url: String,
    /// 指向描述时间轴的 json。格式由播放器与 agent 约定，本服务不解析它。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    pub updated_at: u64,
    pub files: Vec<ManifestFileDto>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub exports_stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mp4_export: Option<serde_json::Value>,
}

impl From<canvas::Manifest> for ManifestResponse {
    fn from(m: canvas::Manifest) -> Self {
        Self {
            exports_stale: false,
            mp4_export: None,
            canvas_id: m.canvas_id,
            name: m.name,
            visibility: m.visibility.to_owned(),
            base_url: m.base_url,
            entry: m.entry,
            updated_at: m.updated_at,
            files: m
                .files
                .into_iter()
                .map(|f| ManifestFileDto {
                    path: f.path,
                    url: f.url,
                    size: f.size,
                    mime: f.mime,
                    hash: f.hash,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct MimeQuery {
    /// 覆盖内容类型的判定。省略时按扩展名推导。
    ///
    /// 只影响内容落进哪个列族（文本走 zstd、二进制不压缩），不落盘。
    pub mime: Option<String>,
    /// Protected storyboard updates require the hash of the version read by the caller.
    pub expected_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// 公共前置
// ---------------------------------------------------------------------------

/// 一次画布操作要求的权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// 只读元数据（列表、manifest）。
    Read,
    /// 写入文件。
    Write,
    /// 删除文件。
    Delete,
}

/// 定位画布并确认调用者有权做这件事。
///
/// # 三类主体在这里汇合
///
/// | 主体 | 能操作哪个画布 | 权限 |
/// |------|----------------|------|
/// | 用户 JWT | 自己名下的 | 读写删 |
/// | 管理员 JWT | 任意（记审计日志） | 读写删 |
/// | Agent Token | **只有它绑定的那一个** | 按权限位，默认只有写 |
///
/// 跨用户一律 404（不泄露存在性）；权限位不足返回 403（凭证没问题，是权限不够）。
pub async fn authorize_canvas(
    state: &AppState,
    caller: &Caller,
    canvas_id: &str,
    need: Need,
) -> AppResult<(crate::storage::keys::Uid, CanvasId)> {
    caller.ensure_current(state).await?;
    let canvas_id = CanvasId::parse(canvas_id).map_err(|_| AppError::NotFound)?;

    let owner = match caller {
        Caller::Agent { uid, .. } => *uid,
        Caller::User { uid, role } => {
            if space::get_canvas(&state.store, *uid, canvas_id)
                .await?
                .is_some()
            {
                *uid
            } else if role.is_admin() {
                let owner = find_canvas_owner(state, canvas_id)
                    .await?
                    .ok_or(AppError::NotFound)?;
                super::auth::audit_admin_action(*uid, "canvas_files", owner, &canvas_id.to_text());
                owner
            } else {
                return Err(AppError::NotFound);
            }
        }
    };
    let canvas = space::get_canvas(&state.store, owner, canvas_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Caller::Agent {
        project_id, perms, ..
    } = caller
    {
        if canvas.project_id != *project_id {
            return Err(AppError::NotFound);
        }
        let allowed = match (canvas.role, need) {
            (_, Need::Read) => true,
            (crate::storage::records::CanvasRole::Production, Need::Write) => {
                perms.canvas_files_write
            }
            (crate::storage::records::CanvasRole::Production, Need::Delete) => {
                perms.canvas_files_delete
            }
            (crate::storage::records::CanvasRole::Resources, Need::Write) => {
                perms.constraints_write
            }
            (crate::storage::records::CanvasRole::Resources, Need::Delete) => {
                perms.constraints_delete
            }
        };
        if !allowed {
            return Err(AppError::Forbidden);
        }
    }
    let project = space::get_project(&state.store, owner, canvas.project_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if need != Need::Read && project.status != crate::storage::records::ProjectStatus::Active {
        return Err(AppError::Conflict("项目正在删除".into()));
    }
    if need != Need::Read {
        let hash = match caller {
            Caller::Agent { token_hash, .. } => Some(token_hash.to_hex()),
            _ => None,
        };
        crate::domain::jobs::authorize_write(
            &state.store,
            canvas.project_id,
            hash.as_deref(),
            crate::util::now_secs(),
        )
        .await
        .map_err(|e| AppError::Conflict(e.to_string()))?;
    }
    Ok((owner, canvas_id))
}

/// 给别的模块复用的入口（agent_api 也要按 canvas_id 找所有者）。
pub async fn find_canvas_owner_pub(
    state: &AppState,
    canvas_id: CanvasId,
) -> AppResult<Option<crate::storage::keys::Uid>> {
    find_canvas_owner(state, canvas_id).await
}

/// 全库找一个画布的所有者。只在管理员路径上用。
async fn find_canvas_owner(
    state: &AppState,
    canvas_id: CanvasId,
) -> AppResult<Option<crate::storage::keys::Uid>> {
    // `c|{uid}|{canvas_id}` 的前缀是 uid，所以只能全扫 `c|`。
    let rows = state
        .store
        .scan_prefix_keys(crate::storage::Cf::Meta, keys::canvas_prefix(), usize::MAX)
        .await?;
    for key in rows {
        let Ok((uid, found)) = keys::parse_canvas_key(&key) else {
            continue;
        };
        if found == canvas_id {
            return Ok(Some(uid));
        }
    }
    Ok(None)
}

/// 规范化并校验画布内相对路径。
///
/// axum 的 `{*path}` 通配会把整段剩余路径给我们，可能带前导斜杠或百分号编码
/// （axum 已解码），所以要在这里统一处理。
fn normalize_path(raw: &str) -> AppResult<String> {
    let trimmed = raw.trim_start_matches('/');
    keys::validate_path(trimmed).map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(trimmed.to_owned())
}

// ---------------------------------------------------------------------------
// 接口
// ---------------------------------------------------------------------------

/// 列出画布内全部文件
#[utoipa::path(
    summary = "列出画布文件",
    get,
    path = "/canvases/{id}/files",
    tag = "files",
    params(("id" = String, Path, description = "画布 id")),
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "按路径字典序的文件列表", body = FileListResponse),
        (status = 404, description = "画布不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
pub(crate) async fn list_files(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> AppResult<Json<FileListResponse>> {
    let (_, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Read).await?;

    let rows = canvas::list_files(&state.store, canvas_id, usize::MAX).await?;
    Ok(Json(FileListResponse {
        files: rows
            .into_iter()
            .map(|row| FileDto {
                mime: canvas::mime_for_path(&row.path),
                path: row.path,
                size: row.entry.size,
                hash: row.entry.hash.to_hex(),
                updated_at: row.entry.updated_at,
            })
            .collect(),
    }))
}

/// 画布 manifest
///
/// 播放器契约。与 `/f/{slug}/manifest.json` 同内容，
/// 区别只是这里需要凭证、那里公开。
#[utoipa::path(
    summary = "获取画布播放清单",
    get,
    path = "/canvases/{id}/manifest",
    tag = "files",
    params(("id" = String, Path, description = "画布 id")),
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "播放器契约", body = ManifestResponse),
        (status = 404, description = "画布不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
async fn get_manifest(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> AppResult<Json<ManifestResponse>> {
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Read).await?;

    let c = space::get_canvas(&state.store, uid, canvas_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows = canvas::list_files(&state.store, canvas_id, usize::MAX).await?;

    let mut dto: ManifestResponse = canvas::build_manifest(canvas_id, &c, &rows).into();
    let export_state = crate::domain::mp4_jobs::state(&state.store, canvas_id).await?;
    dto.exports_stale = export_state["exports_stale"].as_bool().unwrap_or(false);
    dto.mp4_export = export_state["published"]
        .as_object()
        .map(|_| export_state["published"].clone());
    Ok(Json(dto))
}

/// 上传或覆盖单个文件
///
/// body 是原始字节。`path` 允许 `scene01/f001.jpg` 这种带斜杠的逻辑目录，
/// 但不存在真实的目录实体。
///
/// 超过 32MB 的内容建议走 S7 的分块上传通道：Cloudflare 免费版单请求体上限
/// 100MB，而分块还顺带提供断点续传。
#[utoipa::path(
    summary = "上传或覆盖画布文件",
    put,
    // 注意这里写的是**完整路径**（带 `/api`），而同文件里走 `routes!` 宏的接口
    // 写的是相对路径。原因：`routes!` 登记的 path 会被 `nest("/api", ..)` 加上
    // 前缀，而这条是手工登记进 `ApiDoc` 的，不经过 nest，所以要自己写全。
    path = "/api/canvases/{id}/files/{path}",
    tag = "files",
    params(
        ("id" = String, Path, description = "画布 id"),
        ("path" = String, Path, description = "画布内相对路径，可含斜杠"),
        MimeQuery,
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "已写入", body = PutFileResponse),
        (status = 400, description = "路径不合规", body = super::error::ErrorBody),
        (status = 404, description = "画布不存在或不属于调用者", body = super::error::ErrorBody),
        (status = 409, description = "超出单画布文件数上限", body = super::error::ErrorBody),
        (status = 413, description = "请求体超过单次上传上限，改走分块上传", body = super::error::ErrorBody),
    ),
)]
pub async fn put_file(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, path)): Path<(String, String)>,
    Query(q): Query<MimeQuery>,
    body: Bytes,
) -> AppResult<Json<PutFileResponse>> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Write).await?;
    let path = normalize_path(&path)?;

    check_storyboard_write(&state, canvas_id, &path, q.expected_hash.as_deref()).await?;
    // mime 只决定落进哪个内容列族。调用方不传就按扩展名推。
    let mime = q.mime.unwrap_or_else(|| canvas::mime_for_path(&path));

    let outcome = canvas::put_file(
        &state.store,
        uid,
        canvas_id,
        &path,
        &mime,
        &body,
        util::now_secs(),
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;

    match outcome {
        Ok(o) => Ok(Json(PutFileResponse {
            url: format!("{path}?v={}", o.hash.to_hex8()),
            path,
            hash: o.hash.to_hex(),
            size: o.size,
            replaced: o.replaced,
            deduplicated: o.deduplicated,
        })),
        Err(e) => Err(map_canvas_error(e)),
    }
}

/// 删除单个文件
#[utoipa::path(
    summary = "删除画布文件",
    delete,
    // 同上：手工登记进 ApiDoc，要写完整路径。
    path = "/api/canvases/{id}/files/{path}",
    tag = "files",
    params(
        ("id" = String, Path, description = "画布 id"),
        ("path" = String, Path, description = "画布内相对路径"),
    ),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 204, description = "已删除"),
        (status = 404, description = "画布或文件不存在", body = super::error::ErrorBody),
    ),
)]
pub async fn delete_file(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, path)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Delete).await?;
    let path = normalize_path(&path)?;

    // 单文件删除要区分「文件不存在」，返回 404——与批量删除的宽容语义不同，
    // 因为这里调用方明确指定了一个文件。
    let deleted = canvas::delete_files(
        &state.store,
        uid,
        canvas_id,
        std::slice::from_ref(&path),
        util::now_secs(),
    )
    .await?
    .map_err(map_canvas_error)?;

    if deleted == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// 批量删除文件
///
/// 不存在的 path 被跳过，返回实际删除的条数。整批在一个 WriteBatch 里提交。
#[utoipa::path(
    summary = "批量删除画布文件",
    post,
    path = "/canvases/{id}/files:delete",
    tag = "files",
    params(("id" = String, Path, description = "画布 id")),
    request_body = DeletePaths,
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "已删除，返回实际条数", body = DeleteFilesResponse),
        (status = 404, description = "画布不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
async fn delete_files_batch(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<DeletePaths>,
) -> AppResult<Json<DeleteFilesResponse>> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Delete).await?;

    let deleted = canvas::delete_files(&state.store, uid, canvas_id, &body.paths, util::now_secs())
        .await?
        .map_err(map_canvas_error)?;

    Ok(Json(DeleteFilesResponse { deleted }))
}

/// A repaired storyboard can only be replaced using a conditional single-file PUT.
pub async fn check_storyboard_write(
    state: &AppState,
    cid: CanvasId,
    path: &str,
    expected: Option<&str>,
) -> AppResult<()> {
    if path != "storyboard.json" {
        return Ok(());
    }
    let protected = state
        .store
        .get(crate::storage::Cf::Meta, keys::resource_version_key(cid))
        .await?
        .is_some();
    if !protected && expected.is_none() {
        return Ok(());
    }
    let current = canvas::get_file(&state.store, cid, path).await?;
    if expected.is_none() || current.is_none_or(|f| Some(f.hash.to_hex().as_str()) != expected) {
        return Err(AppError::Conflict(
            "分镜已受版本保护，请使用读取时的 expected_hash 单文件上传".into(),
        ));
    }
    Ok(())
}
