//! 文件夹与画布的树 CRUD。
//!
//! # 一条贯穿全部接口的规则
//!
//! **跨用户访问返回 404，不是 403。** 403 会告诉调用者「这个 id 确实存在，
//! 只是不属于你」，那就成了一个资源探测接口。管理员例外（可跨用户），
//! 但每次都会写审计日志。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::{self, Caller, UserCaller};
use super::error::{AppError, AppResult};
use crate::domain::identity;
use crate::domain::space::{self, Node, SpaceError};
use crate::storage::keys::{self, CanvasId, FolderId, ParentId, Uid};
use crate::storage::records::{Canvas, Folder, Visibility};
use crate::util;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_tree))
        .routes(routes!(create_folder))
        .routes(routes!(update_folder, delete_folder))
        .routes(routes!(move_folder))
        .routes(routes!(update_canvas, delete_canvas))
}

/// 把领域层的失败映射成 HTTP 状态码。
///
/// 集中在一处，保证同一种失败在所有接口上长得一样。
pub(crate) fn map_space_error(e: SpaceError) -> AppError {
    match e {
        SpaceError::Inactive => AppError::Conflict("项目正在删除".into()),
        SpaceError::TooManyCanvases => AppError::Conflict("单项目最多 100 个制作画布".into()),
        // 不存在与不属于你合并成 404，见模块说明。
        SpaceError::NotFound => AppError::NotFound,
        SpaceError::ParentIsCanvas => {
            AppError::BadRequest("画布是叶子节点，不能在它下面创建子项".into())
        }
        SpaceError::WouldCycle => {
            AppError::BadRequest("不能把文件夹移动到它自己或它的子孙下".into())
        }
        SpaceError::NotEmpty => {
            AppError::Conflict("文件夹非空，需要 ?recursive=true 才能级联删除".into())
        }
    }
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

/// 可见性的对外表示。与 [`Visibility`] 分开，理由同 `RoleDto`：
/// 存储层不该依赖接口文档框架。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum VisibilityDto {
    Public,
    Private,
}

impl From<Visibility> for VisibilityDto {
    fn from(v: Visibility) -> Self {
        match v {
            Visibility::Public => Self::Public,
            Visibility::Private => Self::Private,
        }
    }
}

impl From<VisibilityDto> for Visibility {
    fn from(v: VisibilityDto) -> Self {
        match v {
            VisibilityDto::Public => Self::Public,
            VisibilityDto::Private => Self::Private,
        }
    }
}

/// 树上的一个节点。
#[derive(Debug, Serialize, ToSchema)]
pub struct NodeDto {
    /// `folder` 或 `canvas`。
    #[schema(example = "canvas")]
    pub kind: &'static str,
    #[schema(value_type = String, example = "01JBXY7Z3K8QW5N2M4P6R8T0V1")]
    pub id: String,
    #[schema(example = "夏日海边")]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<VisibilityDto>,
    /// 画布才有：静态 URL 里用的随机串，**改名不变**。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// 画布才有：文件数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<u64>,
    /// 画布才有：总字节数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub updated_at: u64,
}

impl NodeDto {
    fn folder(id: FolderId, f: &Folder) -> Self {
        Self {
            kind: "folder",
            id: id.to_text(),
            name: f.name.clone(),
            visibility: Some(f.visibility.into()),
            slug: None,
            file_count: None,
            bytes: None,
            updated_at: f.updated_at,
        }
    }

    fn canvas(id: CanvasId, c: &Canvas) -> Self {
        Self {
            kind: "canvas",
            id: id.to_text(),
            name: c.name.clone(),
            visibility: Some(c.visibility.into()),
            slug: Some(c.slug.clone()),
            file_count: Some(c.file_count),
            bytes: Some(c.bytes),
            updated_at: c.updated_at,
        }
    }

