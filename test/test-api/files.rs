//! S5 画布内文件接口测试。
//!
//! 两条要特别注意：
//!
//! 1. **多段 path 必须能用**（`scene01/f001.jpg`）——这是 agent 上传产物的常态，
//!    而 axum 的 `{path}` 只匹配一段，很容易写成 404；
//! 2. 画布统计（`file_count` / `bytes`）是增量维护的，覆盖与删除都要算对，
//!    否则配额会慢慢漂移。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::{Cf, keys};

/// 建一个画布，返回 `(id, slug)`。
async fn mk_canvas(app: &common::TestApp, token: &str, name: &str) -> (String, String) {
    let r = app
        .post_json_auth("/api/projects", serde_json::json!({ "name": name }), token)
        .await;
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.expect("JSON");
    let id = v["canvases"][0]["id"].as_str().expect("有 id");
    assert_eq!(
        app.patch_json_auth(
            &format!("/api/canvases/{id}"),
            serde_json::json!({"name":name}),
            token
        )
        .await
        .status(),
        200
    );
    (
        v["canvases"][0]["id"].as_str().expect("有 id").to_owned(),
        v["canvases"][0]["slug"]
            .as_str()
            .expect("有 slug")
            .to_owned(),
    )
}

/// 上传一个文件。
async fn put(
    app: &common::TestApp,
    token: &str,
    canvas: &str,
    path: &str,
    body: &[u8],
) -> reqwest::Response {
    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/{path}"),
        body.to_vec(),
        token,
    )
    .await
}

/// 列出文件。
async fn list(app: &common::TestApp, token: &str, canvas: &str) -> Vec<serde_json::Value> {
    let r = app
        .get_auth(&format!("/api/canvases/{canvas}/files"), token)
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    v["files"].as_array().expect("有 files").clone()
}

/// 读画布节点，拿统计字段。
async fn canvas_stats(app: &common::TestApp, token: &str, canvas: &str) -> (u64, u64) {
    let v: serde_json::Value = app
        .get_auth("/api/projects", token)
        .await
        .json()
        .await
        .expect("JSON");
    for p in v["items"].as_array().expect("projects") {
        for c in p["canvases"].as_array().expect("canvases") {
            if c["id"] == canvas {
                return (
                    c["file_count"].as_u64().unwrap(),
                    c["bytes"].as_u64().unwrap(),
                );
            }
        }
    }
    panic!("应找到画布 {canvas}")
}

// ---------------------------------------------------------------------------
// 上传
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 上传单文件返回哈希与url() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    let body = b"hello world";
    let r = put(&app, &t, &canvas, "a.jpg", body).await;
    assert_eq!(r.status(), 200);

    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["path"], "a.jpg");
    assert_eq!(v["size"], body.len());
    assert_eq!(v["replaced"], false);
    assert_eq!(v["deduplicated"], false);

    // 哈希是完整 64 字符 hex；url 的 `?v=` 取它的前 8 位。
    let hash = v["hash"].as_str().expect("有 hash");
    assert_eq!(hash.len(), 64);
    assert_eq!(v["url"], format!("a.jpg?v={}", &hash[..8]));
}

#[tokio::test]
async fn 多段路径可用且不产生目录实体() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // ==== agent 上传产物的常态 ====
    // axum 的 `{path}` 只匹配一段，必须用 `{*path}` 通配，否则这些全 404。
    for path in [
        "a.jpg",
        "scene01/f001.jpg",
        "scene01/scene02/deep.png",
        "a/b/c/d/e/f/g/h.srt",
    ] {
        let r = put(&app, &t, &canvas, path, b"x").await;
        assert_eq!(r.status(), 200, "{path} 应可上传");
    }

    let files = list(&app, &t, &canvas).await;
    assert_eq!(files.len(), 4);

    // 画布内没有真实的目录实体：`meta` 列族里只有 4 条 `f|` 记录，
    // 没有任何「目录」记录。
    let entries = app
        .store
        .scan_prefix_keys(
            Cf::Meta,
            keys::file_prefix(keys::CanvasId::parse(&canvas).expect("解析")),
            usize::MAX,
        )
        .await
        .expect("扫描");
    assert_eq!(entries.len(), 4, "应只有 4 条文件条目，没有目录记录");
}

