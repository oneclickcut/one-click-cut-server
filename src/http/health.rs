//! 健康检查。部署时给 systemd 与 cloudflared 探活用。

use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use super::AppState;

/// `/healthz` 响应体。
#[derive(Debug, Serialize, ToSchema)]
pub struct Health {
    /// 固定 `ok`。探活只看状态码，这个字段是给人看的。
    #[schema(example = "ok")]
    pub status: &'static str,
    /// 来自 `CARGO_PKG_VERSION`，用于确认线上跑的是哪个版本。
    #[schema(example = "0.1.0")]
    pub version: &'static str,
    /// 进程启动时刻，Unix 秒。
    #[schema(example = 1758182400_u64)]
    pub started_at: u64,
    /// 已运行秒数。重启会归零，可用来发现意外重启。
    #[schema(example = 42_u64)]
    pub uptime_seconds: u64,
}

/// 健康检查
///
/// 无需凭证。只表示进程活着且能响应，**不代表存储可用**——存储探活等 S8
/// 的 admin 接口，避免探活本身给 RocksDB 加压。
#[utoipa::path(
    summary = "检查服务是否存活",
    get,
    path = "/healthz",
    tag = "system",
    responses((status = 200, description = "服务存活", body = Health)),
)]
pub async fn healthz(State(state): State<AppState>) -> Json<Health> {
    let started_at = state
        .started_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let uptime_seconds = SystemTime::now()
        .duration_since(state.started_at)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        started_at,
        uptime_seconds,
    })
}
