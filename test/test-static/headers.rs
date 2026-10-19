//! S6 静态分发的响应头与鉴权测试。
//!
//! 最重要的一条是**读路径上没有任何鉴权**：不带凭证就该拿到内容。
//! 这是 v2 相比 v1 取消访问票后的核心语义变化，必须被测试钉死。

#[path = "../common/mod.rs"]
mod common;

/// 建画布 + 上传若干文件，返回 `(canvas_id, slug)`。
async fn fixture(app: &common::TestApp, token: &str) -> (String, String) {
    let r = app
        .post_json_auth(
            "/api/projects",
            serde_json::json!({ "name": "项目" }),
            token,
        )
        .await;
    let v: serde_json::Value = r.json().await.expect("JSON");
    let id = v["canvases"][0]["id"].as_str().expect("有 id").to_owned();
    let slug = v["canvases"][0]["slug"]
        .as_str()
        .expect("有 slug")
        .to_owned();

    for (path, body) in [
        ("timeline.json", br#"{"scenes":[]}"#.to_vec()),
        ("scene01/f001.jpg", b"jpeg-bytes".to_vec()),
        ("final.mp4", b"mp4-bytes".to_vec()),
        (
            "sub.srt",
            b"1\n00:00:00,000 --> 00:00:01,000\nhi\n".to_vec(),
        ),
    ] {
        let r = app
            .put_bytes_auth(&format!("/api/canvases/{id}/files/{path}"), body, token)
            .await;
        assert_eq!(r.status(), 200, "{path} 上传应成功");
    }
    (id, slug)
}

fn header(r: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<&str> {
    r.headers().get(name).and_then(|v| v.to_str().ok())
}

// ---------------------------------------------------------------------------
// 读取公开
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 无凭证即可读取() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    // ==== 这是 v2 最核心的语义 ====
    // 读取一律公开：不校验凭证、不换票、不心跳。读路径上不该有任何鉴权代码。
    for path in ["timeline.json", "scene01/f001.jpg", "final.mp4", "sub.srt"] {
        let r = app.get(&format!("/f/{slug}/{path}")).await;
        assert_eq!(r.status(), 200, "{path} 不带凭证应可读");
    }
}

#[tokio::test]
async fn private画布也能公开读取() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (id, slug) = fixture(&app, &t).await;

    // 画布默认就是 private。显式再设一次，确认 visibility **完全不参与读取判断**——
    // 它只影响可发现性（是否出现在公开列表与分享入口）。
    app.patch_json_auth(
        &format!("/api/canvases/{id}"),
        serde_json::json!({ "visibility": "private" }),
        &t,
    )
    .await;

    let r = app.get(&format!("/f/{slug}/final.mp4")).await;
    assert_eq!(
        r.status(),
        200,
        "private 画布靠 slug 不可枚举保护，不靠拒绝访问"
    );
}

#[tokio::test]
async fn 别的用户也能读() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    app.signup("bob", "password123").await;
    let (_, slug) = fixture(&app, &alice).await;

    // 读取公开的意思就是「谁都能读」，包括别的登录用户与匿名访问者。
    // 跨用户隔离只针对写入面（`/api/*`）。
    let r = app.get(&format!("/f/{slug}/final.mp4")).await;
    assert_eq!(r.status(), 200);
}

// ---------------------------------------------------------------------------
// 响应头
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 必需的响应头齐全() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    let r = app.get(&format!("/f/{slug}/final.mp4")).await;
    assert_eq!(r.status(), 200);

    assert_eq!(header(&r, reqwest::header::CONTENT_TYPE), Some("video/mp4"));
    assert_eq!(header(&r, reqwest::header::CONTENT_LENGTH), Some("9"));
    assert_eq!(
        header(&r, reqwest::header::ACCEPT_RANGES),
        Some("bytes"),
        "没这个头，有些播放器根本不会尝试拖进度条"
    );

    // ETag 用内容哈希：同内容永远同 ETag，跨机器也一致。
    let etag = header(&r, reqwest::header::ETAG).expect("应有 ETag");
    assert!(
        etag.starts_with('"') && etag.ends_with('"'),
        "ETag 应带引号"
    );
    assert_eq!(etag.len(), 66, "应是 64 字符 hex 加两个引号");

    // CORS 中间件可加入 Origin 相关 Vary，但静态内容不能随凭证变化。
    assert!(
        !header(&r, reqwest::header::VARY)
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("authorization")
    );
}

#[tokio::test]
async fn etag等于内容哈希() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (id, slug) = fixture(&app, &t).await;

    let files: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{id}/files"), &t)
        .await
        .json()
        .await
        .expect("JSON");
    let entry = files["files"]
        .as_array()
        .expect("有")
        .iter()
        .find(|f| f["path"] == "final.mp4")
        .expect("应有 final.mp4");
    let hash = entry["hash"].as_str().expect("有 hash");

    let r = app.get(&format!("/f/{slug}/final.mp4")).await;
    assert_eq!(
        header(&r, reqwest::header::ETAG),
        Some(format!("\"{hash}\"").as_str()),
        "ETag 应就是内容哈希"
    );
}

