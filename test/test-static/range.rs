//! S6 Range 请求测试。
//!
//! 全部区间读取都与内存原始切片**逐字节比对**。算错一个字节，播放器拖进度条会
//! 花屏或直接报错，而症状离原因很远——所以这里不看长度，看内容。

#[path = "../common/mod.rs"]
mod common;

const MB4: u64 = 4 << 20;

/// 建画布 + 上传一个文件，返回 `(slug, path, 原始内容)`。
async fn fixture(app: &common::TestApp, token: &str, size: usize) -> (String, Vec<u8>) {
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

    let body = common::pseudo_content(size, 4242);
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{id}/files/final.mp4"),
            body.clone(),
            token,
        )
        .await;
    assert_eq!(r.status(), 200, "上传应成功");

    (slug, body)
}

/// 发一个带 Range 的请求。
async fn get_range(
    app: &common::TestApp,
    slug: &str,
    range: Option<&str>,
) -> (reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>) {
    let mut req = app.client.get(app.url(&format!("/f/{slug}/final.mp4")));
    if let Some(r) = range {
        req = req.header(reqwest::header::RANGE, r);
    }
    let resp = req.send().await.expect("请求失败");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.bytes().await.expect("读 body").to_vec();
    (status, headers, body)
}

// ---------------------------------------------------------------------------
// 逐字节正确性
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 分块文件的各种区间逐字节正确() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    // 30MiB = 7×4MB + 2MB，共 8 块，末块短块。
    let size = 31_457_280usize;
    let (slug, body) = fixture(&app, &t, size).await;

    let cases: &[(&str, u64, u64, &str)] = &[
        ("bytes=0-0", 0, 0, "首字节"),
        ("bytes=0-99", 0, 99, "开头 100 字节"),
        ("bytes=4194303-4194304", MB4 - 1, MB4, "跨块边界两字节"),
        ("bytes=4194304-8388607", MB4, 2 * MB4 - 1, "第 1 块整块"),
        (
            "bytes=8388608-12582911",
            8_388_608,
            12_582_911,
            "第 2 块整块",
        ),
        ("bytes=4194294-8388617", MB4 - 10, 2 * MB4 + 9, "跨三块"),
        (
            "bytes=29360128-31457279",
            7 * MB4,
            size as u64 - 1,
            "整个末块（2MB 短块）",
        ),
        (
            "bytes=31457279-31457279",
            size as u64 - 1,
            size as u64 - 1,
            "末字节",
        ),
        ("bytes=0-", 0, size as u64 - 1, "从 0 到末尾"),
        (
            "bytes=-500",
            size as u64 - 500,
            size as u64 - 1,
            "末尾 500 字节",
        ),
    ];

    for &(header, want_start, want_end, why) in cases {
        let (status, h, got) = get_range(&app, &slug, Some(header)).await;
        assert_eq!(status, 206, "{why}（{header}）应返回 206");

        let want = &body[want_start as usize..=want_end as usize];
        assert_eq!(got.len(), want.len(), "{why} 长度不对");
        assert_eq!(got, want, "{why}（{header}）内容与原始切片不一致");

        // Content-Range 必须精确，播放器靠它算总时长与可 seek 范围。
        assert_eq!(
            h.get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            Some(format!("bytes {want_start}-{want_end}/{size}").as_str()),
            "{why} 的 Content-Range 不对"
        );
        assert_eq!(
            h.get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some(want.len().to_string().as_str()),
            "{why} 的 Content-Length 不对"
        );
    }
}

#[tokio::test]
async fn 单块文件的区间逐字节正确() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, body) = fixture(&app, &t, 100_000).await;

    for (header, start, end) in [
        ("bytes=0-0", 0usize, 0usize),
        ("bytes=0-999", 0, 999),
        ("bytes=50000-50099", 50_000, 50_099),
        ("bytes=99999-99999", 99_999, 99_999),
        ("bytes=-100", 99_900, 99_999),
    ] {
        let (status, _, got) = get_range(&app, &slug, Some(header)).await;
        assert_eq!(status, 206, "{header}");
        assert_eq!(got, &body[start..=end], "{header} 内容不一致");
    }
}