    fn from_node(node: &Node) -> Self {
        match node {
            Node::Folder { id, folder } => Self::folder(*id, folder),
            Node::Project { id, project } => Self {
                kind: "project",
                id: id.to_text(),
                name: project.name.clone(),
                visibility: None,
                slug: None,
                file_count: None,
                bytes: None,
                updated_at: project.updated_at,
            },
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TreeResponse {
    pub nodes: Vec<NodeDto>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct TreeQuery {
    /// 父文件夹 id，省略即根层。
    pub parent: Option<String>,
    /// 管理员专用：查看指定用户的空间。
    pub uid: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateNode {
    /// 父文件夹 id，省略即根层。
    #[schema(value_type = Option<String>)]
    pub parent_id: Option<String>,
    #[schema(example = "夏日海边")]
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateFolder {
    pub name: Option<String>,
    pub visibility: Option<VisibilityDto>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateCanvas {
    pub name: Option<String>,
    pub visibility: Option<VisibilityDto>,
    /// manifest 里的 `entry`，指向描述时间轴的 json。
    pub entry: Option<String>,
    pub sort_order: Option<i64>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct UpdateFolderQuery {
    /// 改可见性时是否显式下推给整棵子树。
    ///
    /// 默认 false：可见性**不继承**，画布自身的字段是唯一判据。
    #[serde(default)]
    pub apply_to_children: bool,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct DeleteFolderQuery {
    /// 非空文件夹必须显式带上这个参数才能级联删除。
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct MoveRequest {
    /// 新的父文件夹 id，省略即移到根层。
    #[schema(value_type = Option<String>)]
    pub new_parent_id: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CanvasCreated {
    #[schema(value_type = String)]
    pub id: String,
    /// 12 字符随机 base62，创建后永不变。
    #[schema(example = "k7Qm3xB9pLdR")]
    pub slug: String,
    /// 静态资源的基地址，播放器只需要这一个输入。
    #[schema(example = "/f/k7Qm3xB9pLdR/")]
    pub base_url: String,
    pub name: String,
    pub visibility: VisibilityDto,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteResponse {
    pub folders: u64,
    pub canvases: u64,
    pub files: u64,
    /// 连带清理掉的**画布内目录**记录数（不是被删的文件夹数，那是 `folders`）。
    pub dirs: u64,
}

// ---------------------------------------------------------------------------
// 所有者解析
// ---------------------------------------------------------------------------

/// 定位要操作的用户空间，并在管理员越权时记审计日志。
///
/// 普通用户传 `uid` 参数会被忽略（只能操作自己）；管理员传了才生效。
async fn resolve_owner(
    state: &AppState,
    caller: &UserCaller,
    requested: Option<&str>,
    action: &str,
) -> AppResult<Uid> {
    let owner = match requested {
        Some(text) if caller.is_admin() => {
            let uid = Uid::parse(text).map_err(|_| AppError::NotFound)?;
            if uid != caller.uid {
                auth::audit_admin_action(caller.uid, action, uid, "space");
            }
            uid
        }
        // 非管理员指定别人的 uid：当成不存在，不提示「你没权限」。
        Some(_) => return Err(AppError::NotFound),
        None => caller.uid,
    };

    // 存在性检查：uid 来自 JWT 或管理员入参，账户可能已被删。
    if identity::find_by_uid(&state.store, owner).await?.is_none() {
        return Err(AppError::NotFound);
    }
    Ok(owner)
}

/// 解析可选的父节点 id。
fn parse_parent(raw: Option<&str>) -> AppResult<ParentId> {
    match raw {
        None => Ok(ParentId::Root),
        // `?parent=` 这种空值也算根层，前端拼 query 时容易出现。
        Some("") => Ok(ParentId::Root),
        Some(s) => {
            let id = FolderId::parse(s)
                .map_err(|_| AppError::BadRequest("父节点 id 不是合法 ULID".into()))?;
            Ok(ParentId::Folder(id))
        }
    }
}

fn parse_folder_id(raw: &str) -> AppResult<FolderId> {
    FolderId::parse(raw).map_err(|_| AppError::NotFound)
}

fn parse_canvas_id(raw: &str) -> AppResult<CanvasId> {
    CanvasId::parse(raw).map_err(|_| AppError::NotFound)
}

async fn canvas_owner(
    state: &AppState,
    caller: &UserCaller,
    id: CanvasId,
    action: &str,
) -> AppResult<Uid> {
    if space::get_canvas(&state.store, caller.uid, id)
        .await?
        .is_some()
    {
        return Ok(caller.uid);
    }
    if caller.is_admin() {
        if let Some(owner) = super::file_api::find_canvas_owner_pub(state, id).await? {
            auth::audit_admin_action(caller.uid, action, owner, &id.to_text());
            return Ok(owner);
        }
    }
    Err(AppError::NotFound)
}

// ---------------------------------------------------------------------------
// 接口
// ---------------------------------------------------------------------------

/// 列出某一层的子文件夹与画布
///
/// `parent` 省略即根层。管理员可以带 `uid` 查看任意用户的空间。
#[utoipa::path(
    summary = "列出用户空间中的文件夹和项目",
    get,
    path = "/tree",
    tag = "tree",
    params(TreeQuery),
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "该层的子节点", body = TreeResponse),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
        (status = 404, description = "父节点不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
async fn list_tree(
    State(state): State<AppState>,
    caller: UserCaller,
    Query(q): Query<TreeQuery>,
) -> AppResult<Json<TreeResponse>> {
    let owner = resolve_owner(&state, &caller, q.uid.as_deref(), "list_tree").await?;
    let parent = parse_parent(q.parent.as_deref())?;

    // 指定了父节点就要确认它存在，否则「不存在的父节点」会静默返回空列表，
    // 让前端以为是个空文件夹。
    if let ParentId::Folder(id) = parent
        && space::get_folder(&state.store, owner, id).await?.is_none()
    {
        return Err(AppError::NotFound);
    }

    let nodes = space::list_children(&state.store, owner, parent, usize::MAX).await?;
    Ok(Json(TreeResponse {
        nodes: nodes.iter().map(NodeDto::from_node).collect(),
    }))
}

/// 建文件夹
#[utoipa::path(
    summary = "创建文件夹",
    post,
    path = "/folders",
    tag = "tree",
    request_body = CreateNode,
    security(("bearer_jwt" = [])),
    responses(
        (status = 201, description = "已创建", body = NodeDto),
        (status = 400, description = "名称不合规，或父节点是画布", body = super::error::ErrorBody),
        (status = 404, description = "父节点不存在", body = super::error::ErrorBody),
    ),
)]
async fn create_folder(
    State(state): State<AppState>,
    caller: UserCaller,
    Json(body): Json<CreateNode>,
) -> AppResult<(StatusCode, Json<NodeDto>)> {
    let _guard = state.operation_lock.lock().await;
    let owner = resolve_owner(&state, &caller, None, "create_folder").await?;
    let parent = parse_parent(body.parent_id.as_deref())?;

    let created = space::create_folder(&state.store, owner, parent, &body.name, util::now_secs())
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?;

    match created {
        Ok((id, folder)) => Ok((StatusCode::CREATED, Json(NodeDto::folder(id, &folder)))),
        Err(e) => Err(map_space_error(e)),
    }
}

/// 改文件夹
///
/// 改名是 O(1)：只动记录，不动索引，也不会引发子树重写。
#[utoipa::path(
    summary = "修改文件夹",
    patch,
    path = "/folders/{id}",
    tag = "tree",
    params(("id" = String, Path, description = "文件夹 id"), UpdateFolderQuery),
    request_body = UpdateFolder,
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "已更新", body = NodeDto),
        (status = 404, description = "不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
async fn update_folder(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(id): Path<String>,
    Query(q): Query<UpdateFolderQuery>,
    Json(body): Json<UpdateFolder>,
) -> AppResult<Json<NodeDto>> {
    let _guard = state.operation_lock.lock().await;
    let owner = resolve_owner(&state, &caller, None, "update_folder").await?;
    let id = parse_folder_id(&id)?;

    if let Some(name) = &body.name {
        keys::validate_display_name(name).map_err(|e| AppError::BadRequest(e.to_string()))?;
    }
    let updated = space::update_folder(
        &state.store,
        caller.uid,
        owner,
        id,
        body.name.as_deref(),
        body.visibility.map(Into::into),
        q.apply_to_children,
        util::now_secs(),
    )
    .await
    .map_err(super::error::gallery_error)?;

    match updated {
        Ok(folder) => Ok(Json(NodeDto::folder(id, &folder))),
        Err(e) => Err(map_space_error(e)),
    }
}

/// 删文件夹
///
/// 非空时必须带 `?recursive=true`，否则 409。
#[utoipa::path(
    summary = "删除文件夹",
    delete,
    path = "/folders/{id}",
    tag = "tree",
    params(("id" = String, Path, description = "文件夹 id"), DeleteFolderQuery),
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "已删除，返回影响规模", body = DeleteResponse),
        (status = 404, description = "不存在或不属于调用者", body = super::error::ErrorBody),
        (status = 409, description = "非空且未带 recursive", body = super::error::ErrorBody),
    ),
)]
async fn delete_folder(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(id): Path<String>,
    Query(q): Query<DeleteFolderQuery>,
) -> AppResult<Json<DeleteResponse>> {
    let _guard = state.operation_lock.lock().await;
    let owner = resolve_owner(&state, &caller, None, "delete_folder").await?;
    let id = parse_folder_id(&id)?;

    match space::delete_folder(&state.store, owner, id, q.recursive, util::now_secs()).await? {
        Ok(r) => Ok(Json(DeleteResponse {
            folders: r.folders,
            canvases: r.canvases,
            files: r.files,
            dirs: r.dirs,
        })),
        Err(e) => Err(map_space_error(e)),
    }
}

/// 移动文件夹
#[utoipa::path(
    summary = "移动文件夹",
    post,
    path = "/folders/{id}/move",
    tag = "tree",
    params(("id" = String, Path, description = "文件夹 id")),
    request_body = MoveRequest,
    security(("bearer_jwt" = [])),
    responses(
        (status = 204, description = "已移动"),
        (status = 400, description = "会形成环，或目标是画布", body = super::error::ErrorBody),
        (status = 404, description = "不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
async fn move_folder(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(id): Path<String>,
    Json(body): Json<MoveRequest>,
) -> AppResult<StatusCode> {
    let _guard = state.operation_lock.lock().await;
    let owner = resolve_owner(&state, &caller, None, "move_folder").await?;
    let id = parse_folder_id(&id)?;
    let new_parent = parse_parent(body.new_parent_id.as_deref())?;

    match space::move_folder(&state.store, owner, id, new_parent, util::now_secs()).await? {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(map_space_error(e)),
    }
}

/// 改画布
///
/// **改名不影响 URL**：静态路径用的是 slug，这里碰不到它。
/// Agent Token 仅允许修改绑定项目内制作画布的 name，无需额外权限位。
#[utoipa::path(
    summary = "修改制作画布",
    patch,
    path = "/canvases/{id}",
    tag = "tree",
    params(("id" = String, Path, description = "画布 id")),
    request_body = UpdateCanvas,
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "已更新", body = NodeDto),
        (status = 403, description = "Agent Token 仅允许修改 name", body = super::error::ErrorBody),
        (status = 404, description = "不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
async fn update_canvas(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<UpdateCanvas>,
) -> AppResult<Json<NodeDto>> {
    let _guard = state.operation_lock.lock().await;
    let id = parse_canvas_id(&id)?;
    let owner = match &caller {
        Caller::Agent { .. } => {
            // 复用项目归属和 Token 有效性校验；domain 层继续检查制作画布和项目状态。
            let (owner, _) = super::file_api::authorize_canvas(
                &state,
                &caller,
                &id.to_text(),
                super::file_api::Need::Read,
            )
            .await?;
            if body.name.is_none()
                || body.visibility.is_some()
                || body.entry.is_some()
                || body.sort_order.is_some()
            {
                return Err(AppError::Forbidden);
            }
            owner
        }
        Caller::User { uid, role } => {
            let user = UserCaller {
                uid: *uid,
                role: *role,
            };
            canvas_owner(&state, &user, id, "update_canvas").await?
        }
    };

    if let Some(name) = &body.name {
        keys::validate_display_name(name).map_err(|e| AppError::BadRequest(e.to_string()))?;
    }
    if let Some(entry) = &body.entry {
        keys::validate_path(entry).map_err(|e| AppError::BadRequest(e.to_string()))?;
    }
    let updated = space::update_canvas(
        &state.store,
        match &caller {
            Caller::User { uid, .. } => *uid,
            Caller::Agent { .. } => owner,
        },
        owner,
        id,
        body.name.as_deref(),
        body.visibility.map(Into::into),
        body.entry.as_deref(),
        body.sort_order,
        util::now_secs(),
    )
    .await
    .map_err(super::error::gallery_error)?;

    match updated {
        Ok(canvas) => Ok(Json(NodeDto::canvas(id, &canvas))),
        Err(e) => Err(map_space_error(e)),
    }
}

/// 删画布
///
/// 连带删除它的全部文件条目；项目 Token 继续有效。
/// 引用归零的内容进 GC 队列，延迟一小时才真正删除。
#[utoipa::path(
    summary = "删除制作画布",
    delete,
    path = "/canvases/{id}",
    tag = "tree",
    params(("id" = String, Path, description = "画布 id")),
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "已删除，返回影响规模", body = DeleteResponse),
        (status = 404, description = "不存在或不属于调用者", body = super::error::ErrorBody),
    ),
)]
async fn delete_canvas(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(id): Path<String>,
) -> AppResult<Json<DeleteResponse>> {
    let _guard = state.operation_lock.lock().await;
    let id = parse_canvas_id(&id)?;
    let owner = canvas_owner(&state, &caller, id, "delete_canvas").await?;

    match space::delete_canvas(&state.store, owner, id, util::now_secs()).await? {
        Ok(r) => Ok(Json(DeleteResponse {
            folders: r.folders,
            canvases: r.canvases,
            files: r.files,
            dirs: r.dirs,
        })),
        Err(e) => Err(map_space_error(e)),
    }
}
