//! 一键成片文件服务。
//!
//! 以画布为单位的文件存储与静态资源分发服务，设计文档见 `notes/架构设计-*.md`。
//!
//! 模块按三层划分，**依赖方向自上而下，不许反向**：
//!
//! - [`http`]：协议边界。状态码、请求头、路由只出现在这一层。
//! - [`domain`]：业务规则。不认识 HTTP。
//! - [`storage`]：存储引擎。不认识业务。
//!
//! 代码全挂在 lib 上、`main.rs` 只做启动，是为了让 `test/` 下的集成测试能
//! `use one_click_cut_server::*`——bin crate 是 `use` 不到的。

pub mod config;
pub mod domain;
pub mod http;
pub mod storage;
pub mod util;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::signal;

pub use config::Config;
pub use http::AppState;
pub use storage::Store;

/// 按配置初始化日志。
///
/// 只应调用一次；重复调用会返回错误（测试里用 `let _ =` 忽略即可）。
pub fn init_tracing(cfg: &config::LogConfig) -> Result<()> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // 环境变量 RUST_LOG 优先，方便临时调试不改配置。
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&cfg.filter))
        .context("日志过滤表达式非法")?;

    let registry = tracing_subscriber::registry().with(filter);

    if cfg.json {
        registry.with(fmt::layer().json()).try_init()
    } else {
        registry.with(fmt::layer()).try_init()
    }
    .map_err(|e| anyhow::anyhow!("初始化日志失败: {e}"))
}

/// 打开存储、组装 router、绑定端口并进入服务循环，收到 SIGTERM/Ctrl-C 后优雅退出。
pub async fn serve(config: Config) -> Result<()> {
    let store = Store::open(config.rocksdb_dir())?;
    tracing::info!(path = %store.path().display(), "存储已打开");

    let listen = config.listen;
    let docs_enabled = config.docs.enabled;
    let admin_username = config.admin.username.clone();

    // AppState 会校验 JWT 密钥，不合规直接在这里失败而不是等第一个请求。
    let state = AppState::new(config, store)?;

    domain::plans::sync_plan_limits(&state.store, &state.config.plans).await?;

    bootstrap_admin(&state, &admin_username).await?;

    // 后台维护任务：GC 每分钟、过期凭证每小时、对账每天。
    let _maintenance = domain::maintenance::spawn(state.store.clone());
    tracing::info!("后台维护任务已启动");

    let app = http::router(state);

    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("绑定监听地址失败: {listen}"))?;
    let local = listener.local_addr().context("读取本地地址失败")?;

    tracing::info!(addr = %local, docs = docs_enabled, "服务已启动");
    if docs_enabled {
        tracing::info!("接口文档: http://{local}/swagger-ui");
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("服务异常退出")?;

    tracing::info!("服务已停止");
    Ok(())
}

/// 首次启动时创建管理账户，并把一次性初始密码打进日志。
///
/// 已存在则什么都不做——**重启不重置密码**，否则每次重启都会在日志里留下一个
/// 可用的管理员凭证，而日志通常比密码库更容易被看到。
async fn bootstrap_admin(state: &AppState, username: &str) -> Result<()> {
    let created = domain::identity::ensure_admin(&state.store, username, util::now_secs()).await?;

    match created {
        Some(password) => {
            // 这是整个系统里唯一一处主动打印凭证的地方，而且只发生一次。
            tracing::warn!(
                username,
                password,
                "已创建管理账户，这个初始密码只显示一次，请立即登录并修改"
            );
        }
        None => tracing::debug!(username, "管理账户已存在，未做修改"),
    }
    Ok(())
}

/// 等待关闭信号。
///
/// 优雅关闭在这个项目里不只是礼貌：正在传输中的 Range 响应被硬切会让播放器
/// 报错，而 RocksDB 也需要机会把 memtable 刷干净。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!(error = %e, "注册 SIGTERM 处理失败"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl-C，开始优雅关闭"),
        _ = terminate => tracing::info!("收到 SIGTERM，开始优雅关闭"),
    }
}