#[tokio::test]
async fn 列表按路径字典序() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // 乱序上传，列表应按字典序返回——manifest 直接用这个顺序，不用再排。
    for path in [
        "timeline.json",
        "scene02/f001.jpg",
        "final.mp4",
        "scene01/f001.jpg",
    ] {
        put(&app, &t, &canvas, path, b"x").await;
    }

    let paths: Vec<&str> = list(&app, &t, &canvas)
        .await
        .iter()
        .map(|f| f["path"].as_str().expect("有"))
        .collect::<Vec<_>>()
        .iter()
        .map(|s| Box::leak(s.to_string().into_boxed_str()) as &str)
        .collect();
    assert_eq!(
        paths,
        vec![
            "final.mp4",
            "scene01/f001.jpg",
            "scene02/f001.jpg",
            "timeline.json"
        ]
    );
}

#[tokio::test]
async fn 非法路径逐条被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // 这里测的是**走完整 HTTP 之后没有脏 path 落盘**，而不是「服务端一定返回 400」。
    //
    // 原因：`./a.jpg`、`../x.jpg` 这类形态会被 HTTP 客户端在发送前就按 RFC 3986
    // 规范化掉，服务端根本收不到原始形态。所以逐条的校验规则由
    // `storage::keys::validate_path` 的单元测试覆盖（`test/test-storage/keys.rs`
    // 的「非法路径逐条被拒」），这里只守住结果：**一个文件都不该写进去**。
    let cases: &[(&str, &str)] = &[("a//b.jpg", "空段"), ("a/", "以斜杠结尾")];
    for (path, why) in cases {
        let r = put(&app, &t, &canvas, path, b"x").await;
        assert_ne!(r.status(), 200, "{path:?}（{why}）不该被接受");
    }

    // 超长与超段数不会被客户端规范化，必定走到我们的校验，返回 400。
    let long = "a".repeat(600);
    assert_eq!(put(&app, &t, &canvas, &long, b"x").await.status(), 400);
    let deep = (0..20).map(|_| "a").collect::<Vec<_>>().join("/");
    assert_eq!(put(&app, &t, &canvas, &deep, b"x").await.status(), 400);

    assert!(list(&app, &t, &canvas).await.is_empty(), "不该写进任何文件");
}

#[tokio::test]
async fn 原始报文里的穿越路径也被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // ==== 绕过客户端规范化，直接发原始报文 ====
    // 真实攻击者不会用规范化 URL 的 HTTP 客户端，所以要确认服务端自己挡得住。
    // 这里手写 TCP 请求，把 `..` 原样送到线上。
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let addr = app.base_url.trim_start_matches("http://").to_owned();
    for raw_path in [
        "../../../etc/passwd",
        "a/../../escape.jpg",
        "./dot.jpg",
        "%2e%2e%2fescape.jpg",
        // 反斜杠也要在这里测：HTTP 客户端按 WHATWG URL 规范会把它转成 `/`，
        // 所以在客户端层面根本送不出去。
        "a\\b.jpg",
    ] {
        let mut sock = tokio::net::TcpStream::connect(&addr).await.expect("连接");
        let req = format!(
            "PUT /api/canvases/{canvas}/files/{raw_path} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Authorization: Bearer {t}\r\n\
             Content-Length: 1\r\n\
             Connection: close\r\n\r\nx"
        );
        sock.write_all(req.as_bytes()).await.expect("发送");
        let mut resp = String::new();
        sock.read_to_string(&mut resp).await.expect("读响应");

        let first_line = resp.lines().next().unwrap_or("");
        assert!(
            !first_line.contains(" 200 "),
            "原始路径 {raw_path:?} 不该被接受，响应: {first_line}"
        );
    }

    assert!(
        list(&app, &t, &canvas).await.is_empty(),
        "穿越路径不该写进任何文件"
    );
}

#[tokio::test]
async fn 覆盖同路径更新哈希与统计() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    let r = put(&app, &t, &canvas, "a.jpg", b"first content").await;
    let v1: serde_json::Value = r.json().await.expect("JSON");
    let (count, bytes) = canvas_stats(&app, &t, &canvas).await;
    assert_eq!(count, 1);
    assert_eq!(bytes, 13);

    let r = put(&app, &t, &canvas, "a.jpg", b"second content is longer").await;
    assert_eq!(r.status(), 200);
    let v2: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v2["replaced"], true);
    assert_ne!(v2["hash"], v1["hash"], "内容变了哈希必须变");

    // 覆盖不增加条目数，字节数按差值调整——统计是增量维护的，算错会让配额漂移。
    let (count, bytes) = canvas_stats(&app, &t, &canvas).await;
    assert_eq!(count, 1, "覆盖不该增加文件数");
    assert_eq!(bytes, 24);
    assert_eq!(list(&app, &t, &canvas).await.len(), 1);
}

