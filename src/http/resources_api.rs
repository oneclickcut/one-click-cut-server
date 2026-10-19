//! 项目公共资料入口。内部复用画布文件与目录能力。
use super::{
    AppState,
    auth::Caller,
    dir_api,
    error::{AppError, AppResult},
    file_api, project_api,
};
use crate::domain::space;
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router(max_single_bytes: usize) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_files))
        .routes(routes!(list_entries))
        .routes(routes!(create_dir, delete_dir))
        .route(
            "/projects/{id}/resources/files/{*path}",
            axum::routing::put(put_file)
                .delete(delete_file)
                .layer(axum::extract::DefaultBodyLimit::max(max_single_bytes)),
        )
}
async fn resources_id(state: &AppState, caller: &Caller, raw: &str) -> AppResult<String> {
    let (uid, _, p) = project_api::authorize(state, caller, raw, false).await?;
    let c = space::get_canvas(&state.store, uid, p.resources_canvas_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if c.project_id.to_text() != raw || c.role != crate::storage::records::CanvasRole::Resources {
        return Err(AppError::NotFound);
    }
    Ok(p.resources_canvas_id.to_text())
}
#[utoipa::path(summary="列出项目公共资料文件",get,path="/projects/{id}/resources/files",tag="constraints",params(("id"=String,Path)),security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=200,body=file_api::FileListResponse)))]
async fn list_files(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> AppResult<Json<file_api::FileListResponse>> {
    let cid = resources_id(&state, &caller, &id).await?;
    file_api::list_files(State(state), caller, Path(cid)).await
}
#[utoipa::path(summary="浏览项目公共资料目录",get,path="/projects/{id}/resources/entries",tag="constraints",params(("id"=String,Path),dir_api::EntriesQuery),security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=200,body=dir_api::EntriesResponse)))]
async fn list_entries(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Query(q): Query<dir_api::EntriesQuery>,
) -> AppResult<Json<dir_api::EntriesResponse>> {
    let cid = resources_id(&state, &caller, &id).await?;
    dir_api::list_entries(State(state), caller, Path(cid), Query(q)).await
}
#[utoipa::path(summary="创建项目公共资料目录",post,path="/projects/{id}/resources/dirs",tag="constraints",params(("id"=String,Path)),request_body=dir_api::CreateDirRequest,security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=201,body=dir_api::DirCreated)))]
async fn create_dir(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<dir_api::CreateDirRequest>,
) -> AppResult<(StatusCode, Json<dir_api::DirCreated>)> {
    let cid = resources_id(&state, &caller, &id).await?;
    dir_api::create_dir(State(state), caller, Path(cid), Json(body)).await
}
#[utoipa::path(summary="删除项目公共资料目录",delete,path="/projects/{id}/resources/dirs",tag="constraints",params(("id"=String,Path),dir_api::DeleteDirQuery),security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=200,body=dir_api::DeleteDirResponse)))]
async fn delete_dir(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Query(q): Query<dir_api::DeleteDirQuery>,
) -> AppResult<Json<dir_api::DeleteDirResponse>> {
    let cid = resources_id(&state, &caller, &id).await?;
    dir_api::delete_dir(State(state), caller, Path(cid), Query(q)).await
}
#[utoipa::path(summary="上传或覆盖项目公共资料文件",put,path="/api/projects/{id}/resources/files/{path}",tag="constraints",params(("id"=String,Path),("path"=String,Path),file_api::MimeQuery),request_body(content=Vec<u8>,content_type="application/octet-stream"),security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=200,body=file_api::PutFileResponse),(status=413,description="请求体超过单次上传上限，改走分块上传")))]
pub async fn put_file(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, path)): Path<(String, String)>,
    Query(q): Query<file_api::MimeQuery>,
    body: Bytes,
) -> AppResult<Json<file_api::PutFileResponse>> {
    let cid = resources_id(&state, &caller, &id).await?;
    file_api::put_file(State(state), caller, Path((cid, path)), Query(q), body).await
}
#[utoipa::path(summary="删除项目公共资料文件",delete,path="/api/projects/{id}/resources/files/{path}",tag="constraints",params(("id"=String,Path),("path"=String,Path)),security(("bearer_jwt"=[]),("agent_token"=[])),responses((status=204)))]
pub async fn delete_file(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, path)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let cid = resources_id(&state, &caller, &id).await?;
    file_api::delete_file(State(state), caller, Path((cid, path))).await
}