#[tokio::test]
async fn 全量读取内容完整() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, body) = fixture(&app, &t, 12 * 1024 * 1024).await;

    let (status, h, got) = get_range(&app, &slug, None).await;
    assert_eq!(status, 200, "没带 Range 应返回 200");
    assert_eq!(got.len(), body.len());
    assert_eq!(
        blake3::hash(&got).as_bytes(),
        blake3::hash(&body).as_bytes(),
        "全量读取的内容应与上传的完全一致"
    );
    assert!(
        h.get(reqwest::header::CONTENT_RANGE).is_none(),
        "200 响应不该带 Content-Range"
    );
}

// ---------------------------------------------------------------------------
// 边界与错误
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 末尾越界按截断处理() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, body) = fixture(&app, &t, 1000).await;

    // 播放器常发 `bytes=0-<很大的数>` 探测文件大小，不能报错。
    let (status, h, got) = get_range(&app, &slug, Some("bytes=0-999999")).await;
    assert_eq!(status, 206);
    assert_eq!(got, body);
    assert_eq!(
        h.get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some("bytes 0-999/1000")
    );
}

#[tokio::test]
async fn 起点越界返回416且带content_range() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, _) = fixture(&app, &t, 1000).await;

    for header in ["bytes=1000-", "bytes=1000-2000", "bytes=5000-6000"] {
        let (status, h, _) = get_range(&app, &slug, Some(header)).await;
        assert_eq!(status, 416, "{header} 应返回 416");
        // 416 必须带 `bytes */{size}`，否则客户端不知道该退到哪。
        assert_eq!(
            h.get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
            Some("bytes */1000"),
            "{header} 的 416 响应必须带 Content-Range: bytes */size"
        );
    }
}

#[tokio::test]
async fn 多区间返回416() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, _) = fixture(&app, &t, 1000).await;

    // 多区间要返回 multipart/byteranges，复杂度与收益不匹配，主流播放器也不用。
    // 返回 416 是诚实的答复——比静默返回全量好，后者会让客户端以为拿到了它要的段。
    for header in ["bytes=0-99,200-299", "bytes=0-,-100"] {
        let (status, _, _) = get_range(&app, &slug, Some(header)).await;
        assert_eq!(status, 416, "{header} 应返回 416");
    }
}

#[tokio::test]
async fn 语法不合法的range被忽略() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, body) = fixture(&app, &t, 1000).await;

    // 按 RFC 9110，不合法的 Range 头应当被忽略，返回 200 全量。
    for header in ["items=0-99", "bytes=abc-def", "bytes=-", "0-99"] {
        let (status, _, got) = get_range(&app, &slug, Some(header)).await;
        assert_eq!(status, 200, "{header} 应被忽略并返回 200");
        assert_eq!(got, body, "{header} 应返回全量内容");
    }
}

#[tokio::test]
async fn 空文件的range返回416() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;

    let r = app
        .post_json_auth("/api/projects", serde_json::json!({ "name": "项目" }), &t)
        .await;
    let v: serde_json::Value = r.json().await.expect("JSON");
    let id = v["canvases"][0]["id"].as_str().expect("有").to_owned();
    let slug = v["canvases"][0]["slug"].as_str().expect("有").to_owned();
    app.put_bytes_auth(
        &format!("/api/canvases/{id}/files/empty.txt"),
        Vec::new(),
        &t,
    )
    .await;

    // 全量读空文件是 200 + 0 字节。
    let r = app.get(&format!("/f/{slug}/empty.txt")).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.expect("body").len(), 0);

    // 但任何区间都无法满足。
    let r = app
        .client
        .get(app.url(&format!("/f/{slug}/empty.txt")))
        .header(reqwest::header::RANGE, "bytes=0-")
        .send()
        .await
        .expect("请求");
    assert_eq!(r.status(), 416);
}

