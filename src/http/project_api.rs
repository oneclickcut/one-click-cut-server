//! 项目、制作画布与公共资料入口。
use super::{
    AppState,
    auth::{Caller, UserCaller},
    error::{AppError, AppResult},
};
use crate::{
    domain::{canvas, space},
    storage::{
        Cf,
        keys::{self, CanvasId, ParentId, ProjectId, Uid},
        records::{self, CanvasRole, ProjectStatus},
    },
    util,
};
use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_project, list_projects))
        .routes(routes!(get_project, update_project, delete_project))
        .routes(routes!(list_canvases, create_canvas))
        .routes(routes!(get_context))
}
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    pub name: String,
    pub parent_id: Option<String>,
}
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    pub parent_id: Option<String>,
    pub resources_name: Option<String>,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct CanvasDto {
    pub id: String,
    pub project_id: String,
    pub name: String,
    pub slug: String,
    pub base_url: String,
    pub role: String,
    pub sort_order: i64,
    pub file_count: u64,
    pub bytes: u64,
    pub updated_at: u64,
}
fn canvas_dto(id: CanvasId, c: &records::Canvas) -> CanvasDto {
    CanvasDto {
        id: id.to_text(),
        project_id: c.project_id.to_text(),
        name: c.name.clone(),
        slug: c.slug.clone(),
        base_url: canvas::base_url(&c.slug),
        role: match c.role {
            CanvasRole::Production => "production",
            CanvasRole::Resources => "resources",
        }
        .into(),
        sort_order: c.sort_order,
        file_count: c.file_count,
        bytes: c.bytes,
        updated_at: c.updated_at,
    }
}
#[derive(Debug, Serialize, ToSchema)]
pub struct ProjectDto {
    pub is_shared: bool,
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub status: String,
    pub resources: Option<CanvasDto>,
    pub canvases: Vec<CanvasDto>,
    pub production_canvas_count: usize,
    pub logical_bytes: u64,
    pub created_at: u64,
    pub updated_at: u64,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct ContextDto {
    #[serde(flatten)]
    pub project: ProjectDto,
    /// 经授权校验取得的项目所有者 UID；Worker 与队列外层 user_id 比较。
    pub owner_uid: String,
    pub permissions: super::agent_api::AgentPermsDto,
}
async fn project_dto(
    state: &AppState,
    uid: Uid,
    id: ProjectId,
    p: &records::Project,
) -> AppResult<ProjectDto> {
    let mut resources = None;
    let mut canvases = Vec::new();
    for (cid, c) in space::list_project_canvases(&state.store, uid, id).await? {
        if c.role == CanvasRole::Resources {
            if cid != p.resources_canvas_id || resources.is_some() {
                return Err(AppError::Internal(anyhow::anyhow!("项目公共区引用不一致")));
            }
            resources = Some(canvas_dto(cid, &c))
        } else {
            canvases.push(canvas_dto(cid, &c))
        }
    }
    if resources.is_none() && p.status != ProjectStatus::Deleting {
        return Err(AppError::Internal(anyhow::anyhow!("项目缺少公共区")));
    }
    let logical_bytes = canvases
        .iter()
        .fold(resources.as_ref().map_or(0, |r| r.bytes), |sum, c| {
            sum.saturating_add(c.bytes)
        });
    Ok(ProjectDto {
        is_shared: crate::domain::gallery::get(&state.store, id)
            .await?
            .is_some(),
        id: id.to_text(),
        name: p.name.clone(),
        parent_id: p.parent_id.map(|id| id.to_text()),
        status: match p.status {
            ProjectStatus::Active => "active",
            ProjectStatus::Deleting => "deleting",
        }
        .into(),
        resources,
        production_canvas_count: canvases.len(),
        logical_bytes,
        canvases,
        created_at: p.created_at,
        updated_at: p.updated_at,
    })
}
fn request_key(headers: &HeaderMap) -> AppResult<Option<&str>> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map_err(|_| AppError::BadRequest("幂等标识非法".into()))?;
    if key.is_empty()
        || key.len() > 128
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(AppError::BadRequest(
            "幂等标识只允许字母、数字、横线和下划线，最长 128 字节".into(),
        ));
    }
    Ok(Some(key))
}
fn parent(raw: Option<&str>) -> AppResult<ParentId> {
    match raw {
        None | Some("") => Ok(ParentId::Root),
        Some(s) => Ok(ParentId::Folder(
            keys::FolderId::parse(s).map_err(|_| AppError::NotFound)?,
        )),
    }
}
async fn owner(state: &AppState, caller: &Caller, id: ProjectId) -> AppResult<Uid> {
    if let Caller::Agent { project_id, .. } = caller {
        if *project_id != id {
            return Err(AppError::NotFound);
        }
    }
    let uid = caller.uid();
    if space::get_project(&state.store, uid, id).await?.is_some() {
        return Ok(uid);
    }
    if caller.is_admin() {
        for (key, _) in state
            .store
            .scan_prefix(Cf::Meta, keys::project_prefix())
            .await?
        {
            let (uid, pid) = keys::parse_project_key(&key).map_err(AppError::Internal)?;
            if pid == id {
                super::auth::audit_admin_action(caller.uid(), "project_access", uid, &id.to_text());
                return Ok(uid);
            }
        }
    }
    Err(AppError::NotFound)
}
pub async fn authorize(
    state: &AppState,
    caller: &Caller,
    raw: &str,
    write: bool,
) -> AppResult<(Uid, ProjectId, records::Project)> {
    caller.ensure_current(state).await?;
    let id = ProjectId::parse(raw).map_err(|_| AppError::NotFound)?;
    let uid = owner(state, caller, id).await?;
    let p = space::get_project(&state.store, uid, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if write && p.status != ProjectStatus::Active {
        return Err(AppError::Conflict("项目正在删除".into()));
    }
    if write {
        let hash = match caller {
            Caller::Agent { token_hash, .. } => Some(token_hash.to_hex()),
            _ => None,
        };
        crate::domain::jobs::authorize_write(
            &state.store,
            id,
            hash.as_deref(),
            crate::util::now_secs(),
        )
        .await
        .map_err(|e| AppError::Conflict(e.to_string()))?;
    }
    Ok((uid, id, p))
}
#[utoipa::path(summary="创建项目并初始化公共资料和默认画布",post,path="/projects",tag="projects",request_body=CreateProjectRequest,security(("bearer_jwt"=[])),responses((status=201,body=ProjectDto)))]
async fn create_project(
    State(state): State<AppState>,
    caller: UserCaller,
    headers: HeaderMap,
    Json(body): Json<CreateProjectRequest>,
) -> AppResult<(StatusCode, Json<ProjectDto>)> {
    let _guard = state.operation_lock.lock().await;
    let request = request_key(&headers)?;
    if let Some(key) = request {
        if let Some(raw) = state
            .store
            .get(Cf::Idx, keys::project_request_key(caller.uid, key))
            .await?
        {
            let id = ProjectId::parse(std::str::from_utf8(&raw).map_err(|_| AppError::NotFound)?)
                .map_err(|_| AppError::NotFound)?;
            let p = space::get_project(&state.store, caller.uid, id)
                .await?
                .ok_or(AppError::NotFound)?;
            return Ok((
                StatusCode::OK,
                Json(project_dto(&state, caller.uid, id, &p).await?),
            ));
        }
    }
    let (id, p, _, _) = space::create_project(
        &state.store,
        caller.uid,
        parent(body.parent_id.as_deref())?,
        &body.name,
        request,
        util::now_secs(),
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?
    .map_err(super::tree_api::map_space_error)?;
    Ok((
        StatusCode::CREATED,
        Json(project_dto(&state, caller.uid, id, &p).await?),
    ))
}
#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectOrder {
    Asc,
    #[default]
    Desc,
}

#[derive(Debug, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct ListProjectsQuery {
    /// 每页数量，默认 20，范围 1–100。
    #[param(minimum = 1, maximum = 100, default = 20)]
    pub limit: Option<usize>,
    /// 上一页的 next_cursor（项目 ULID），不包含游标项目本身。
    pub cursor: Option<String>,
    /// 按项目 ID 排序，默认 desc；翻页时保持相同方向。
    #[serde(default)]
    #[param(default = "desc")]
    pub order: ProjectOrder,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ListProjectsResponse {
    pub items: Vec<ProjectDto>,
    /// 没有下一页时为 null。
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[utoipa::path(summary="分页列出当前用户的项目（按项目 ID 排序）",get,path="/projects",tag="projects",params(ListProjectsQuery),security(("bearer_jwt"=[])),responses((status=200,body=ListProjectsResponse),(status=400,description="分页参数非法")))]
async fn list_projects(
    State(state): State<AppState>,
    caller: UserCaller,
    query: Result<Query<ListProjectsQuery>, QueryRejection>,
) -> AppResult<Json<ListProjectsResponse>> {
    let Query(query) = query.map_err(|_| AppError::BadRequest("分页参数非法".into()))?;
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(AppError::BadRequest("limit 必须为 1 到 100".into()));
    }
    let after = query
        .cursor
        .as_deref()
        .map(|raw| {
            ProjectId::parse(raw)
                // 拒绝超出 128 位范围等解析后发生截断/归一化的输入。
                .and_then(|id| {
                    anyhow::ensure!(id.to_text().eq_ignore_ascii_case(raw), "游标不是规范 ULID");
                    Ok(id)
                })
                .map(|id| keys::project_key(caller.uid, id))
                .map_err(|_| AppError::BadRequest("cursor 必须为合法的项目 ULID".into()))
        })
        .transpose()?;
    let rows = state
        .store
        .scan_prefix_page(
            Cf::Meta,
            keys::user_project_prefix(caller.uid),
            after,
            matches!(query.order, ProjectOrder::Desc),
            limit + 1,
        )
        .await?;
    let has_more = rows.len() > limit;
    let mut items = Vec::with_capacity(rows.len().min(limit));
    for (key, value) in rows.into_iter().take(limit) {
        let (uid, id) = keys::parse_project_key(&key).map_err(AppError::Internal)?;
        let p: records::Project = records::decode(&value).map_err(AppError::Internal)?;
        items.push(project_dto(&state, uid, id, &p).await?);
    }
    let next_cursor = if has_more {
        items.last().map(|p| p.id.clone())
    } else {
        None
    };
    Ok(Json(ListProjectsResponse {
        items,
        next_cursor,
        has_more,
    }))
}
#[utoipa::path(summary="获取项目详情",get,path="/projects/{id}",tag="projects",params(("id"=String,Path)),security(("bearer_jwt"=[])),responses((status=200,body=ProjectDto)))]
async fn get_project(
    State(state): State<AppState>,
    caller: Caller,
    Path(raw): Path<String>,
) -> AppResult<Json<ProjectDto>> {
    let (uid, id, p) = authorize(&state, &caller, &raw, false).await?;
    Ok(Json(project_dto(&state, uid, id, &p).await?))
}
/// Agent Token 仅允许修改绑定项目的 name，无需额外权限位。
#[utoipa::path(summary="修改项目名称、位置或公共资料名称",patch,path="/projects/{id}",tag="projects",params(("id"=String,Path)),request_body=UpdateProjectRequest,security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=200,body=ProjectDto),(status=403,description="Agent Token 仅允许修改 name")))]
async fn update_project(
    State(state): State<AppState>,
    caller: Caller,
    Path(raw): Path<String>,
    Json(body): Json<UpdateProjectRequest>,
) -> AppResult<Json<ProjectDto>> {
    let _guard = state.operation_lock.lock().await;
    let (target_uid, id, mut p) = authorize(&state, &caller, &raw, true).await?;
    if matches!(caller, Caller::Agent { .. })
        && (body.name.is_none() || body.parent_id.is_some() || body.resources_name.is_some())
    {
        return Err(AppError::Forbidden);
    }
    if let Some(name) = body.name {
        keys::validate_display_name(&name).map_err(|e| AppError::BadRequest(e.to_string()))?;
        p.name = name
    }
    let old_parent = ParentId::from(p.parent_id);
    if let Some(raw) = body.parent_id {
        let new_parent = parent(Some(&raw))?;
        if let ParentId::Folder(fid) = new_parent {
            if space::get_folder(&state.store, target_uid, fid)
                .await?
                .is_none()
            {
                return Err(AppError::NotFound);
            }
        }
        p.parent_id = match new_parent {
            ParentId::Root => None,
            ParentId::Folder(fid) => Some(fid),
        };
    }
    p.updated_at = util::now_secs();
    let mut batch = crate::storage::Batch::new();
    batch.put(
        Cf::Meta,
        keys::project_key(target_uid, id),
        records::encode(&p).map_err(AppError::Internal)?,
    );
    if old_parent != ParentId::from(p.parent_id) {
        batch
            .delete(
                Cf::Idx,
                keys::child_key(
                    target_uid,
                    old_parent,
                    keys::ChildKind::Project,
                    &id.to_text(),
                )
                .map_err(AppError::Internal)?,
            )
            .put(
                Cf::Idx,
                keys::child_key(
                    target_uid,
                    ParentId::from(p.parent_id),
                    keys::ChildKind::Project,
                    &id.to_text(),
                )
                .map_err(AppError::Internal)?,
                Vec::new(),
            );
    }
    if let Some(name) = body.resources_name {
        keys::validate_display_name(&name).map_err(|e| AppError::BadRequest(e.to_string()))?;
        let mut c = space::get_canvas(&state.store, target_uid, p.resources_canvas_id)
            .await?
            .ok_or(AppError::NotFound)?;
        c.name = name;
        c.updated_at = p.updated_at;
        batch.put(
            Cf::Meta,
            keys::canvas_key(target_uid, p.resources_canvas_id),
            records::encode(&c).map_err(AppError::Internal)?,
        );
    }
    state.store.write(batch).await?;
    Ok(Json(project_dto(&state, target_uid, id, &p).await?))
}
#[utoipa::path(summary="删除项目及其资源",delete,path="/projects/{id}",tag="projects",params(("id"=String,Path)),security(("bearer_jwt"=[])),responses((status=204)))]
async fn delete_project(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(raw): Path<String>,
) -> AppResult<StatusCode> {
    let _guard = state.operation_lock.lock().await;
    let id = ProjectId::parse(&raw).map_err(|_| AppError::NotFound)?;
    let identity = Caller::User {
        uid: caller.uid,
        role: caller.role,
    };
    let target_uid = owner(&state, &identity, id).await?;
    space::delete_project(&state.store, target_uid, id, util::now_secs())
        .await?
        .map_err(|_| AppError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}
#[utoipa::path(summary="列出项目的制作画布",get,path="/projects/{id}/canvases",tag="projects",params(("id"=String,Path)),security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=200,body=Vec<CanvasDto>)))]
async fn list_canvases(
    State(state): State<AppState>,
    caller: Caller,
    Path(raw): Path<String>,
) -> AppResult<Json<Vec<CanvasDto>>> {
    let (uid, id, _) = authorize(&state, &caller, &raw, false).await?;
    Ok(Json(
        space::list_project_canvases(&state.store, uid, id)
            .await?
            .into_iter()
            .filter(|(_, c)| c.role == CanvasRole::Production)
            .map(|(id, c)| canvas_dto(id, &c))
            .collect(),
    ))
}
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateCanvasRequest {
    pub name: String,
}
#[utoipa::path(summary="在项目中创建制作画布",post,path="/projects/{id}/canvases",tag="projects",params(("id"=String,Path)),request_body=CreateCanvasRequest,security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=201,body=CanvasDto)))]
async fn create_canvas(
    State(state): State<AppState>,
    caller: Caller,
    Path(raw): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateCanvasRequest>,
) -> AppResult<(StatusCode, Json<CanvasDto>)> {
    let _guard = state.operation_lock.lock().await;
    let (uid, id, _) = authorize(&state, &caller, &raw, true).await?;
    if let Caller::Agent { perms, .. } = caller {
        if !perms.canvas_create {
            return Err(AppError::Forbidden);
        }
    }
    let request = request_key(&headers)?;
    if let Some(key) = request {
        if let Some(raw) = state
            .store
            .get(Cf::Idx, keys::canvas_request_key(id, key))
            .await?
        {
            let cid = CanvasId::parse(std::str::from_utf8(&raw).map_err(|_| AppError::NotFound)?)
                .map_err(|_| AppError::NotFound)?;
            let c = space::get_canvas(&state.store, uid, cid)
                .await?
                .ok_or(AppError::NotFound)?;
            return Ok((StatusCode::OK, Json(canvas_dto(cid, &c))));
        }
    }
    let (cid, c) =
        space::create_canvas(&state.store, uid, id, &body.name, request, util::now_secs())
            .await
            .map_err(|e| AppError::BadRequest(e.to_string()))?
            .map_err(super::tree_api::map_space_error)?;
    Ok((StatusCode::CREATED, Json(canvas_dto(cid, &c))))
}
#[utoipa::path(summary="获取项目上下文和当前权限",get,path="/projects/{id}/context",tag="projects",params(("id"=String,Path)),security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=200,body=ContextDto)))]
async fn get_context(
    State(state): State<AppState>,
    caller: Caller,
    Path(raw): Path<String>,
) -> AppResult<Json<ContextDto>> {
    let (uid, id, p) = authorize(&state, &caller, &raw, false).await?;
    let permissions = match caller {
        Caller::Agent { perms, .. } => perms.into(),
        _ => super::agent_api::AgentPermsDto {
            canvas_files_write: Some(true),
            canvas_files_delete: Some(true),
            canvas_create: Some(true),
            constraints_write: Some(true),
            constraints_delete: Some(true),
        },
    };
    Ok(Json(ContextDto {
        project: project_dto(&state, uid, id, &p).await?,
        owner_uid: uid.to_text(),
        permissions,
    }))
}
