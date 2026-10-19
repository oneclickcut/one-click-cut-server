//! S0 接口冒烟测试：服务起得来、`/healthz` 可用、docs 开关真的生效。
//!
//! 这个文件同时是 `spawn_app` fixture 的第一个使用者——后面 S3~S8 的接口测试
//! 全都建立在它上面，所以这里先把它跑通。

#[path = "../common/mod.rs"]
mod common;

#[tokio::test]
async fn healthz_返回版本与运行时长() {
    let app = common::spawn_app().await;

    let resp = app.get("/healthz").await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("响应应是 JSON");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        body["started_at"].as_u64().is_some_and(|t| t > 0),
        "started_at 应是正的 Unix 秒: {body}"
    );
    assert!(
        body["uptime_seconds"].as_u64().is_some(),
        "uptime_seconds 应存在: {body}"
    );
}

#[tokio::test]
async fn 请求id回写到响应头() {
    let app = common::spawn_app().await;

    // 不带 x-request-id 时由中间件生成一个。
    let resp = app.get("/healthz").await;
    assert!(
        resp.headers().contains_key("x-request-id"),
        "响应应带 x-request-id，日志串联依赖它"
    );

    // 带了就原样透传，方便跨服务串联同一个请求。
    let resp = app
        .client
        .get(app.url("/healthz"))
        .header("x-request-id", "trace-me-42")
        .send()
        .await
        .expect("请求失败");
    assert_eq!(
        resp.headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("trace-me-42"),
        "调用方传入的 request id 应被透传"
    );
}

#[tokio::test]
async fn docs开启时可访问文档() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;

    let resp = app.get("/api-docs/openapi.json").await;
    assert_eq!(resp.status(), 200, "openapi.json 应可访问");

    let doc: serde_json::Value = resp.json().await.expect("应是 JSON");
    assert!(
        doc["paths"]["/healthz"]["get"].is_object(),
        "healthz 应出现在 openapi 文档里（注解与实现同源）"
    );
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    for (path, methods) in doc["paths"].as_object().expect("paths 应是对象") {
        for (method, operation) in methods.as_object().expect("路径项应是对象") {
            if !["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                continue;
            }
            assert!(
                operation["summary"]
                    .as_str()
                    .is_some_and(|summary| !summary.trim().is_empty()),
                "{method} {path} 缺少 Swagger UI 操作说明"
            );
        }
    }

    let resp = app.get("/swagger-ui/").await;
    assert_eq!(resp.status(), 200, "Swagger UI 页面应可访问");
}

#[tokio::test]
async fn docs关闭时返回404() {
    let app = common::spawn_app_with(|c| c.docs.enabled = false).await;

    // 关掉时这些路径应当**不存在**，而不是返回「已禁用」——
    // 不给外部确认服务里有 Swagger 的机会。生产环境依赖这条。
    for path in ["/api-docs/openapi.json", "/swagger-ui", "/swagger-ui/"] {
        let resp = app.get(path).await;
        assert_eq!(resp.status(), 404, "{path} 在 docs 关闭时应返回 404");
    }

    // 业务接口不受开关影响。
    assert_eq!(app.get("/healthz").await.status(), 200);
}

#[tokio::test]
async fn 未注册路径返回404() {
    let app = common::spawn_app().await;
    assert_eq!(app.get("/nope").await.status(), 404);
    assert_eq!(app.get("/api/nope").await.status(), 404);
    // `/f` 前缀要等 S6 才有实现。
    assert_eq!(app.get("/f/k7Qm3xB9pLdR/x.jpg").await.status(), 404);
}

#[tokio::test]
async fn 已实现的受保护路径返回401而不是404() {
    let app = common::spawn_app().await;

    // 存在但未认证 → 401；不存在 → 404。两者要分得开，否则排查问题时
    // 无法区分「路由没挂上」与「凭证不对」。
    assert_eq!(app.get("/api/me").await.status(), 401);
}
