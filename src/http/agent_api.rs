//! 项目级 Agent Token 管理。Token 只能由用户身份签发和吊销。
use super::{
    AppState,
    auth::UserCaller,
    error::{AppError, AppResult},
};
use crate::{
    domain::{agent, space},
    storage::{
        Cf,
        keys::{self, ProjectId},
        records::{self, AgentPerms},
    },
    util,
};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(issue_token, list_tokens))
        .routes(routes!(revoke_token))
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct IssueRequest {
    #[serde(flatten)]
    pub perms: AgentPermsDto,
    pub ttl_secs: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AgentPermsDto {
    pub canvas_files_write: Option<bool>,
    pub canvas_files_delete: Option<bool>,
    pub canvas_create: Option<bool>,
    pub constraints_write: Option<bool>,
    pub constraints_delete: Option<bool>,
}
impl From<AgentPermsDto> for AgentPerms {
    fn from(p: AgentPermsDto) -> Self {
        Self {
            canvas_files_write: p.canvas_files_write.unwrap_or(true),
            canvas_files_delete: p.canvas_files_delete.unwrap_or(false),
            canvas_create: p.canvas_create.unwrap_or(false),
            constraints_write: p.constraints_write.unwrap_or(false),
            constraints_delete: p.constraints_delete.unwrap_or(false),
        }
    }
}
impl From<AgentPerms> for AgentPermsDto {
    fn from(p: AgentPerms) -> Self {
        Self {
            canvas_files_write: Some(p.canvas_files_write),
            canvas_files_delete: Some(p.canvas_files_delete),
            canvas_create: Some(p.canvas_create),
            constraints_write: Some(p.constraints_write),
            constraints_delete: Some(p.constraints_delete),
        }
    }
}
#[derive(Debug, Serialize, ToSchema)]
pub struct IssuedTokenResponse {
    pub token: String,
    pub token_id: String,
    pub prefix: String,
    pub expires_at: u64,
    #[serde(flatten)]
    pub perms: AgentPermsDto,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct TokenSummaryDto {
    pub token_id: String,
    pub prefix: String,
    pub project_id: String,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(flatten)]
    pub perms: AgentPermsDto,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct TokenListResponse {
    pub tokens: Vec<TokenSummaryDto>,
}

async fn authorize(
    state: &AppState,
    caller: &UserCaller,
    raw: &str,
) -> AppResult<(keys::Uid, ProjectId)> {
    let id = ProjectId::parse(raw).map_err(|_| AppError::NotFound)?;
    if space::get_project(&state.store, caller.uid, id)
        .await?
        .is_some()
    {
        return Ok((caller.uid, id));
    }
    if caller.is_admin() {
        for (key, _) in state
            .store
            .scan_prefix(Cf::Meta, keys::project_prefix())
            .await?
        {
            let (uid, got) = keys::parse_project_key(&key).map_err(AppError::Internal)?;
            if got == id {
                super::auth::audit_admin_action(caller.uid, "agent_tokens", uid, raw);
                return Ok((uid, id));
            }
        }
    }
    Err(AppError::NotFound)
}

#[utoipa::path(summary="签发项目级 Agent Token",post, path="/projects/{id}/agent-tokens", tag="agent", params(("id"=String,Path)), request_body=IssueRequest, security(("bearer_jwt"=[])), responses((status=201, body=IssuedTokenResponse)))]
async fn issue_token(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(id): Path<String>,
    Json(body): Json<IssueRequest>,
) -> AppResult<(StatusCode, Json<IssuedTokenResponse>)> {
    let _guard = state.operation_lock.lock().await;
    let (uid, id) = authorize(&state, &caller, &id).await?;
    let project = space::get_project(&state.store, uid, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if project.status != records::ProjectStatus::Active {
        return Err(AppError::Conflict("项目不可写".into()));
    }
    let issued = agent::issue(
        &state.store,
        uid,
        id,
        body.perms.into(),
        body.ttl_secs,
        util::now_secs(),
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok((
        StatusCode::CREATED,
        Json(IssuedTokenResponse {
            token: issued.plaintext,
            token_id: issued.token_id,
            prefix: issued.prefix,
            expires_at: issued.expires_at,
            perms: issued.perms.into(),
        }),
    ))
}
#[utoipa::path(summary="列出项目级 Agent Token",get, path="/projects/{id}/agent-tokens", tag="agent", params(("id"=String,Path)), security(("bearer_jwt"=[])), responses((status=200, body=TokenListResponse)))]
async fn list_tokens(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(id): Path<String>,
) -> AppResult<Json<TokenListResponse>> {
    let (_, id) = authorize(&state, &caller, &id).await?;
    let tokens = agent::list_for_project(&state.store, id, util::now_secs())
        .await?
        .into_iter()
        .map(|t| TokenSummaryDto {
            token_id: t.token_id,
            prefix: t.prefix,
            project_id: t.project_id.to_text(),
            created_at: t.created_at,
            expires_at: t.expires_at,
            perms: t.perms.into(),
        })
        .collect();
    Ok(Json(TokenListResponse { tokens }))
}
#[utoipa::path(summary="吊销 Agent Token",delete, path="/agent-tokens/{token_id}", tag="agent", params(("token_id"=String,Path)), security(("bearer_jwt"=[])), responses((status=204)))]
async fn revoke_token(
    State(state): State<AppState>,
    caller: UserCaller,
    Path(token_id): Path<String>,
) -> AppResult<StatusCode> {
    let _guard = state.operation_lock.lock().await;
    let hash = keys::ContentHash::parse_hex(&token_id).map_err(|_| AppError::NotFound)?;
    let raw = state
        .store
        .get(Cf::Auth, keys::agent_token_key(hash))
        .await?
        .ok_or(AppError::NotFound)?;
    let record: records::AgentToken = records::decode(&raw).map_err(AppError::Internal)?;
    if !caller.can_access_space(record.uid) {
        return Err(AppError::NotFound);
    }
    agent::revoke(&state.store, &token_id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}
