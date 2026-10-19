//! 画布**内**目录的列一层、建、删。
//!
//! # 为什么目录 path 不进 URL 路径段
//!
//! `{path}` 在 axum 里只匹配一段，含斜杠的路径得写成 `{*path}`，而那个写法进
//! OpenAPI 就变成一个叫 `*path` 的非法参数名——[`super::file_api`] 和
//! [`super::static_api`] 已经为此各写了一段注释并手工登记路径。
//!
//! 目录接口不必再踩一遍：**path 一律走 query 或 body**，于是全部接口都能正常用
//! `routes!` 宏登记，路由与文档一次搞定。
//!
//! # 用词
//!
//! 这一层全部叫「目录 / dir」，指画布**内部**；画布外那棵树上的叫
//! 「文件夹 / folder」，在 [`super::tree_api`]。不要出现 `canvas_folder`
//! 这种混合命名。

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::Caller;
use super::error::{AppError, AppResult};
use super::file_api::{FileDto, Need, authorize_canvas};
use crate::domain::canvas;
use crate::domain::canvas_dir::{self, DirError, DirRow};
use crate::storage::keys;
use crate::util;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_entries))
        .routes(routes!(create_dir, delete_dir))
}

/// 把领域层的失败映射成 HTTP 状态码。
///
/// **不引入新的 `code` 值**：`not_found` / `conflict` / `bad_request` 已经够用，
/// 前端的错误分支一行都不用加。
fn map_dir_error(e: DirError) -> AppError {
    match e {
        // 画布不存在、不属于你、目录不存在，三者都是 404，与既有接口一致。
        DirError::CanvasNotFound | DirError::DirNotFound => AppError::NotFound,
        DirError::NotEmpty => {
            AppError::Conflict("目录非空，需要 ?recursive=true 才能级联删除".into())
        }
        DirError::TooManyDirs => AppError::Conflict(format!(
            "单个画布最多 {} 个目录",
            canvas_dir::MAX_DIRS_PER_CANVAS
        )),
        DirError::PathConflict(c) => AppError::Conflict(match c {
            canvas::PathConflict::TargetIsDir => "这个路径已经是一个目录".into(),
            canvas::PathConflict::AncestorIsFile => {
                "路径上有一段已经是文件，同一个名字不能既是文件又是目录".to_owned()
            }
        }),
    }
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct DirDto {
    /// 画布内完整路径，不带前后斜杠。
    #[schema(example = "scene01/raw")]
    pub path: String,
    /// 本层显示名。
    #[schema(example = "raw")]
    pub name: String,
    /// **整棵子树**的文件数，不是直接子项。
    pub file_count: u64,
    /// 整棵子树的字节数。
    pub bytes: u64,
    /// 只有显式建过的目录才有；纯粹从文件 path 派生出来的目录没有这个字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
}

impl From<DirRow> for DirDto {
    fn from(r: DirRow) -> Self {
        Self {
            path: r.path,
            name: r.name,
            file_count: r.file_count,
            bytes: r.bytes,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EntriesResponse {
    /// 当前所在的目录，根层是空字符串。
    #[schema(example = "scene01")]
    pub prefix: String,
    /// 子目录，按名字字典序。
    pub dirs: Vec<DirDto>,
    /// 本层的文件（不含子目录里的），按名字字典序。
    pub files: Vec<FileDto>,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct EntriesQuery {
    /// 要列的目录，省略即画布根层。前后斜杠可有可无（`scene01` 与 `scene01/` 等价）。
    pub prefix: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDirRequest {
    /// 目录路径，不带前后斜杠。中间层不存在会自动补齐。
    #[schema(example = "scene01/raw")]
    pub path: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DirCreated {
    pub path: String,
    /// false 表示这个目录本来就在（幂等命中），此时状态码是 200 而不是 201。
    pub created: bool,
    /// 实际写入的记录数，含自动补齐的中间层。
    pub dirs_written: u64,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct DeleteDirQuery {
    /// 要删的目录路径。
    #[param(example = "scene01")]
    pub path: String,
    /// 非空目录必须显式带上它才能级联删除。
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteDirResponse {
    pub deleted_files: u64,
    /// 删掉的目录记录数（含子目录）。派生出来的目录不落盘，所以不计入。
    pub deleted_dirs: u64,
    /// 释放的逻辑字节数。`GET /api/me` 的 `bytes_used` 会同步下降。
    pub freed_bytes: u64,
}

// ---------------------------------------------------------------------------
// 入参规范化
// ---------------------------------------------------------------------------

/// 规范化目录路径：去掉前后斜杠，空串视为根层。
///
/// 前端拼 query 时 `prefix=scene01/`、`prefix=/scene01`、`prefix=` 都会出现，
/// 在这里统一，而不是让每个 handler 各自 trim。
fn normalize_dir(raw: Option<&str>) -> AppResult<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    keys::validate_dir_path(trimmed).map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Some(trimmed.to_owned()))
}

/// 删与建必须指定一个具体目录，根层不是合法目标。
fn require_dir(raw: &str) -> AppResult<String> {
    normalize_dir(Some(raw))?.ok_or_else(|| AppError::BadRequest("目录路径不能为空".into()))
}

// ---------------------------------------------------------------------------
// 接口
// ---------------------------------------------------------------------------

/// 列出画布内一层的子目录与文件
///
/// 目录树从文件 `path` 里的斜杠派生，另外算上显式建过的空目录。
/// 子目录的 `file_count` / `bytes` 是**整棵子树**的合计。
///
/// 不分页：与 `/api/tree`、`/api/canvases/{id}/files` 一致，单画布 5000 文件的
/// 上限已经把最坏情况钉住了。
#[utoipa::path(
    summary = "浏览画布目录",
    get,
    path = "/canvases/{id}/entries",
    tag = "dirs",
    params(("id" = String, Path, description = "画布 id"), EntriesQuery),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "这一层的子目录与文件", body = EntriesResponse),
        (status = 400, description = "prefix 不合规", body = super::error::ErrorBody),
        (status = 404, description = "画布或目录不存在", body = super::error::ErrorBody),
    ),
)]
pub(crate) async fn list_entries(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Query(q): Query<EntriesQuery>,
) -> AppResult<Json<EntriesResponse>> {
    let (_, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Read).await?;
    let prefix = normalize_dir(q.prefix.as_deref())?;

    let list = canvas_dir::list_entries(&state.store, canvas_id, prefix.as_deref())
        .await?
        .map_err(map_dir_error)?;

    Ok(Json(EntriesResponse {
        prefix: prefix.unwrap_or_default(),
        dirs: list.dirs.into_iter().map(Into::into).collect(),
        files: list
            .files
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

/// 建一个画布内目录
///
/// **幂等**：目录已经存在（显式建过，或已有文件落在这个前缀下）时返回 200 而不是
/// 报错，因为目录的身份就是路径，重复创建没有歧义。中间层会自动补齐。
///
/// 上传文件**不需要**先建目录——`path` 里的斜杠自己就会形成目录。这个接口存在的
/// 理由只有一个：让还没有任何文件的空目录也能持久化。
#[utoipa::path(
    summary = "创建画布目录",
    post,
    path = "/canvases/{id}/dirs",
    tag = "dirs",
    params(("id" = String, Path, description = "画布 id")),
    request_body = CreateDirRequest,
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 201, description = "已创建", body = DirCreated),
        (status = 200, description = "已存在，幂等返回", body = DirCreated),
        (status = 400, description = "路径不合规（含段数超过 15）", body = super::error::ErrorBody),
        (status = 404, description = "画布不存在或不属于调用者", body = super::error::ErrorBody),
        (status = 409, description = "同名文件已存在，或超出目录数上限", body = super::error::ErrorBody),
    ),
)]
pub(crate) async fn create_dir(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<CreateDirRequest>,
) -> AppResult<(StatusCode, Json<DirCreated>)> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Write).await?;
    let path = require_dir(&body.path)?;

    let outcome = canvas_dir::create_dir(&state.store, uid, canvas_id, &path, util::now_secs())
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
        .map_err(map_dir_error)?;

    let status = if outcome.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(DirCreated {
            path,
            created: outcome.created,
            dirs_written: outcome.dirs_written,
        }),
    ))
}

