//! 第一层：协议边界。router 组装与中间件栈。
//!
//! 路径前缀的划分（管理 API 与静态资源共用一个域）：
//!
//! | 前缀 | 用途 | 缓存 |
//! |------|------|------|
//! | `/api/*` | 管理接口，JWT 或 Agent Token | `no-store` |
//! | `/f/*` | 静态资源，无需凭证 | 长缓存 |
//! | `/swagger-ui`、`/api-docs` | 接口文档，按配置开关 | `no-store` |

pub mod admin_api;
pub mod agent_api;
pub mod auth;
pub mod auth_api;
pub mod dir_api;
pub mod error;
pub mod file_api;
pub mod gallery_api;
pub mod health;
pub mod jobs_api;
pub mod openapi;
pub mod project_api;
pub mod range;
pub mod resources_api;
pub mod static_api;
pub mod task_api;
pub mod tree_api;
pub mod upload_api;

use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use axum::http::Request;
use tower::ServiceBuilder;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi as _;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

use crate::config::Config;
use crate::domain::identity::Identity;
use crate::storage::Store;

/// 共享状态。克隆代价是几次 `Arc` 引用计数。
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    /// JWT 签发与校验。
    pub identity: Identity,
    /// 进程启动时刻，`/healthz` 用来算 uptime。
    pub started_at: SystemTime,
    pub operation_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    /// 构造共享状态。
    ///
    /// JWT 密钥不合规（太短）时直接返回错误——**宁可起不来，也不要带着弱密钥上线**。
    pub fn new(config: Config, store: Store) -> anyhow::Result<Self> {
        config.task_queue.validate()?;
        config.gallery.validate()?;
        config.plans.validate()?;
        let identity = Identity::new(&config.jwt.secret)?;
        Ok(Self {
            config: Arc::new(config),
            store,
            identity,
            started_at: SystemTime::now(),
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
}

/// 组装完整的 router。
pub fn router(state: AppState) -> Router {
    let docs_enabled = state.config.docs.enabled;

    // 用 OpenApiRouter 注册，路由与文档同一次登记。
    let (router, api) = OpenApiRouter::with_openapi(openapi::ApiDoc::openapi())
        .routes(routes!(health::healthz))
        .nest("/api/auth", auth_api::router())
        // `/api/me` 与树接口都挂在 /api 下，与接口文档第 3、4 节一致。
        .nest("/api", auth_api::router_me())
        .nest("/api", tree_api::router())
        .nest("/api", project_api::router())
        .nest("/api", gallery_api::router())
        .nest(
            "/api",
            resources_api::router(state.config.upload.max_single_bytes),
        )
        .nest(
            "/api",
            file_api::router(state.config.upload.max_single_bytes),
        )
        .nest("/api", dir_api::router())
        .nest("/api", agent_api::router())
        .nest(
            "/api",
            task_api::router(state.config.task_queue.max_payload_bytes),
        )
        .nest(
            "/api",
            upload_api::router(state.config.upload.max_part_bytes),
        )
        .nest(
            "/api",
            jobs_api::router(state.config.upload.max_single_bytes),
        )
        .nest("/api", admin_api::router())
        // `/f/*` 挂在根上，不带 `/api` 前缀：管理接口与静态资源共用一个域，
        // 靠路径前缀区分，这样 Cloudflare 只需一条 Tunnel、一套缓存规则。
        .merge(static_api::router())
        .split_for_parts();

    let mut router = router.with_state(state);

    if docs_enabled {
        // 文档与调试页只在开关打开时才存在。关掉时这两个路径返回 404，
        // 而不是返回「已禁用」——不给外部确认服务里有 Swagger 的机会。
        router = router.merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", api));
    }

    // `/api/*` 与文档页一律 `no-store`：前者含凭证与实时状态，后者是调试页，
    // 缓存了会让人对着旧接口结构调半天。静态路径的缓存头由 handler 自己给，
    // 所以这里只在**没有设过** Cache-Control 时才补。
    let router = router.layer(axum::middleware::from_fn(no_store_for_api));

    // 中间件栈用 ServiceBuilder 组装，**顺序很重要**：
    //
    // 1. `SetRequestId` 必须最外层，否则后面两层看不到 request id；
    // 2. `TraceLayer` 在它之后，建 span 时才能读到头；
    // 3. `PropagateRequestId` 最内层，负责把请求上的 id 抄到响应头。
    //
    // 注意 `ServiceBuilder` 是「先写的在外层」，而 axum 的 `Router::layer`
    // 链式调用是「后写的在外层」——顺序相反。写成一层 ServiceBuilder 是为了
    // 让代码顺序与执行顺序一致，避免下次改动时又搞反。
    let middleware = ServiceBuilder::new()
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
                let request_id = req
                    .headers()
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                tracing::info_span!(
                    "http",
                    method = %req.method(),
                    path = %req.uri().path(),
                    request_id = %request_id,
                )
            }),
        )
        .layer(PropagateRequestIdLayer::x_request_id());

    // CORS 放最外层：`Cors<S>` 要求内层响应体实现 `Default`，TraceLayer 包出来的
    // 响应体类型不满足，所以不能夹在 ServiceBuilder 栈中间，只能整个包在最外面。
    //
    // 前后端不同源（如本地开发 :5173 打 :3823），且要带 Cookie/凭证，所以不能用
    // `Any` origin——那和 allow_credentials 互斥，浏览器会拒绝预检。用
    // mirror_request 把请求带的 Origin/Method/Header 原样镀回去，等价于「全部放行」。
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_methods(tower_http::cors::AllowMethods::mirror_request())
        .allow_headers(tower_http::cors::AllowHeaders::mirror_request())
        .allow_credentials(true)
        // 不 expose 的话，响应头对浏览器端 JS 不可见（预检不管这个，是响应侧的白名单）。
        // `x-request-id` 是 PropagateRequestIdLayer 加的，前端要能读到才能配合日志排查。
        .expose_headers([
            axum::http::HeaderName::from_static("x-request-id"),
            axum::http::header::ETAG,
        ]);

    router.layer(middleware).layer(cors)
}

/// 给 `/api/*` 与文档页补上 `Cache-Control: no-store`。
///
/// 为什么用中间件而不是在每个 handler 里写：接口会越来越多，漏一个就意味着
/// 一个带凭证的响应被 CDN 缓存下来。中间件是「默认安全」的做法。
///
/// 已经设过 `Cache-Control` 的响应不覆盖——静态分发的 handler 自己给长缓存，
/// 那是有意的。
async fn no_store_for_api(
    req: Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_owned();
    let mut resp = next.run(req).await;

    let needs_no_store = path.starts_with("/api") || path.starts_with("/swagger-ui");
    if needs_no_store
        && !resp
            .headers()
            .contains_key(axum::http::header::CACHE_CONTROL)
    {
        resp.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
    }
    resp
}