#[tokio::test]
async fn 同内容不同路径命中去重() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    let body = common::pseudo_content(100_000, 7);
    let r1 = put(&app, &t, &canvas, "a.jpg", &body).await;
    let v1: serde_json::Value = r1.json().await.expect("JSON");
    assert_eq!(v1["deduplicated"], false);

    let r2 = put(&app, &t, &canvas, "b.jpg", &body).await;
    let v2: serde_json::Value = r2.json().await.expect("JSON");
    assert_eq!(v2["deduplicated"], true, "第二次应命中去重");
    assert_eq!(v1["hash"], v2["hash"]);

    // 两条条目共享一份内容，但统计要算两份字节——配额按逻辑大小算，
    // 不按物理占用算，否则用户无法预测自己还能存多少。
    let (count, bytes) = canvas_stats(&app, &t, &canvas).await;
    assert_eq!(count, 2);
    assert_eq!(bytes, 200_000);
}

#[tokio::test]
async fn 重复上传完全相同的内容与路径() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // agent 重试是常态：同 path 同内容写两次，结果必须与写一次一致。
    put(&app, &t, &canvas, "a.jpg", b"same").await;
    let r = put(&app, &t, &canvas, "a.jpg", b"same").await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["replaced"], true);
    assert_eq!(v["deduplicated"], true);

    let (count, bytes) = canvas_stats(&app, &t, &canvas).await;
    assert_eq!(count, 1);
    assert_eq!(bytes, 4);

    // 引用计数要正好是 1：加一次减一次抵消，不能变成 2 也不能归零。
    let hash = keys::ContentHash::parse_hex(v["hash"].as_str().expect("有")).expect("解析");
    assert_eq!(
        app.store
            .counter(Cf::BlobMeta, keys::blob_ref_key(hash))
            .await
            .expect("读"),
        1,
        "同哈希覆盖后引用计数应仍为 1"
    );
}

#[tokio::test]
async fn 大文件上传走分块且可读回() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // 12MB → 3 个 4MB 块。
    let body = common::pseudo_content(12 * 1024 * 1024, 3);
    let r = put(&app, &t, &canvas, "final.mp4", &body).await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["size"], body.len());

    let hash = keys::ContentHash::parse_hex(v["hash"].as_str().expect("有")).expect("解析");
    let back = one_click_cut_server::storage::blob::read_all(&app.store, hash)
        .await
        .expect("读")
        .expect("应存在");
    assert_eq!(
        blake3::hash(&back).as_bytes(),
        blake3::hash(&body).as_bytes(),
        "读回内容应与上传的一致"
    );
}

#[tokio::test]
async fn 空文件可上传() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    let r = put(&app, &t, &canvas, "empty.txt", b"").await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["size"], 0);
    assert_eq!(list(&app, &t, &canvas).await.len(), 1);
}

#[tokio::test]
async fn mime按扩展名推导() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    for path in ["a.jpg", "b.png", "c.mp4", "d.json", "e.srt", "f.unknown"] {
        put(&app, &t, &canvas, path, b"x").await;
    }

    let files = list(&app, &t, &canvas).await;
    let by_path: std::collections::HashMap<&str, &str> = files
        .iter()
        .map(|f| {
            (
                f["path"].as_str().expect("有"),
                f["mime"].as_str().expect("有"),
            )
        })
        .collect();
    assert_eq!(by_path["a.jpg"], "image/jpeg");
    assert_eq!(by_path["b.png"], "image/png");
    assert_eq!(by_path["c.mp4"], "video/mp4");
    assert_eq!(by_path["d.json"], "application/json");
    assert!(by_path["e.srt"].contains("charset=utf-8"));
    assert_eq!(by_path["f.unknown"], "application/octet-stream");
}