/// 删一个画布内目录
///
/// 非空时必须带 `?recursive=true`，否则 409——与删非空文件夹的手感对齐。
/// 级联删除会连带给内容减引用，归零的进 GC 队列（延迟一小时真删）。
#[utoipa::path(
    summary = "删除画布目录",
    delete,
    path = "/canvases/{id}/dirs",
    tag = "dirs",
    params(("id" = String, Path, description = "画布 id"), DeleteDirQuery),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "已删除，返回影响规模", body = DeleteDirResponse),
        (status = 400, description = "路径不合规", body = super::error::ErrorBody),
        (status = 404, description = "画布或目录不存在", body = super::error::ErrorBody),
        (status = 409, description = "非空且未带 recursive", body = super::error::ErrorBody),
    ),
)]
pub(crate) async fn delete_dir(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Query(q): Query<DeleteDirQuery>,
) -> AppResult<Json<DeleteDirResponse>> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Delete).await?;
    let path = require_dir(&q.path)?;

    let report = canvas_dir::delete_dir(
        &state.store,
        uid,
        canvas_id,
        &path,
        q.recursive,
        util::now_secs(),
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?
    .map_err(map_dir_error)?;

    Ok(Json(DeleteDirResponse {
        deleted_files: report.deleted_files,
        deleted_dirs: report.deleted_dirs,
        freed_bytes: report.freed_bytes,
    }))
}
