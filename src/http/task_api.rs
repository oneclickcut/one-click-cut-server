//! 公共任务队列接口；复用用户 JWT，拒绝 Agent Token。

use axum::Json;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use utoipa_axum::{
    router::{OpenApiRouter, UtoipaMethodRouterExt},
    routes,
};

use super::AppState;
use super::auth::UserCaller;
use super::error::{AppError, AppResult};
use crate::domain::{identity, task_queue};
use crate::storage::records::QueueTask;

pub fn router(max_payload_bytes: usize) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(push).layer(DefaultBodyLimit::max(max_payload_bytes)))
        .routes(routes!(pop).layer(DefaultBodyLimit::max(1024)))
        .routes(routes!(list))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PushResponse {
    pub task_id: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PopRequest {
    /// 最多领取多少条；省略时为 1。
    #[serde(default = "default_limit")]
    #[schema(minimum = 1, maximum = 100, default = 1)]
    pub limit: usize,
}

fn default_limit() -> usize {
    1
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TaskDto {
    pub task_id: String,
    /// 由登录身份确定的提交者，业务 payload 中的同名字段不会覆盖此值。
    pub user_id: String,
    /// Unix 秒。
    pub created_at: u64,
    pub payload: Value,
}

impl From<QueueTask> for TaskDto {
    fn from(task: QueueTask) -> Self {
        Self {
            task_id: task.task_id,
            user_id: task.user_id,
            created_at: task.created_at,
            payload: task.payload,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PopResponse {
    pub tasks: Vec<TaskDto>,
}

/// 查看全部尚未领取的任务，不修改队列。仅 reader_usernames 名单内的登录用户可用。
#[utoipa::path(
    get, path = "/tasks", tag = "tasks",
    summary = "查看当前任务列表",
    security(("bearer_jwt" = [])),
    responses(
        (status = 200, description = "当前未领取的全部任务，队列为空时 tasks 为 []", body = PopResponse),
        (status = 401, description = "登录凭证无效或账户已禁用"),
        (status = 403, description = "不在读取名单中或使用 Agent Token"),
        (status = 500, description = "存储失败或任务记录损坏")
    )
)]
pub async fn list(
    State(state): State<AppState>,
    caller: UserCaller,
) -> AppResult<Json<PopResponse>> {
    authorize_reader(&state, &caller).await?;
    let tasks = task_queue::list(&state.store).await?;
    Ok(Json(PopResponse {
        tasks: tasks.into_iter().map(Into::into).collect(),
    }))
}

/// 提交任意合法 JSON。每次调用生成新任务，重试可能重复入队。
#[utoipa::path(
    post, path = "/tasks/push", tag = "tasks",
    summary = "提交任务",
    security(("bearer_jwt" = [])),
    request_body = Value,
    responses(
        (status = 201, description = "任务已入队", body = PushResponse),
        (status = 400, description = "JSON 格式错误"),
        (status = 401, description = "登录凭证无效或账户已禁用"),
        (status = 403, description = "不接受 Agent Token"),
        (status = 413, description = "请求体超过配置的大小限制"),
        (status = 415, description = "需要 application/json"),
        (status = 429, description = "本周期提交次数已用尽"),
        (status = 500, description = "存储失败")
    )
)]
pub async fn push(
    State(state): State<AppState>,
    user: UserCaller,
    Json(payload): Json<Value>,
) -> AppResult<(StatusCode, Json<PushResponse>)> {
    let _guard = state.operation_lock.lock().await;
    let id = match task_queue::push_limited(
        &state.store,
        user.uid,
        payload,
        &state.config.plans,
        crate::util::now_secs(),
    )
    .await?
    {
        task_queue::PushOutcome::Created(id) => id,
        task_queue::PushOutcome::LimitReached {
            used,
            limit,
            next_reset_at,
        } => {
            return Err(AppError::MonthlyPushLimit(format!(
                "本周期任务提交次数已用尽：{used}/{limit}，下次重置时间 {next_reset_at}"
            )));
        }
    };
    Ok((
        StatusCode::CREATED,
        Json(PushResponse {
            task_id: id.to_string(),
        }),
    ))
}

/// 领取并删除任务。仅 reader_usernames 名单内的登录用户可用，管理员没有豁免。
/// 空队列立即返回空数组。删除后断网或处理失败不会恢复，不应自动重试本请求。
#[utoipa::path(
    post, path = "/tasks/pop", tag = "tasks",
    summary = "领取并删除任务",
    security(("bearer_jwt" = [])),
    request_body = PopRequest,
    responses(
        (status = 200, description = "已领取的任务，队列为空时 tasks 为 []", body = PopResponse),
        (status = 400, description = "limit 不在 1 到 100 之间或 JSON 格式错误"),
        (status = 401, description = "登录凭证无效或账户已禁用"),
        (status = 403, description = "不在读取名单中或使用 Agent Token"),
        (status = 413, description = "领取请求体超过 1024 字节"),
        (status = 415, description = "需要 application/json"),
        (status = 422, description = "请求字段类型错误或有未知字段"),
        (status = 500, description = "存储失败或任务记录损坏")
    )
)]
pub async fn pop(
    State(state): State<AppState>,
    caller: UserCaller,
    Json(request): Json<PopRequest>,
) -> AppResult<Json<PopResponse>> {
    authorize_reader(&state, &caller).await?;
    if !(1..=100).contains(&request.limit) {
        return Err(AppError::BadRequest("limit 必须在 1 到 100 之间".into()));
    }
    let tasks = task_queue::pop(&state.store, request.limit).await?;
    Ok(Json(PopResponse {
        tasks: tasks.into_iter().map(Into::into).collect(),
    }))
}

pub(crate) async fn authorize_reader(state: &AppState, caller: &UserCaller) -> AppResult<()> {
    let user = identity::find_by_uid(&state.store, caller.uid)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if user.disabled {
        return Err(AppError::Unauthorized);
    }
    if !state
        .config
        .task_queue
        .reader_usernames
        .contains(&user.username)
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}