#[tokio::test]
async fn 文本与二进制分流到不同列族() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    let r = put(&app, &t, &canvas, "a.json", br#"{"k":"v"}"#).await;
    let v: serde_json::Value = r.json().await.expect("JSON");
    let txt_hash = keys::ContentHash::parse_hex(v["hash"].as_str().expect("有")).expect("解析");

    let r = put(&app, &t, &canvas, "b.jpg", &common::pseudo_content(2048, 9)).await;
    let v: serde_json::Value = r.json().await.expect("JSON");
    let bin_hash = keys::ContentHash::parse_hex(v["hash"].as_str().expect("有")).expect("解析");

    // json 走 blob_txt（开 zstd），jpg 走 blob_bin（不压缩）。
    assert!(
        app.store
            .get(Cf::BlobTxt, keys::content_key(txt_hash))
            .await
            .expect("get")
            .is_some(),
        "json 应落在 blob_txt"
    );
    assert!(
        app.store
            .get(Cf::BlobBin, keys::content_key(bin_hash))
            .await
            .expect("get")
            .is_some(),
        "jpg 应落在 blob_bin"
    );
}

// ---------------------------------------------------------------------------
// 删除
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 删单文件并更新统计() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    put(&app, &t, &canvas, "a.jpg", b"aaa").await;
    put(&app, &t, &canvas, "b.jpg", b"bbbbb").await;

    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}/files/a.jpg"), &t)
        .await;
    assert_eq!(r.status(), 204);

    let files = list(&app, &t, &canvas).await;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "b.jpg");

    let (count, bytes) = canvas_stats(&app, &t, &canvas).await;
    assert_eq!(count, 1);
    assert_eq!(bytes, 5);
}

#[tokio::test]
async fn 删不存在的单文件返回404() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // 单文件删除调用方明确指定了一个文件，删不到就该报 404。
    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}/files/nope.jpg"), &t)
        .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn 批量删除跳过不存在的路径() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    for p in ["a.jpg", "b.jpg", "c.jpg"] {
        put(&app, &t, &canvas, p, b"x").await;
    }

    // ==== 批量删除的语义 ====
    // 调用方通常在做「清理这些路径」，其中一部分已经不在了不该让整批失败。
    // 所以跳过不存在的，返回实际删除的条数。
    let r = app
        .post_json_auth(
            &format!("/api/canvases/{canvas}/files:delete"),
            serde_json::json!({ "paths": ["a.jpg", "nope.jpg", "c.jpg", "../bad"] }),
            &t,
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["deleted"], 2, "只有 a.jpg 与 c.jpg 存在");

    let files = list(&app, &t, &canvas).await;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "b.jpg");
}

#[tokio::test]
async fn 批量删除空列表是空操作() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;
    put(&app, &t, &canvas, "a.jpg", b"x").await;

    let r = app
        .post_json_auth(
            &format!("/api/canvases/{canvas}/files:delete"),
            serde_json::json!({ "paths": [] }),
            &t,
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["deleted"], 0);
    assert_eq!(list(&app, &t, &canvas).await.len(), 1);
}

#[tokio::test]
async fn 删文件后引用归零进gc队列() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    let r = put(&app, &t, &canvas, "a.jpg", b"unique content here").await;
    let v: serde_json::Value = r.json().await.expect("JSON");
    let hash = keys::ContentHash::parse_hex(v["hash"].as_str().expect("有")).expect("解析");

    app.delete_auth(&format!("/api/canvases/{canvas}/files/a.jpg"), &t)
        .await;

    // 引用归零 → 进待删队列，延迟一小时（避开正在传输中的响应）。
    // 内容此刻仍在，这是有意的。
    assert_eq!(
        app.store
            .counter(Cf::BlobMeta, keys::blob_ref_key(hash))
            .await
            .expect("读"),
        0
    );
    let queued = app
        .store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Blob), usize::MAX)
        .await
        .expect("扫描");
    assert_eq!(queued.len(), 1, "归零的内容应进 GC 队列");
    assert!(
        app.store
            .get(Cf::BlobBin, keys::content_key(hash))
            .await
            .expect("get")
            .is_some(),
        "延迟窗口内内容应还在"
    );
}

#[tokio::test]
async fn 共享内容删一个不影响另一个() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    let body = b"shared content";
    put(&app, &t, &canvas, "a.jpg", body).await;
    let r = put(&app, &t, &canvas, "b.jpg", body).await;
    let v: serde_json::Value = r.json().await.expect("JSON");
    let hash = keys::ContentHash::parse_hex(v["hash"].as_str().expect("有")).expect("解析");

    app.delete_auth(&format!("/api/canvases/{canvas}/files/a.jpg"), &t)
        .await;

    // b.jpg 还引用着它，计数应是 1 而不是 0，也不该进 GC 队列。
    assert_eq!(
        app.store
            .counter(Cf::BlobMeta, keys::blob_ref_key(hash))
            .await
            .expect("读"),
        1
    );
    let queued = app
        .store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Blob), usize::MAX)
        .await
        .expect("扫描");
    assert!(queued.is_empty(), "仍被引用的内容不该进 GC 队列");
}