#[tokio::test]
async fn 缓存头矩阵() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let t = app.signup("alice", "password123").await;
    let (id, slug) = fixture(&app, &t).await;

    // 带 `?v=` → 永久缓存。URL 带内容哈希，内容变了 URL 就变，所以安全。
    let r = app.get(&format!("/f/{slug}/final.mp4?v=deadbeef")).await;
    assert_eq!(
        header(&r, reqwest::header::CACHE_CONTROL),
        Some("public, max-age=31536000, immutable")
    );

    // 不带 `?v=` → 只给一分钟。没有版本参数就没法保证内容变了 URL 也变。
    let r = app.get(&format!("/f/{slug}/final.mp4")).await;
    assert_eq!(
        header(&r, reqwest::header::CACHE_CONTROL),
        Some("public, max-age=60")
    );

    // manifest 是新鲜度入口，必须能很快反映画布变化。
    let r = app.get(&format!("/f/{slug}/manifest.json")).await;
    assert_eq!(
        header(&r, reqwest::header::CACHE_CONTROL),
        Some("public, max-age=30")
    );

    // `/api/*` 含凭证与实时状态，绝不缓存。
    let r = app.get_auth(&format!("/api/canvases/{id}/files"), &t).await;
    assert_eq!(header(&r, reqwest::header::CACHE_CONTROL), Some("no-store"));
    let r = app.get_auth("/api/me", &t).await;
    assert_eq!(header(&r, reqwest::header::CACHE_CONTROL), Some("no-store"));

    // 文档页也不缓存，否则会对着旧接口结构调半天。
    let r = app.get("/swagger-ui/").await;
    assert_eq!(header(&r, reqwest::header::CACHE_CONTROL), Some("no-store"));
}

#[tokio::test]
async fn 版本参数的值不参与判断() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    // ==== 这是一条明确的语义选择 ====
    // `?v=` 只参与 CDN 缓存键，服务端始终返回当前内容。这样覆盖写之后
    // 旧链接不会 404，只是会拿到新内容（CDN 上的旧条目按 LRU 自然淘汰）。
    for v in ["deadbeef", "00000000", "wrong", ""] {
        let r = app.get(&format!("/f/{slug}/final.mp4?v={v}")).await;
        assert_eq!(r.status(), 200, "?v={v} 也该返回当前内容");
        assert_eq!(r.bytes().await.expect("body").to_vec(), b"mp4-bytes");
    }
}

#[tokio::test]
async fn mime按扩展名给() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    for (path, want) in [
        ("final.mp4", "video/mp4"),
        ("scene01/f001.jpg", "image/jpeg"),
        ("timeline.json", "application/json"),
    ] {
        let r = app.get(&format!("/f/{slug}/{path}")).await;
        assert_eq!(
            header(&r, reqwest::header::CONTENT_TYPE),
            Some(want),
            "{path}"
        );
    }

    // 文本类必须带 charset，否则浏览器会按 latin-1 解中文。
    let r = app.get(&format!("/f/{slug}/sub.srt")).await;
    assert!(
        header(&r, reqwest::header::CONTENT_TYPE)
            .expect("有")
            .contains("charset=utf-8"),
        "文本类应带 charset"
    );
}

// ---------------------------------------------------------------------------
// 条件请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn if_none_match命中返回304() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    let first = app.get(&format!("/f/{slug}/final.mp4")).await;
    let etag = header(&first, reqwest::header::ETAG)
        .expect("有")
        .to_owned();

    let r = app
        .client
        .get(app.url(&format!("/f/{slug}/final.mp4")))
        .header(reqwest::header::IF_NONE_MATCH, &etag)
        .send()
        .await
        .expect("请求");
    assert_eq!(r.status(), 304);
    assert_eq!(
        header(&r, reqwest::header::ETAG),
        Some(etag.as_str()),
        "304 也要带 ETag"
    );
    assert_eq!(r.bytes().await.expect("body").len(), 0, "304 不该有 body");
}

#[tokio::test]
async fn if_none_match不匹配返回200() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    let r = app
        .client
        .get(app.url(&format!("/f/{slug}/final.mp4")))
        .header(reqwest::header::IF_NONE_MATCH, "\"some-other-etag\"")
        .send()
        .await
        .expect("请求");
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.expect("body").to_vec(), b"mp4-bytes");
}