// ---------------------------------------------------------------------------
// 条件请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn if_range不匹配时返回全量() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, body) = fixture(&app, &t, 1000).await;

    // ==== 为什么必须忽略 Range ====
    // If-Range 不匹配意味着客户端手上那份内容已经变了。给它拼接一段新内容
    // 会拼出一个**坏文件**，而客户端不会察觉。所以要返回 200 全量让它重下。
    let r = app
        .client
        .get(app.url(&format!("/f/{slug}/final.mp4")))
        .header(reqwest::header::RANGE, "bytes=0-99")
        .header(reqwest::header::IF_RANGE, "\"stale-etag\"")
        .send()
        .await
        .expect("请求");
    assert_eq!(r.status(), 200, "If-Range 不匹配应返回 200 全量");
    assert_eq!(r.bytes().await.expect("body").to_vec(), body);
}

#[tokio::test]
async fn if_range匹配时range正常生效() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, body) = fixture(&app, &t, 1000).await;

    let etag = app
        .get(&format!("/f/{slug}/final.mp4"))
        .await
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("应有 ETag")
        .to_owned();

    let r = app
        .client
        .get(app.url(&format!("/f/{slug}/final.mp4")))
        .header(reqwest::header::RANGE, "bytes=0-99")
        .header(reqwest::header::IF_RANGE, &etag)
        .send()
        .await
        .expect("请求");
    assert_eq!(r.status(), 206);
    assert_eq!(r.bytes().await.expect("body").to_vec(), &body[0..=99]);
}

// ---------------------------------------------------------------------------
// 内存峰值
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 并发range读取不随文件大小增长内存() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let size = 31_457_280usize;
    let (slug, body) = fixture(&app, &t, size).await;

    // ==== 峰值内存 = 并发数 × 4MB，与文件大小无关 ====
    // 这是「逐块读取」而不是「整文件读出再切」的直接结果。如果实现退化成后者，
    // 20 个并发读 30MB 文件会瞬间吃掉 600MB。
    //
    // 这里没法在测试里可靠地测 RSS，所以退一步：验证 20 个并发各取一个不同区间，
    // 每个都拿到正确的字节。实现若是「整文件载入」，结果依然正确但内存爆掉，
    // 所以这条测试守的是正确性；内存峰值由 9.4 节的手工验收覆盖。
    let mut tasks = Vec::new();
    for i in 0..20u64 {
        let client = app.client.clone();
        let url = app.url(&format!("/f/{slug}/final.mp4"));
        let start = i * 1_048_576;
        let end = start + 65_535;
        tasks.push(tokio::spawn(async move {
            let r = client
                .get(url)
                .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
                .send()
                .await
                .expect("请求");
            assert_eq!(r.status(), 206);
            (start, end, r.bytes().await.expect("body").to_vec())
        }));
    }

    for task in tasks {
        let (start, end, got) = task.await.expect("任务完成");
        assert_eq!(
            got,
            &body[start as usize..=end as usize],
            "并发读 {start}-{end} 的内容不对"
        );
    }
}

#[tokio::test]
async fn head请求不带body但头一致() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (slug, body) = fixture(&app, &t, 100_000).await;

    let get = app.get(&format!("/f/{slug}/final.mp4")).await;
    let get_headers = get.headers().clone();
    assert_eq!(get.bytes().await.expect("body").len(), body.len());

    let head = app
        .client
        .head(app.url(&format!("/f/{slug}/final.mp4")))
        .send()
        .await
        .expect("请求");
    assert_eq!(head.status(), 200);

    // HEAD 的头必须与 GET 一致（含 Content-Length），否则客户端拿 HEAD
    // 探测出来的大小是错的。
    for name in [
        reqwest::header::CONTENT_TYPE,
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::ETAG,
        reqwest::header::ACCEPT_RANGES,
        reqwest::header::CACHE_CONTROL,
    ] {
        assert_eq!(
            head.headers().get(&name),
            get_headers.get(&name),
            "HEAD 与 GET 的 {name} 应一致"
        );
    }
    assert_eq!(
        head.bytes().await.expect("body").len(),
        0,
        "HEAD 不该有 body"
    );
}