#[tokio::test]
async fn 删画布连带删掉文件条目() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    for i in 0..5 {
        put(&app, &t, &canvas, &format!("f{i}.jpg"), b"x").await;
    }

    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}"), &t)
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["files"], 5, "应报告删了 5 个文件");

    let entries = app
        .store
        .scan_prefix_keys(
            Cf::Meta,
            keys::file_prefix(keys::CanvasId::parse(&canvas).expect("解析")),
            usize::MAX,
        )
        .await
        .expect("扫描");
    assert!(entries.is_empty(), "文件条目应全部清除");
}

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest契约() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t, "夏日海边").await;

    put(&app, &t, &canvas, "timeline.json", br#"{"scenes":[]}"#).await;
    put(&app, &t, &canvas, "scene01/f001.jpg", b"frame").await;
    put(&app, &t, &canvas, "final.mp4", b"video").await;

    // entry 指向时间轴 json。本服务不解析它的内容。
    app.patch_json_auth(
        &format!("/api/canvases/{canvas}"),
        serde_json::json!({ "entry": "timeline.json" }),
        &t,
    )
    .await;

    let r = app
        .get_auth(&format!("/api/canvases/{canvas}/manifest"), &t)
        .await;
    assert_eq!(r.status(), 200);
    let m: serde_json::Value = r.json().await.expect("JSON");

    assert_eq!(m["canvas_id"], canvas);
    assert_eq!(m["name"], "夏日海边");
    assert_eq!(m["visibility"], "private");
    assert_eq!(m["base_url"], format!("/f/{slug}/"));
    assert_eq!(m["entry"], "timeline.json");

    // 播放器契约里**不该有 expires_at**：manifest 拿到就一直有效，
    // 不需要心跳续期。这是 v2 取消访问票后播放器侧最直接的简化。
    assert!(
        m.get("expires_at").is_none(),
        "manifest 不该有 expires_at: {m}"
    );

    let files = m["files"].as_array().expect("有 files");
    assert_eq!(files.len(), 3);
    // 按路径字典序。
    assert_eq!(files[0]["path"], "final.mp4");
    assert_eq!(files[1]["path"], "scene01/f001.jpg");
    assert_eq!(files[2]["path"], "timeline.json");

    // 每条的 url 都要带 `?v=hash8`——这是缓存正确性的唯一保证。
    for f in files {
        let hash = f["hash"].as_str().expect("有 hash");
        let path = f["path"].as_str().expect("有 path");
        assert_eq!(
            f["url"].as_str().expect("有 url"),
            format!("{path}?v={}", &hash[..8]),
            "url 必须带内容版本"
        );
        assert!(f["size"].as_u64().is_some());
        assert!(!f["mime"].as_str().expect("有").is_empty());
    }
}

#[tokio::test]
async fn 覆盖文件后manifest的版本参数变化() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    put(&app, &t, &canvas, "a.jpg", b"version one").await;
    let m1: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{canvas}/manifest"), &t)
        .await
        .json()
        .await
        .expect("JSON");
    let url1 = m1["files"][0]["url"].as_str().expect("有").to_owned();

    put(&app, &t, &canvas, "a.jpg", b"version two").await;
    let m2: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{canvas}/manifest"), &t)
        .await
        .json()
        .await
        .expect("JSON");
    let url2 = m2["files"][0]["url"].as_str().expect("有").to_owned();

    // ==== 缓存失效完全靠 URL 版本化 ====
    // 覆盖后 hash 变 → `?v=` 变 → CDN 缓存键变 → 自然取新内容，零 purge 成本。
    assert_ne!(url1, url2, "内容变了 URL 必须变，否则 CDN 会一直给旧内容");
    assert!(url1.starts_with("a.jpg?v=") && url2.starts_with("a.jpg?v="));
}

#[tokio::test]
async fn 空画布的manifest() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "空画布").await;

    let r = app
        .get_auth(&format!("/api/canvases/{canvas}/manifest"), &t)
        .await;
    assert_eq!(r.status(), 200);
    let m: serde_json::Value = r.json().await.expect("JSON");
    assert!(m["files"].as_array().expect("有").is_empty());
    assert!(m.get("entry").is_none(), "没设 entry 时该字段应省略");
}

