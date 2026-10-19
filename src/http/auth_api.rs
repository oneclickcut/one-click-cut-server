//! 认证接口：注册、单 JWT 登录、当前用户。

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::UserCaller;
use super::error::{AppError, AppResult};
use crate::domain::{gallery_policy, identity, plans};
use crate::storage::records::Role;
use crate::util;

/// 挂在 `/api/auth` 下的接口。
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(register))
        .routes(routes!(login))
}

/// `/api/me` 单独挂，因为它不在 `/api/auth` 前缀下（接口文档第 3 节）。
pub fn router_me() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(me))
}

/// 角色的对外表示。
///
/// 为什么不直接给 [`Role`] 加 `#[derive(ToSchema)]`：`Role` 住在 `storage/records.rs`，
/// 而分层规则要求**存储层不认识业务、更不认识 HTTP**。给它加 utoipa 派生
/// 等于让存储层依赖一个接口文档框架。
///
/// 镜像一个 DTO 还有个实际好处：线上格式与落盘格式可以独立演进——
/// 以后加一个内部角色但不想暴露给客户端时，这里不动就行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RoleDto {
    User,
    Admin,
}

impl From<Role> for RoleDto {
    fn from(r: Role) -> Self {
        match r {
            Role::User => Self::User,
            Role::Admin => Self::Admin,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct Credentials {
    #[schema(example = "alice")]
    pub username: String,
    #[schema(example = "correct horse battery")]
    pub password: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TokenResponse {
    pub access_token: String,
    /// JWT 有效秒数；null 表示队列读取名单账号的永久凭证。
    #[schema(example = 7776000_u64, required = true)]
    pub expires_in: Option<u64>,
    #[schema(example = "user")]
    pub role: RoleDto,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct MeResponse {
    pub auto_share_gallery: bool,
    pub can_disable_auto_share_gallery: bool,
    #[schema(value_type = String, example = "01JBXY7Z3K8QW5N2M4P6R8T0V1")]
    pub uid: String,
    #[schema(example = "alice")]
    pub username: String,
    pub role: RoleDto,
    /// 已用字节数。
    pub bytes_used: u64,
    /// 配额上限，0 表示不限。
    pub quota_bytes: u64,
    #[schema(value_type = Option<String>)]
    pub plan: Option<plans::Plan>,
    pub plan_name: Option<&'static str>,
    pub plan_started_at: Option<u64>,
    pub storage_remaining_bytes: Option<u64>,
    pub monthly_push_limit: Option<u64>,
    pub push_used: Option<u64>,
    pub push_remaining: Option<u64>,
    pub period_start: Option<u64>,
    pub next_reset_at: Option<u64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterResponse {
    #[schema(value_type = String)]
    pub uid: String,
    pub username: String,
}

/// 注册
///
/// 用户名只允许字母、数字、下划线、短横线与点；密码至少 8 个字符。
/// 管理账户不开放注册，由启动时自动创建。
#[utoipa::path(
    summary = "注册用户",
    post,
    path = "/register",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 201, description = "注册成功", body = RegisterResponse),
        (status = 400, description = "用户名或密码不合规", body = super::error::ErrorBody),
        (status = 409, description = "用户名已被占用", body = super::error::ErrorBody),
        (status = 429, description = "今日公开注册额度已用尽", body = super::error::ErrorBody),
    ),
)]
async fn register(
    State(state): State<AppState>,
    Json(body): Json<Credentials>,
) -> AppResult<(StatusCode, Json<RegisterResponse>)> {
    // 校验失败的具体原因可以告诉调用者——这是注册接口，用户名是否可用本来就是公开信息。
    crate::storage::keys::validate_username(&body.username)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    identity::validate_password(&body.password).map_err(|e| AppError::BadRequest(e.to_string()))?;
    let _guard = state.operation_lock.lock().await;
    let now = util::now_secs();
    let created = identity::register_public(
        &state.store,
        &body.username,
        &body.password,
        now,
        state.config.plans.basic.storage_bytes,
        state.config.registration.daily_limit,
    )
    .await
    .map_err(AppError::Internal)?;

    let (uid, user) = match created {
        identity::RegistrationOutcome::Created(uid, user) => (uid, user),
        identity::RegistrationOutcome::UsernameTaken => {
            return Err(AppError::Conflict("用户名已被占用".into()));
        }
        identity::RegistrationOutcome::DailyLimitReached => {
            let seconds_to_midnight = 86_400 - (now + 8 * 3_600) % 86_400;
            return Err(AppError::DailyRegistrationLimit {
                retry_after: seconds_to_midnight,
            });
        }
    };

    tracing::info!(uid = %uid, username = %user.username, "新用户注册");
    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            uid: uid.to_text(),
            username: user.username,
        }),
    ))
}