#[tokio::test]
async fn 覆盖内容后etag变化() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (id, slug) = fixture(&app, &t).await;

    let etag_before = header(
        &app.get(&format!("/f/{slug}/final.mp4")).await,
        reqwest::header::ETAG,
    )
    .expect("有")
    .to_owned();

    app.put_bytes_auth(
        &format!("/api/canvases/{id}/files/final.mp4"),
        b"different bytes".to_vec(),
        &t,
    )
    .await;

    let after = app.get(&format!("/f/{slug}/final.mp4")).await;
    let etag_after = header(&after, reqwest::header::ETAG)
        .expect("有")
        .to_owned();
    assert_ne!(etag_before, etag_after, "内容变了 ETag 必须变");

    // 用旧 ETag 发条件请求应该拿到 200 + 新内容，而不是 304。
    let r = app
        .client
        .get(app.url(&format!("/f/{slug}/final.mp4")))
        .header(reqwest::header::IF_NONE_MATCH, &etag_before)
        .send()
        .await
        .expect("请求");
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.expect("body").to_vec(), b"different bytes");
}

// ---------------------------------------------------------------------------
// 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 不存在的slug或文件返回404() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    // slug 格式合法但不存在。
    assert_eq!(app.get("/f/aaaaaaaaaaaa/final.mp4").await.status(), 404);
    // 文件不存在。
    assert_eq!(app.get(&format!("/f/{slug}/nope.mp4")).await.status(), 404);
    // slug 格式不合法——与不存在同一个结果，不暴露格式细节。
    // 太短、太长、含非 base62 字符，三种都走同一条路。
    for bad in ["short", "aaaaaaaaaaaaa", "k7Qm3xB9pLd!"] {
        assert_eq!(
            app.get(&format!("/f/{bad}/final.mp4")).await.status(),
            404,
            "非法 slug `{bad}` 应与不存在同一个结果"
        );
    }
}

#[tokio::test]
async fn 删掉文件后立即404() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (id, slug) = fixture(&app, &t).await;

    assert_eq!(app.get(&format!("/f/{slug}/final.mp4")).await.status(), 200);

    app.delete_auth(&format!("/api/canvases/{id}/files/final.mp4"), &t)
        .await;

    // 文件条目删了就立即 404，即使内容还在 GC 的延迟窗口里没被回收。
    assert_eq!(app.get(&format!("/f/{slug}/final.mp4")).await.status(), 404);
}

#[tokio::test]
async fn 删掉画布后slug立即失效() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (id, slug) = fixture(&app, &t).await;

    app.delete_auth(&format!("/api/canvases/{id}"), &t).await;

    assert_eq!(app.get(&format!("/f/{slug}/final.mp4")).await.status(), 404);
    assert_eq!(
        app.get(&format!("/f/{slug}/manifest.json")).await.status(),
        404
    );
}

// ---------------------------------------------------------------------------
// 公开 manifest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 公开manifest与鉴权版同内容() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (id, slug) = fixture(&app, &t).await;

    let public: serde_json::Value = app
        .get(&format!("/f/{slug}/manifest.json"))
        .await
        .json()
        .await
        .expect("JSON");
    let private: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{id}/manifest"), &t)
        .await
        .json()
        .await
        .expect("JSON");

    // 两条路径返回同一份契约，区别只是需不需要凭证。
    assert_eq!(public, private);
}

#[tokio::test]
async fn manifest里的url可直接拼接访问() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (_, slug) = fixture(&app, &t).await;

    let m: serde_json::Value = app
        .get(&format!("/f/{slug}/manifest.json"))
        .await
        .json()
        .await
        .expect("JSON");
    let base_url = m["base_url"].as_str().expect("有 base_url").to_owned();

    // ==== 播放器契约的核心 ====
    // 播放器拼 `base_url + files[].url` 就能取到资源，不需要知道任何别的规则。
    // 这条测试实际走了一遍播放器要做的事。
    for f in m["files"].as_array().expect("有 files") {
        let url = f["url"].as_str().expect("有 url");
        let full = format!("{base_url}{url}");
        let r = app.get(&full).await;
        assert_eq!(r.status(), 200, "{full} 应可访问");
        assert_eq!(
            r.headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("public, max-age=31536000, immutable"),
            "manifest 给出的 URL 都带 ?v=，应拿到长缓存"
        );
        assert_eq!(
            f["size"].as_u64().expect("有 size"),
            r.bytes().await.expect("body").len() as u64,
            "manifest 里的 size 应与实际内容一致"
        );
    }
}

#[tokio::test]
async fn 静态路径在openapi文档里() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let doc: serde_json::Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .expect("JSON");

    let item = &doc["paths"]["/f/{slug}/{path}"]["get"];
    assert!(item.is_object(), "静态分发应在文档里");

    // ==== 读接口不标安全方案 ====
    // 这样「公开可读」这条设计在文档上一眼就能看出来，而不是要去读说明文字。
    assert!(
        item.get("security").is_none(),
        "读接口不该标安全方案，否则会让人以为需要凭证"
    );
}