// ---------------------------------------------------------------------------
// 鉴权与隔离
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 文件接口都需要凭证() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    assert_eq!(
        app.get(&format!("/api/canvases/{canvas}/files"))
            .await
            .status(),
        401
    );
    assert_eq!(
        app.get(&format!("/api/canvases/{canvas}/manifest"))
            .await
            .status(),
        401
    );
    let r = app
        .client
        .put(app.url(&format!("/api/canvases/{canvas}/files/a.jpg")))
        .body("x")
        .send()
        .await
        .expect("请求");
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn 跨用户访问画布文件返回404() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    let bob = app.signup("bob", "password123").await;
    let (canvas, _) = mk_canvas(&app, &alice, "alice 的画布").await;
    put(&app, &alice, &canvas, "a.jpg", b"secret").await;

    // 与树接口一致：不存在与不属于你合并成 404。
    assert_eq!(
        app.get_auth(&format!("/api/canvases/{canvas}/files"), &bob)
            .await
            .status(),
        404
    );
    assert_eq!(
        app.get_auth(&format!("/api/canvases/{canvas}/manifest"), &bob)
            .await
            .status(),
        404
    );
    assert_eq!(
        put(&app, &bob, &canvas, "hack.jpg", b"x").await.status(),
        404
    );
    assert_eq!(
        app.delete_auth(&format!("/api/canvases/{canvas}/files/a.jpg"), &bob)
            .await
            .status(),
        404
    );

    // alice 的文件必须完好。
    assert_eq!(list(&app, &alice, &canvas).await.len(), 1);
}

#[tokio::test]
async fn 不存在的画布返回404() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let ghost = "01JBXY7Z3K8QW5N2M4P6R8T0V1";

    assert_eq!(
        app.get_auth(&format!("/api/canvases/{ghost}/files"), &t)
            .await
            .status(),
        404
    );
    assert_eq!(put(&app, &t, ghost, "a.jpg", b"x").await.status(), 404);
    // id 格式不对也当成不存在。
    assert_eq!(
        app.get_auth("/api/canvases/not-a-ulid/files", &t)
            .await
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// 规模与上限
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 三百五十个文件的画布() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t, "画布").await;

    // 一个真实画布约 350 个文件。这里用小内容，只验证条目与 manifest 的规模。
    let started = std::time::Instant::now();
    for i in 0..350 {
        let body = common::pseudo_content(512, i as u64);
        let r = put(
            &app,
            &t,
            &canvas,
            &format!("scene{:02}/f{:03}.jpg", i / 50, i),
            &body,
        )
        .await;
        assert_eq!(r.status(), 200);
    }
    let elapsed = started.elapsed();
    println!(
        "350 个文件逐个 PUT 耗时 {elapsed:?}（{:.0} 文件/秒）",
        350.0 / elapsed.as_secs_f64()
    );

    let m: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{canvas}/manifest"), &t)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(
        m["files"].as_array().expect("有").len(),
        350,
        "manifest 条目应正好 350"
    );

    let (count, _) = canvas_stats(&app, &t, &canvas).await;
    assert_eq!(count, 350);
}

#[tokio::test]
async fn 文件接口都在openapi文档里() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let doc: serde_json::Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .expect("JSON");

    for (path, method) in [
        ("/api/canvases/{id}/files", "get"),
        ("/api/canvases/{id}/manifest", "get"),
        ("/api/canvases/{id}/files:delete", "post"),
        // 这两条的 axum 路由是 `{*path}` 通配，文档里写规范的 `{path}`。
        ("/api/canvases/{id}/files/{path}", "put"),
        ("/api/canvases/{id}/files/{path}", "delete"),
    ] {
        assert!(
            doc["paths"][path][method].is_object(),
            "{method} {path} 应出现在文档里"
        );
    }

    // 文档里不该出现 axum 专用的 `{*path}` 写法。
    let paths: Vec<&str> = doc["paths"]
        .as_object()
        .expect("有 paths")
        .keys()
        .map(|s| s.as_str())
        .collect();
    assert!(
        !paths.iter().any(|p| p.contains('*')),
        "OpenAPI 文档里不该有 `*` 通配写法: {paths:?}"
    );
}