/// 登录
///
/// 失败一律返回 401 且响应体完全一致——**不区分「用户不存在」与「密码错误」**，
/// 否则这个接口会变成用户名枚举工具。
#[utoipa::path(
    summary = "登录并获取令牌",
    post,
    path = "/login",
    tag = "auth",
    request_body = Credentials,
    responses(
        (status = 200, description = "登录成功", body = TokenResponse),
        (status = 401, description = "凭证无效", body = super::error::ErrorBody),
    ),
)]
async fn login(
    State(state): State<AppState>,
    Json(body): Json<Credentials>,
) -> AppResult<Json<TokenResponse>> {
    let now = util::now_secs();
    match identity::login(
        &state.store,
        &state.identity,
        &body.username,
        &body.password,
        now,
        &state.config.task_queue.reader_usernames,
    )
    .await?
    {
        Ok(token) => Ok(Json(TokenResponse {
            access_token: token.access_token,
            expires_in: token.expires_in,
            role: token.role.into(),
        })),
        Err(reason) => {
            // 原因只进服务端日志，不进响应。
            debug_assert!(reason.is_credential_failure());
            tracing::info!(
                username = %body.username,
                reason = ?reason,
                "登录失败"
            );
            Err(AppError::Unauthorized)
        }
    }
}

/// 当前用户
#[utoipa::path(
    summary = "获取当前用户信息",
    get,
    path = "/me",
    tag = "auth",
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "当前用户信息", body = MeResponse),
        (status = 401, description = "未认证", body = super::error::ErrorBody),
    ),
)]
async fn me(State(state): State<AppState>, caller: UserCaller) -> AppResult<Json<MeResponse>> {
    let _guard = state.operation_lock.lock().await;
    let user = identity::find_by_uid(&state.store, caller.uid)
        .await?
        .filter(|u| !u.disabled)
        .ok_or(AppError::Unauthorized)?;
    let (auto_share_gallery, can_disable_auto_share_gallery) =
        gallery_policy::preferences(&state.store, caller.uid).await?;

    let plan = if user.role.is_admin() {
        None
    } else {
        let state_plan = plans::get(&state.store, caller.uid, user.created_at).await?;
        Some(state_plan.current(util::now_secs())?)
    };
    let (
        plan_code,
        plan_name,
        started,
        storage_remaining,
        monthly_limit,
        push_used,
        push_remaining,
        period_start,
        next_reset_at,
    ) = if let Some((plan, next)) = plan {
        let limits = plan.plan.limits(&state.config.plans);
        (
            Some(plan.plan),
            Some(plan.plan.name()),
            Some(plan.plan_started_at),
            Some(user.quota_bytes.saturating_sub(user.bytes_used)),
            Some(limits.monthly_push_limit),
            Some(plan.push_used),
            Some(limits.monthly_push_limit.saturating_sub(plan.push_used)),
            Some(plan.push_period_start),
            Some(next),
        )
    } else {
        (None, None, None, None, None, None, None, None, None)
    };
    Ok(Json(MeResponse {
        auto_share_gallery,
        can_disable_auto_share_gallery,
        uid: caller.uid.to_text(),
        username: user.username,
        role: user.role.into(),
        bytes_used: user.bytes_used,
        quota_bytes: user.quota_bytes,
        plan: plan_code,
        plan_name,
        plan_started_at: started,
        storage_remaining_bytes: storage_remaining,
        monthly_push_limit: monthly_limit,
        push_used,
        push_remaining,
        period_start,
        next_reset_at,
    }))
}
