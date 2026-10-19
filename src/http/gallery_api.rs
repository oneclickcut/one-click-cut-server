//! 匿名画廊读取与用户 / 管理员共享管理。
use axum::{
    Json,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    AppState,
    auth::{self, Caller, UserCaller},
    error::{AppError, AppResult, gallery_error},
    project_api,
};
use crate::{
    domain::{gallery, gallery_policy, identity},
    storage::{
        Batch, Cf,
        keys::{self, CanvasId, ProjectId, ShareId, Uid},
        records::{self, GalleryTarget, ProjectShare, ShareConfig, ShareCover},
    },
    util,
};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_share, put_share, delete_share))
        .routes(routes!(list_gallery))
        .routes(routes!(get_gallery))
        .routes(routes!(toggle_sharing))
        .routes(routes!(update_preferences))
        .routes(routes!(generation_completion))
        .layer(DefaultBodyLimit::max(32 * 1024))
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ShareCoverDto {
    pub canvas_id: String,
    pub path: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ShareRequest {
    pub title: String,
    pub description: String,
    pub author_name: String,
    pub canvas_ids: Vec<String>,
    /// 必须提供；null 表示无封面。
    #[serde(deserialize_with = "required_cover")]
    #[schema(required = true)]
    pub cover: Option<ShareCoverDto>,
}

fn required_cover<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<ShareCoverDto>, D::Error> {
    Option::deserialize(d)
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ShareDto {
    #[serde(flatten)]
    pub config: ShareRequest,
    pub share_id: String,
    pub shared_at: u64,
    pub updated_at: u64,
    pub visible_canvas_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ShareResponse {
    pub project_id: String,
    pub can_toggle_share: bool,
    pub auto_share_suppressed: bool,
    pub suppression_reason: Option<String>,
    pub is_shared: bool,
    pub is_visible: bool,
    pub unavailable_reasons: Vec<String>,
    pub share: Option<ShareDto>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GalleryItem {
    pub project_id: String,
    pub title: String,
    pub description: String,
    pub author_name: String,
    pub cover_url: Option<String>,
    pub shared_at: u64,
    pub updated_at: u64,
    pub canvas_count: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GalleryCanvas {
    pub id: String,
    pub name: String,
    pub sort_order: i64,
    pub manifest_url: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GalleryDetail {
    #[serde(flatten)]
    pub item: GalleryItem,
    pub canvases: Vec<GalleryCanvas>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GalleryPage {
    pub items: Vec<GalleryItem>,
    pub next_cursor: Option<String>,
    /// 还有索引候选项，不保证下一页有可展示作品。
    pub has_more: bool,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct GalleryQuery {
    #[param(minimum = 1, maximum = 100, default = 20)]
    pub limit: Option<usize>,
    /// 不透明游标；当前格式为 v1_ 加规范共享 ULID。
    pub cursor: Option<String>,
}

fn parse_canvas(raw: &str) -> AppResult<CanvasId> {
    let id = CanvasId::parse(raw).map_err(|_| AppError::BadRequest("画布 ID 非法".into()))?;
    if !id.to_text().eq_ignore_ascii_case(raw) {
        return Err(AppError::BadRequest("画布 ID 非法".into()));
    }
    Ok(id)
}

/// 在 operation_lock 内重新读取身份，管理员降权或账户禁用立即生效。
async fn authorize(
    state: &AppState,
    caller: &UserCaller,
    raw: &str,
    write: bool,
    action: &str,
) -> AppResult<(Uid, ProjectId)> {
    let user = identity::find_by_uid(&state.store, caller.uid)
        .await?
        .filter(|u| !u.disabled)
        .ok_or(AppError::Unauthorized)?;
    let current = Caller::User {
        uid: caller.uid,
        role: user.role,
    };
    let (uid, pid, _) = project_api::authorize(state, &current, raw, write).await?;
    if uid != caller.uid {
        auth::audit_admin_action(caller.uid, action, uid, raw);
    }
    Ok((uid, pid))
}

async fn share_response(
    state: &AppState,
    actor: Uid,
    owner: Uid,
    pid: ProjectId,
) -> AppResult<ShareResponse> {
    let ctrl = gallery_policy::control(&state.store, pid).await?;
    let admin = gallery_policy::user(&state.store, actor)
        .await?
        .role
        .is_admin();
    let can_toggle_share = admin
        || (actor == owner
            && gallery_policy::can_customize(&state.store, actor).await?
            && ctrl.suppression != gallery_policy::Suppression::Admin);
    let suppression_reason = match ctrl.suppression {
        gallery_policy::Suppression::None => None,
        gallery_policy::Suppression::Owner => Some("owner".to_owned()),
        gallery_policy::Suppression::Admin => Some("admin".to_owned()),
    };
    let auto_share_suppressed = suppression_reason.is_some();
    let Some(share) = gallery::get(&state.store, pid).await? else {
        return Ok(ShareResponse {
            project_id: pid.to_text(),
            can_toggle_share,
            auto_share_suppressed,
            suppression_reason,
            is_shared: false,
            is_visible: false,
            unavailable_reasons: vec![],
            share: None,
        });
    };
    let view = gallery::inspect(&state.store, &share).await?;
    Ok(ShareResponse {
        project_id: pid.to_text(),
        can_toggle_share,
        auto_share_suppressed,
        suppression_reason,
        is_shared: true,
        is_visible: !view.canvases.is_empty(),
        unavailable_reasons: view.reasons,
        share: Some(ShareDto {
            config: ShareRequest {
                title: share.config.title,
                description: share.config.description,
                author_name: share.config.author_name,
                canvas_ids: share
                    .config
                    .canvas_ids
                    .iter()
                    .map(|id| id.to_text())
                    .collect(),
                cover: share.config.cover.map(|c| ShareCoverDto {
                    canvas_id: c.canvas_id.to_text(),
                    path: c.path,
                }),
            },
            share_id: share.share_id.to_text(),
            shared_at: share.shared_at,
            updated_at: share.updated_at,
            visible_canvas_ids: view.canvases.iter().map(|(id, _)| id.to_text()).collect(),
        }),
    })
}

fn public_item(share: &ProjectShare, view: &gallery::Presentation) -> GalleryItem {
    GalleryItem {
        project_id: share.project_id.to_text(),
        title: share.config.title.clone(),
        description: share.config.description.clone(),
        author_name: share.config.author_name.clone(),
        cover_url: view.cover_url.clone(),
        shared_at: share.shared_at,
        updated_at: share.updated_at,
        canvas_count: view.canvases.len(),
    }
}

#[utoipa::path(get, path="/projects/{id}/share", tag="gallery", summary="所有者或管理员查询共享配置", params(("id"=String, Path)), security(("bearer_jwt"=[])), responses((status=200,body=ShareResponse),(status=401),(status=403),(status=404)))]
async fn get_share(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(raw): Path<String>,
) -> AppResult<Json<ShareResponse>> {
    let _guard = state.operation_lock.lock().await;
    let (uid, pid) = authorize(&state, &caller, &raw, false, "gallery_share_read").await?;
    Ok(Json(share_response(&state, caller.uid, uid, pid).await?))
}

#[utoipa::path(put, path="/projects/{id}/share", tag="gallery", summary="Pro、Max 所有者或管理员共享项目或替换展示配置", params(("id"=String, Path)), request_body=ShareRequest, security(("bearer_jwt"=[])), responses((status=201,body=ShareResponse),(status=200,body=ShareResponse),(status=400),(status=401),(status=403),(status=404),(status=409),(status=413)))]
async fn put_share(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(raw): Path<String>,
    body: Result<Json<ShareRequest>, JsonRejection>,
) -> AppResult<(StatusCode, Json<ShareResponse>)> {
    let Json(body) = body.map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge
        } else {
            AppError::BadRequest("共享配置格式非法或缺少必填字段".into())
        }
    })?;
    let _guard = state.operation_lock.lock().await;
    let (uid, pid) = authorize(&state, &caller, &raw, true, "gallery_share_put").await?;
    gallery_policy::authorize_change(&state.store, caller.uid, uid, pid)
        .await
        .map_err(gallery_error)?;
    let config = ShareConfig {
        title: body.title,
        description: body.description,
        author_name: body.author_name,
        canvas_ids: body
            .canvas_ids
            .iter()
            .map(|s| parse_canvas(s))
            .collect::<AppResult<_>>()?,
        cover: body
            .cover
            .map(|c| {
                Ok::<_, AppError>(ShareCover {
                    canvas_id: parse_canvas(&c.canvas_id)?,
                    path: c.path,
                })
            })
            .transpose()?,
    };
    let mut batch = Batch::new();
    let (_, created) = gallery::stage_put(
        &state.store,
        &mut batch,
        uid,
        pid,
        config,
        util::now_secs(),
        &std::collections::HashMap::new(),
    )
    .await
    .map_err(|e| gallery_error(e.into()))?;
    gallery_policy::stage_clear(&state.store, &mut batch, pid, util::now_secs()).await?;
    state.store.write(batch).await?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(share_response(&state, caller.uid, uid, pid).await?),
    ))
}

#[utoipa::path(delete, path="/projects/{id}/share", tag="gallery", summary="Pro、Max 所有者或管理员取消共享并阻止自动发布", params(("id"=String, Path)), security(("bearer_jwt"=[])), responses((status=204),(status=401),(status=403),(status=404),(status=409)))]
async fn delete_share(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(raw): Path<String>,
) -> AppResult<StatusCode> {
    let _guard = state.operation_lock.lock().await;
    let (uid, pid) = authorize(&state, &caller, &raw, false, "gallery_share_delete").await?;
    gallery_policy::toggle(&state.store, caller.uid, uid, pid, false, util::now_secs())
        .await
        .map_err(gallery_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path="/gallery/projects/{project_id}", tag="gallery", summary="匿名读取共享作品详情", params(("project_id"=String, Path)), responses((status=200,body=GalleryDetail),(status=404)))]
async fn get_gallery(
    State(state): State<AppState>,
    Path(raw): Path<String>,
) -> AppResult<Json<GalleryDetail>> {
    let pid = ProjectId::parse(&raw).map_err(|_| AppError::NotFound)?;
    let _guard = state.operation_lock.lock().await;
    let share = gallery::get(&state.store, pid)
        .await?
        .ok_or(AppError::NotFound)?;
    let view = gallery::inspect(&state.store, &share).await?;
    if view.canvases.is_empty() {
        return Err(AppError::NotFound);
    }
    Ok(Json(GalleryDetail {
        item: public_item(&share, &view),
        canvases: view
            .canvases
            .into_iter()
            .map(|(id, c)| GalleryCanvas {
                id: id.to_text(),
                name: c.name,
                sort_order: c.sort_order,
                manifest_url: format!("/f/{}/manifest.json", c.slug),
            })
            .collect(),
    }))
}

#[utoipa::path(get, path="/gallery/projects", tag="gallery", summary="匿名分页浏览共享作品", params(GalleryQuery), responses((status=200,body=GalleryPage),(status=400)))]
async fn list_gallery(
    State(state): State<AppState>,
    query: Result<Query<GalleryQuery>, QueryRejection>,
) -> AppResult<Json<GalleryPage>> {
    let Query(query) = query.map_err(|_| AppError::BadRequest("分页参数非法".into()))?;
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(AppError::BadRequest("limit 必须为 1 到 100".into()));
    }
    let mut position = query
        .cursor
        .as_deref()
        .map(|s| {
            let raw = s
                .strip_prefix("v1_")
                .ok_or_else(|| AppError::BadRequest("cursor 非法".into()))?;
            let id = ShareId::parse(raw).map_err(|_| AppError::BadRequest("cursor 非法".into()))?;
            if id.to_text() != raw {
                return Err(AppError::BadRequest("cursor 非法".into()));
            }
            Ok(id)
        })
        .transpose()?;
    let mut items = Vec::new();
    let mut examined = 0;
    while examined < 1000 && items.len() < limit {
        let rows = state
            .store
            .scan_prefix_page(
                Cf::Idx,
                keys::gallery_feed_prefix(),
                position.map(keys::gallery_feed_key),
                true,
                64.min(1000 - examined),
            )
            .await?;
        if rows.is_empty() {
            break;
        }
        for (key, raw) in rows {
            let sid = keys::parse_gallery_feed_key(&key)?;
            position = Some(sid);
            examined += 1;
            let target: GalleryTarget = records::decode(&raw)?;
            {
                let _guard = state.operation_lock.lock().await;
                if let Some(share) = gallery::get(&state.store, target.project_id).await?
                    && share.share_id == sid
                    && share.owner_uid == target.owner_uid
                {
                    let view = gallery::inspect(&state.store, &share).await?;
                    if !view.canvases.is_empty() {
                        items.push(public_item(&share, &view));
                    }
                }
            }
            if items.len() == limit {
                break;
            }
        }
    }
    // 空库首屏没有扫描位置；即使此刻出现新共享，也留给刷新读取，避免返回无游标的 has_more。
    let has_more = position.is_some()
        && !state
            .store
            .scan_prefix_page(
                Cf::Idx,
                keys::gallery_feed_prefix(),
                position.map(keys::gallery_feed_key),
                true,
                1,
            )
            .await?
            .is_empty();
    Ok(Json(GalleryPage {
        items,
        has_more,
        next_cursor: position.filter(|_| has_more).map(|id| format!("v1_{id}")),
    }))
}

fn strict_json<T>(body: Result<Json<T>, JsonRejection>) -> AppResult<T> {
    body.map(|Json(v)| v).map_err(|e| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::PayloadTooLarge
        } else {
            AppError::BadRequest("请求格式非法或缺少必填字段".into())
        }
    })
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SharingToggle {
    pub enabled: bool,
}

#[utoipa::path(patch, path="/projects/{id}/sharing", tag="gallery", summary="Pro、Max 或管理员开关作品共享", params(("id"=String, Path)), request_body=SharingToggle, security(("bearer_jwt"=[])), responses((status=200,body=ShareResponse),(status=400),(status=401),(status=403),(status=404),(status=409),(status=413)))]
async fn toggle_sharing(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(raw): Path<String>,
    body: Result<Json<SharingToggle>, JsonRejection>,
) -> AppResult<Json<ShareResponse>> {
    let body = strict_json(body)?;
    let _guard = state.operation_lock.lock().await;
    let (uid, pid) = authorize(
        &state,
        &caller,
        &raw,
        body.enabled,
        "gallery_sharing_toggle",
    )
    .await?;
    gallery_policy::toggle(
        &state.store,
        caller.uid,
        uid,
        pid,
        body.enabled,
        util::now_secs(),
    )
    .await
    .map_err(gallery_error)?;
    Ok(Json(share_response(&state, caller.uid, uid, pid).await?))
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GalleryPreferenceRequest {
    pub auto_share_gallery: bool,
}
#[derive(Serialize, ToSchema)]
pub struct GalleryPreferenceResponse {
    pub auto_share_gallery: bool,
    pub can_disable_auto_share_gallery: bool,
}
#[utoipa::path(patch, path="/me/gallery-preferences", tag="gallery", summary="修改今后生成作品的自动共享偏好", request_body=GalleryPreferenceRequest, security(("bearer_jwt"=[])), responses((status=200,body=GalleryPreferenceResponse),(status=400),(status=401),(status=403),(status=413)))]
async fn update_preferences(
    State(state): State<AppState>,
    caller: UserCaller,
    body: Result<Json<GalleryPreferenceRequest>, JsonRejection>,
) -> AppResult<Json<GalleryPreferenceResponse>> {
    let body = strict_json(body)?;
    let _guard = state.operation_lock.lock().await;
    identity::find_by_uid(&state.store, caller.uid)
        .await?
        .filter(|u| !u.disabled)
        .ok_or(AppError::Unauthorized)?;
    gallery_policy::set_preference(
        &state.store,
        caller.uid,
        body.auto_share_gallery,
        util::now_secs(),
    )
    .await
    .map_err(gallery_error)?;
    let (auto_share_gallery, can_disable_auto_share_gallery) =
        gallery_policy::preferences(&state.store, caller.uid).await?;
    Ok(Json(GalleryPreferenceResponse {
        auto_share_gallery,
        can_disable_auto_share_gallery,
    }))
}
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CompletionRequest {
    pub generation_id: String,
    /// 必须来自队列响应外层 user_id；服务端校验该用户确为项目所有者。
    pub task_user_id: String,
    pub project_id: String,
    pub canvas_id: String,
    pub entry: String,
    /// 入口文件完整 BLAKE3 十六进制哈希。
    pub entry_hash: String,
    pub cover_path: Option<String>,
}
#[derive(Serialize, ToSchema)]
pub struct CompletionResponse {
    pub already_processed: bool,
    /// 第一次处理时的历史结果，并不代表现在仍然共享。
    pub auto_share_applied: bool,
    pub current_is_shared: bool,
    pub current_is_visible: bool,
}
#[utoipa::path(post, path="/internal/generation-completions", tag="gallery", summary="发布名单内 Worker 提交已验收的生成完成通知", request_body=CompletionRequest, security(("bearer_jwt"=[])), responses((status=200,body=CompletionResponse),(status=400),(status=401),(status=403),(status=404),(status=409),(status=413)))]
async fn generation_completion(
    State(state): State<AppState>,
    caller: UserCaller,
    body: Result<Json<CompletionRequest>, JsonRejection>,
) -> AppResult<Json<CompletionResponse>> {
    let body = strict_json(body)?;
    let _guard = state.operation_lock.lock().await;
    let worker = identity::find_by_uid(&state.store, caller.uid)
        .await?
        .filter(|u| !u.disabled)
        .ok_or(AppError::Unauthorized)?;
    if !state
        .config
        .gallery
        .publisher_usernames
        .contains(&worker.username)
    {
        tracing::warn!(worker_uid=%caller.uid, worker_username=%worker.username,
            reason="gallery_publisher_required", "生成完成通知被拒绝：账号未在实际生效的发布名单中");
        return Err(AppError::GalleryPublisherRequired);
    }
    let invalid = |_| AppError::BadRequest("ID 非法".into());
    let generation_id = keys::GenerationId::parse(&body.generation_id).map_err(invalid)?;
    let owner = Uid::parse(&body.task_user_id).map_err(invalid)?;
    let pid = ProjectId::parse(&body.project_id).map_err(invalid)?;
    let cid = parse_canvas(&body.canvas_id)?;
    keys::validate_path(&body.entry).map_err(|_| AppError::BadRequest("入口路径非法".into()))?;
    if body.entry_hash.len() != 64 || !body.entry_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::BadRequest(
            "entry_hash 必须是 64 位十六进制哈希".into(),
        ));
    }
    let request = gallery_policy::Completion {
        generation_id,
        task_user_id: owner,
        project_id: pid,
        canvas_id: cid,
        entry: body.entry,
        entry_hash: body.entry_hash.to_ascii_lowercase(),
        cover_path: body.cover_path,
    };
    let (already_processed, auto_share_applied) =
        gallery_policy::complete(&state.store, request, util::now_secs())
            .await
            .map_err(gallery_error)?;
    let share = gallery::get(&state.store, pid).await?;
    let current_is_shared = share.is_some();
    let current_is_visible = match share {
        Some(s) => !gallery::inspect(&state.store, &s)
            .await?
            .canvases
            .is_empty(),
        None => false,
    };
    tracing::info!(worker_uid=%caller.uid, owner_uid=%owner, project_id=%pid, canvas_id=%cid, %generation_id, already_processed, auto_share_applied, "生成完成通知已处理");
    Ok(Json(CompletionResponse {
        already_processed,
        auto_share_applied,
        current_is_shared,
        current_is_visible,
    }))
}
