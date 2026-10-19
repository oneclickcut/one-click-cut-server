//! 统一错误 → HTTP 状态码映射。
//!
//! 这里是**唯一**决定「某个业务错误对外长什么样」的地方。各接口只返回
//! [`AppError`]，不自己拼状态码，这样才能保证同一类错误在所有接口上一致。
//!
//! TODO(S5)：补齐全部业务错误分支，并对 [`AppError`] 实现一次 `utoipa::IntoResponses`，
//! 各接口引用即可，不必逐个手写 4xx/5xx 响应声明。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// 对外错误响应体。
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// 稳定的机器可读错误码，客户端按它分支，不要解析 `message`。
    #[schema(example = "not_found")]
    pub code: &'static str,
    /// 人读的错误说明，可能随版本变化。
    #[schema(example = "资源不存在")]
    pub message: String,
}

/// 应用层错误。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("请求体超过大小限制")]
    PayloadTooLarge,
    /// 资源不存在。**跨用户访问也统一走这里**，不用 403——403 会泄露
    /// 「这个 id 确实存在，只是不属于你」。
    #[error("资源不存在")]
    NotFound,

    /// 入参不合法（path 非法、字段缺失、值越界）。
    #[error("{0}")]
    BadRequest(String),

    /// 凭证缺失或非法。
    #[error("未认证")]
    Unauthorized,

    /// 凭证合法但权限不够（例如 agent token 没有 `delete` 位）。
    #[error("无权限")]
    Forbidden,

    #[error(
        "当前账号未获生成完成发布权限；请检查服务端实际生效的 gallery.publisher_usernames 或 OCC_GALLERY_PUBLISHER_USERNAMES，修改后重启服务"
    )]
    GalleryPublisherRequired,

    #[error("基础版必须自动共享作品；升级 Pro 或 Max 可修改共享设置")]
    AutoShareRequiredByPlan,

    /// 与现有资源冲突（用户名被占用、非空文件夹未带 recursive）。
    #[error("{0}")]
    Conflict(String),

    #[error("项目已有执行任务")]
    ProjectExecutionBusy,

    /// 超出配额。
    #[error("{0}")]
    InsufficientStorage(String),

    #[error("{0}")]
    MonthlyPushLimit(String),

    #[error("每日新用户注册数已达上限")]
    DailyRegistrationLimit { retry_after: u64 },

    /// Range 请求无法满足。`0` 是资源总大小，用于 `Content-Range: bytes */{size}`。
    #[error("请求的字节区间无法满足")]
    RangeNotSatisfiable(u64),

    /// 内部错误。**对外不暴露细节**，只在服务端日志里留全貌。
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    /// 状态码与机器可读错误码。
    fn parts(&self) -> (StatusCode, &'static str) {
        match self {
            Self::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::AutoShareRequiredByPlan => (StatusCode::FORBIDDEN, "auto_share_required_by_plan"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::GalleryPublisherRequired => (StatusCode::FORBIDDEN, "gallery_publisher_required"),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Self::ProjectExecutionBusy => (StatusCode::LOCKED, "project_execution_busy"),
            Self::InsufficientStorage(_) => (StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded"),
            Self::MonthlyPushLimit(_) => {
                (StatusCode::TOO_MANY_REQUESTS, "monthly_push_limit_exceeded")
            }
            Self::DailyRegistrationLimit { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "daily_registration_limit_exceeded",
            ),
            Self::RangeNotSatisfiable(_) => {
                (StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable")
            }
            Self::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.parts();

        // 内部错误的原始链路只进日志，不进响应体。
        let message = match &self {
            Self::Internal(err) => {
                tracing::error!(error = ?err, "内部错误");
                "服务内部错误".to_owned()
            }
            other => other.to_string(),
        };

        let mut resp = (status, Json(ErrorBody { code, message })).into_response();

        if let Self::DailyRegistrationLimit { retry_after } = &self
            && let Ok(v) = retry_after.to_string().parse()
        {
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, v);
        }

        // 416 必须带 `Content-Range: bytes */{size}`，否则客户端不知道该退到哪。
        if let Self::RangeNotSatisfiable(size) = self
            && let Ok(v) = format!("bytes */{size}").parse()
        {
            resp.headers_mut()
                .insert(axum::http::header::CONTENT_RANGE, v);
        }

        resp
    }
}

/// 接口返回类型别名。
pub type AppResult<T> = std::result::Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态码映射固定() {
        assert_eq!(AppError::NotFound.parts().0, StatusCode::NOT_FOUND);
        assert_eq!(AppError::Unauthorized.parts().0, StatusCode::UNAUTHORIZED);
        assert_eq!(AppError::Forbidden.parts().0, StatusCode::FORBIDDEN);
        assert_eq!(
            AppError::BadRequest("x".into()).parts().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            AppError::Internal(anyhow::anyhow!("boom")).parts().0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn 内部错误不泄露细节() {
        let err = AppError::Internal(anyhow::anyhow!("数据库密码是 hunter2"));
        // 只断言分支选择；响应体的脱敏在 into_response 里完成，
        // 由 test/test-api 的集成测试覆盖实际字节。
        assert_eq!(err.parts().1, "internal");
    }
}

/// 领域错误保留权限与冲突语义，其余错误不泄露存储细节。
pub fn gallery_error(error: anyhow::Error) -> AppError {
    use crate::domain::{gallery::ShareError, gallery_policy::PolicyError};
    if let Some(e) = error.downcast_ref::<PolicyError>() {
        return match e {
            PolicyError::RequiredByPlan => AppError::AutoShareRequiredByPlan,
            PolicyError::Forbidden => AppError::Forbidden,
            PolicyError::NotFound => AppError::NotFound,
            PolicyError::Conflict(s) => AppError::Conflict(s.clone()),
        };
    }
    if let Some(e) = error.downcast_ref::<ShareError>() {
        return match e {
            ShareError::Invalid(s) => AppError::BadRequest(s.clone()),
            ShareError::Conflict(s) => AppError::Conflict(s.clone()),
            ShareError::Internal(_) => AppError::Internal(error),
        };
    }
    AppError::Internal(error)
}
