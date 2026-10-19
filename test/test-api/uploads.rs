//! S7 分块上传与 tar 批量上传测试。
//!
//! 分块上传最关键的一条：**乱序上传后 complete，拼出来的内容必须与原文件
//! 逐字节一致**。拼错顺序的文件在播放到那一段时才暴露，症状离原因极远。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::{Cf, keys};

const MB4: usize = 4 << 20;
const PART: usize = 16 << 20;

/// 建画布，返回 `(id, slug)`。
async fn mk_canvas(app: &common::TestApp, token: &str) -> (String, String) {
    let r = app
        .post_json_auth(
            "/api/projects",
            serde_json::json!({ "name": "项目" }),
            token,
        )
        .await;
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.expect("JSON");
    (
        v["canvases"][0]["id"].as_str().expect("有 id").to_owned(),
        v["canvases"][0]["slug"]
            .as_str()
            .expect("有 slug")
            .to_owned(),
    )
}

async fn project_for_canvas(app: &common::TestApp, user: &str, canvas: &str) -> String {
    let projects: serde_json::Value = app
        .get_auth("/api/projects", user)
        .await
        .json()
        .await
        .unwrap();
    projects["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| {
            p["canvases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["id"] == canvas)
        })
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// 初始化一次分块上传，返回 `(upload_id, part_size)`。
async fn initiate(
    app: &common::TestApp,
    token: &str,
    canvas: &str,
    path: &str,
    part_size: Option<u32>,
) -> (String, usize) {
    let mut body = serde_json::json!({ "path": path });
    if let Some(ps) = part_size {
        body["part_size"] = serde_json::json!(ps);
    }
    let r = app
        .post_json_auth(&format!("/api/canvases/{canvas}/uploads"), body, token)
        .await;
    assert_eq!(r.status(), 201, "初始化应成功");
    let v: serde_json::Value = r.json().await.expect("JSON");
    (
        v["upload_id"].as_str().expect("有 upload_id").to_owned(),
        v["part_size"].as_u64().expect("有 part_size") as usize,
    )
}

/// 上传一个分片。
async fn put_part(
    app: &common::TestApp,
    token: &str,
    canvas: &str,
    upload: &str,
    part_no: usize,
    data: &[u8],
) -> reqwest::Response {
    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/uploads/{upload}/{part_no}"),
        data.to_vec(),
        token,
    )
    .await
}

/// complete。
async fn complete(
    app: &common::TestApp,
    token: &str,
    canvas: &str,
    upload: &str,
) -> reqwest::Response {
    app.post_json_auth(
        &format!("/api/canvases/{canvas}/uploads/{upload}/complete"),
        serde_json::json!({}),
        token,
    )
    .await
}

// ---------------------------------------------------------------------------
// 分块上传主流程
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 顺序上传三个分片并完成() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t).await;
    let (upload, part_size) = initiate(&app, &t, &canvas, "final.mp4", None).await;
    assert_eq!(part_size, PART, "默认 16MB");

    // 两个满片 + 一个短片，总 40MB。
    let body = common::pseudo_content(2 * PART + 8 * 1024 * 1024, 7);
    let parts: Vec<&[u8]> = body.chunks(PART).collect();
    assert_eq!(parts.len(), 3);

    for (i, p) in parts.iter().enumerate() {
        let r = put_part(&app, &t, &canvas, &upload, i, p).await;
        assert_eq!(r.status(), 200, "分片 {i} 应收下");
        let v: serde_json::Value = r.json().await.expect("JSON");
        assert_eq!(v["part_no"], i);
        assert_eq!(v["size"], p.len());
        assert_eq!(v["duplicate"], false);
        // 一个 16MB 的 part 切成 4 个 4MB 块。
        assert_eq!(v["chunks"], p.len().div_ceil(MB4));
    }

    let r = complete(&app, &t, &canvas, &upload).await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["size"], body.len());
    assert_eq!(v["path"], "final.mp4");
    assert_eq!(v["replaced"], false);

    // 整文件哈希由服务端从落盘的块算出，应与原文件一致。
    let hash = v["hash"].as_str().expect("有 hash");
    assert_eq!(hash, blake3::hash(&body).to_hex().as_str());

    // 从静态路径读回来逐字节比对——这一条同时验证了块顺序、拼装与 Range 基础。
    let got = app
        .get(&format!("/f/{slug}/final.mp4"))
        .await
        .bytes()
        .await
        .expect("body")
        .to_vec();
    assert_eq!(got.len(), body.len());
    assert_eq!(got, body, "读回内容应与上传的逐字节一致");
}

#[tokio::test]
async fn 乱序上传后拼装顺序正确() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t).await;
    // 用 4MB 的 part（正好一个块），方便造多个分片。
    let (upload, part_size) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;
    assert_eq!(part_size, MB4);

    let body = common::pseudo_content(5 * MB4 + 1234, 11);
    let parts: Vec<&[u8]> = body.chunks(MB4).collect();
    assert_eq!(parts.len(), 6);

    // ==== 乱序是分块上传的常态 ====
    // 客户端可能并发传多个分片，到达顺序不可控。拼装必须按**分片号**而不是
    // 到达顺序——`stage` 里的分片号是 4 字节大端，前缀扫描天然有序。
    for i in [3usize, 0, 5, 1, 4, 2] {
        let r = put_part(&app, &t, &canvas, &upload, i, parts[i]).await;
        assert_eq!(r.status(), 200, "分片 {i}");
    }

    let r = complete(&app, &t, &canvas, &upload).await;
    assert_eq!(r.status(), 200);

    let got = app
        .get(&format!("/f/{slug}/v.mp4"))
        .await
        .bytes()
        .await
        .expect("body")
        .to_vec();
    assert_eq!(got, body, "乱序上传后拼装顺序必须正确");
}

#[tokio::test]
async fn 重传同一分片是幂等的() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;

    let body = common::pseudo_content(2 * MB4, 13);
    let parts: Vec<&[u8]> = body.chunks(MB4).collect();

    put_part(&app, &t, &canvas, &upload, 0, parts[0]).await;
    // 网络抖动导致的重试是常态。
    let r = put_part(&app, &t, &canvas, &upload, 0, parts[0]).await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["duplicate"], true, "应报告是重传");

    put_part(&app, &t, &canvas, &upload, 1, parts[1]).await;
    assert_eq!(complete(&app, &t, &canvas, &upload).await.status(), 200);

    let got = app
        .get(&format!("/f/{slug}/v.mp4"))
        .await
        .bytes()
        .await
        .expect("body")
        .to_vec();
    assert_eq!(got, body, "重传不该让块列表出现重复");
}

#[tokio::test]
async fn 进度查询支持断点续传() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;

    let body = common::pseudo_content(4 * MB4, 17);
    let parts: Vec<&[u8]> = body.chunks(MB4).collect();

    // 只传 0 和 2，模拟中断。
    put_part(&app, &t, &canvas, &upload, 0, parts[0]).await;
    put_part(&app, &t, &canvas, &upload, 2, parts[2]).await;

    let r = app
        .get_auth(&format!("/api/canvases/{canvas}/uploads/{upload}"), &t)
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["path"], "v.mp4");
    assert_eq!(v["part_size"], MB4);
    // 已收到的分片号升序返回，客户端据此只补缺的那些。
    assert_eq!(v["parts"], serde_json::json!([0, 2]));
    assert_eq!(v["bytes_received"], 2 * MB4);
}

#[tokio::test]
async fn 缺分片时complete报出缺哪些() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;

    let body = common::pseudo_content(4 * MB4, 19);
    let parts: Vec<&[u8]> = body.chunks(MB4).collect();
    // 传 0、2、3，缺 1。
    for i in [0usize, 2, 3] {
        put_part(&app, &t, &canvas, &upload, i, parts[i]).await;
    }

    let r = complete(&app, &t, &canvas, &upload).await;
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = r.json().await.expect("JSON");
    // 报出具体缺哪几个，客户端只补那几个而不是全部重传。
    assert!(
        v["message"].as_str().expect("有").contains('1'),
        "错误信息应指出缺分片 1: {v}"
    );
}

#[tokio::test]
async fn 一个分片都没传就complete被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", None).await;

    let r = complete(&app, &t, &canvas, &upload).await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn 中间分片短于约定被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;

    // ==== 只有最后一个分片可以短 ====
    // 中间分片短了，拼出来的文件会在中间缺字节，而且后续块的偏移全部错位——
    // 这种损坏只有在播放到那一段时才暴露。
    put_part(
        &app,
        &t,
        &canvas,
        &upload,
        0,
        &common::pseudo_content(1000, 1),
    )
    .await;
    put_part(
        &app,
        &t,
        &canvas,
        &upload,
        1,
        &common::pseudo_content(MB4, 2),
    )
    .await;

    let r = complete(&app, &t, &canvas, &upload).await;
    assert_eq!(r.status(), 400, "分片 0 短于 part_size，应拒绝 complete");
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert!(
        v["message"].as_str().expect("有").contains("最后一个"),
        "错误信息应说明只有最后一个分片可以短: {v}"
    );
}

#[tokio::test]
async fn 分片超过约定大小被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;

    let r = put_part(
        &app,
        &t,
        &canvas,
        &upload,
        0,
        &common::pseudo_content(MB4 + 1, 3),
    )
    .await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn 空分片被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", None).await;

    let r = put_part(&app, &t, &canvas, &upload, 0, b"").await;
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn part_size必须是块大小整数倍() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    // ==== 对齐要求 ====
    // 每个 part 独立切块落盘，所以 part 必须是 4MB 的整数倍——否则第一个 part
    // 的末尾会产生一个短块，而它在文件中间，破坏「offset / 4MB 索引到块」
    // 这个 Range 读取的基础。
    for bad in [1000u32, 5 << 20, (4 << 20) + 1] {
        let r = app
            .post_json_auth(
                &format!("/api/canvases/{canvas}/uploads"),
                serde_json::json!({ "path": "v.mp4", "part_size": bad }),
                &t,
            )
            .await;
        assert_eq!(r.status(), 400, "part_size={bad} 应被拒");
    }

    // 4MB 的整数倍都可以。
    for ok in [4u32 << 20, 8 << 20, 16 << 20, 48 << 20] {
        let r = app
            .post_json_auth(
                &format!("/api/canvases/{canvas}/uploads"),
                serde_json::json!({ "path": "v.mp4", "part_size": ok }),
                &t,
            )
            .await;
        assert_eq!(r.status(), 201, "part_size={ok} 应被接受");
    }
}

#[tokio::test]
async fn 非法路径在初始化时就被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    // 早失败：不要等传了几十 MB 分片才发现路径不合规。
    for bad in ["../escape.mp4", "a//b.mp4", "", &"a".repeat(600)] {
        let r = app
            .post_json_auth(
                &format!("/api/canvases/{canvas}/uploads"),
                serde_json::json!({ "path": bad }),
                &t,
            )
            .await;
        assert_eq!(r.status(), 400, "路径 {bad:?} 应在初始化时就被拒");
    }
}

#[tokio::test]
async fn 覆盖已有文件() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t).await;

    // 先用单文件 PUT 放一份。
    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/v.mp4"),
        b"old small content".to_vec(),
        &t,
    )
    .await;

    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;
    let body = common::pseudo_content(MB4 + 100, 23);
    for (i, p) in body.chunks(MB4).enumerate() {
        put_part(&app, &t, &canvas, &upload, i, p).await;
    }

    let r = complete(&app, &t, &canvas, &upload).await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["replaced"], true);

    let got = app
        .get(&format!("/f/{slug}/v.mp4"))
        .await
        .bytes()
        .await
        .expect("body")
        .to_vec();
    assert_eq!(got, body);

    // 文件数不该增加。
    let listed: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{canvas}/files"), &t)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(listed["files"].as_array().expect("有").len(), 1);
}

#[tokio::test]
async fn 分块上传的内容可做range读取() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "final.mp4", Some(MB4 as u32)).await;

    let body = common::pseudo_content(3 * MB4 + 5000, 29);
    for (i, p) in body.chunks(MB4).enumerate() {
        put_part(&app, &t, &canvas, &upload, i, p).await;
    }
    assert_eq!(complete(&app, &t, &canvas, &upload).await.status(), 200);

    // ==== 这条把 S7 与 S6 接起来 ====
    // 分块上传写出的块布局必须与单文件 PUT 完全一致，否则 Range 读取会错位。
    // part 与 chunk 的对齐要求就是为了这一点。
    for (start, end) in [
        (0usize, 99usize),
        (MB4 - 1, MB4),
        (MB4, 2 * MB4 - 1),
        (2 * MB4 - 10, 2 * MB4 + 9),
        (3 * MB4, body.len() - 1),
        (body.len() - 1, body.len() - 1),
    ] {
        let r = app
            .client
            .get(app.url(&format!("/f/{slug}/final.mp4")))
            .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
            .send()
            .await
            .expect("请求");
        assert_eq!(r.status(), 206, "{start}-{end}");
        let got = r.bytes().await.expect("body").to_vec();
        assert_eq!(got, &body[start..=end], "{start}-{end} 内容不一致");
    }
}

// ---------------------------------------------------------------------------
// abort 与过期
// ---------------------------------------------------------------------------

#[tokio::test]
async fn abort后会话失效且块成孤儿() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;

    put_part(
        &app,
        &t,
        &canvas,
        &upload,
        0,
        &common::pseudo_content(MB4, 31),
    )
    .await;

    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}/uploads/{upload}"), &t)
        .await;
    assert_eq!(r.status(), 204);

    // 会话与分片记录都该清掉。
    let stage = app
        .store
        .scan_prefix_keys(Cf::Stage, Vec::new(), usize::MAX)
        .await
        .expect("扫描");
    assert!(stage.is_empty(), "stage 应清空，残留 {} 条", stage.len());

    // 会话没了，再传分片或 complete 都是 404。
    assert_eq!(
        put_part(&app, &t, &canvas, &upload, 1, b"x").await.status(),
        404
    );
    assert_eq!(complete(&app, &t, &canvas, &upload).await.status(), 404);

    // ==== 已写入的块不删 ====
    // 它们可能被别的文件引用（内容寻址）。没人引用的那些由 GC 的孤儿扫描回收，
    // 这比在 abort 里判断引用关系简单且更安全。
    let audit = one_click_cut_server::storage::gc::audit(&app.store, usize::MAX)
        .await
        .expect("对账");
    assert_eq!(audit.orphan_chunks.len(), 1, "abort 留下的块应被识别为孤儿");

    let n = one_click_cut_server::storage::gc::purge_orphans(&app.store, &audit.orphan_chunks)
        .await
        .expect("清理");
    assert_eq!(n, 1);
}

#[tokio::test]
async fn 过期会话不可继续使用() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;
    let (upload, _) = initiate(&app, &t, &canvas, "v.mp4", Some(MB4 as u32)).await;
    put_part(
        &app,
        &t,
        &canvas,
        &upload,
        0,
        &common::pseudo_content(MB4, 37),
    )
    .await;

    // 把会话创建时间推到 25 小时前。
    let upload_id = keys::UploadId::parse(&upload).expect("解析");
    let raw = app
        .store
        .get(Cf::Stage, keys::stage_info_key(upload_id))
        .await
        .expect("get")
        .expect("应存在");
    let mut info: one_click_cut_server::storage::records::StageInfo =
        one_click_cut_server::storage::records::decode(&raw).expect("解码");
    info.created_at = 1;
    app.store
        .put(
            Cf::Stage,
            keys::stage_info_key(upload_id),
            one_click_cut_server::storage::records::encode(&info).expect("编码"),
        )
        .await
        .expect("写回");

    assert_eq!(
        put_part(&app, &t, &canvas, &upload, 1, b"x").await.status(),
        404,
        "过期会话不该继续收分片"
    );
    assert_eq!(complete(&app, &t, &canvas, &upload).await.status(), 404);

    // 后台清理任务能收走它。
    let n = one_click_cut_server::domain::upload::purge_expired(
        &app.store,
        one_click_cut_server::util::now_secs(),
        100,
    )
    .await
    .expect("清理");
    assert_eq!(n, 1);
    let stage = app
        .store
        .scan_prefix_keys(Cf::Stage, Vec::new(), usize::MAX)
        .await
        .expect("扫描");
    assert!(stage.is_empty());
}

#[tokio::test]
async fn 不存在的会话返回404() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    let ghost = "01JBXY7Z3K8QW5N2M4P6R8T0V1";
    assert_eq!(
        put_part(&app, &t, &canvas, ghost, 0, b"x").await.status(),
        404
    );
    assert_eq!(complete(&app, &t, &canvas, ghost).await.status(), 404);
    assert_eq!(
        app.get_auth(&format!("/api/canvases/{canvas}/uploads/{ghost}"), &t)
            .await
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// agent token 走分块上传
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent可用分块上传() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t).await;

    let v: serde_json::Value = app
        .post_json_auth(
            &format!(
                "/api/projects/{}/agent-tokens",
                project_for_canvas(&app, &t, &canvas).await
            ),
            serde_json::json!({}),
            &t,
        )
        .await
        .json()
        .await
        .expect("JSON");
    let agent = v["token"].as_str().expect("有").to_owned();

    // 这是 agent 传成片的真实路径：分块上传通道。
    let (upload, _) = initiate(&app, &agent, &canvas, "final.mp4", Some(MB4 as u32)).await;
    let body = common::pseudo_content(2 * MB4, 41);
    for (i, p) in body.chunks(MB4).enumerate() {
        let r = put_part(&app, &agent, &canvas, &upload, i, p).await;
        assert_eq!(r.status(), 200);
    }
    assert_eq!(complete(&app, &agent, &canvas, &upload).await.status(), 200);

    let got = app
        .get(&format!("/f/{slug}/final.mp4"))
        .await
        .bytes()
        .await
        .expect("body")
        .to_vec();
    assert_eq!(got, body);
}

#[tokio::test]
async fn agent不能给别的画布开上传会话() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (bound, _) = mk_canvas(&app, &t).await;
    let (other, _) = mk_canvas(&app, &t).await;

    let v: serde_json::Value = app
        .post_json_auth(
            &format!(
                "/api/projects/{}/agent-tokens",
                project_for_canvas(&app, &t, &bound).await
            ),
            serde_json::json!({}),
            &t,
        )
        .await
        .json()
        .await
        .expect("JSON");
    let agent = v["token"].as_str().expect("有").to_owned();

    let r = app
        .post_json_auth(
            &format!("/api/canvases/{other}/uploads"),
            serde_json::json!({ "path": "v.mp4" }),
            &agent,
        )
        .await;
    assert_eq!(r.status(), 404);
}

// ---------------------------------------------------------------------------
// tar 批量上传
// ---------------------------------------------------------------------------

/// 手工拼 tar，绕过 `tar::Builder` 的安全检查与路径规范化。
///
/// 它自己就拒绝 `..`、还会把 `a//b` 规范化成 `a/b`，所以用它造不出恶意归档。
/// 真实攻击者不会用这个 crate 打包，所以必须手工构造头部来验证解析侧挡得住。
fn raw_tar(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, data) in files {
        let mut block = [0u8; 512];
        let name = path.as_bytes();
        assert!(name.len() < 100, "测试只造短路径");
        block[..name.len()].copy_from_slice(name);
        block[100..107].copy_from_slice(b"0000644");
        block[108..115].copy_from_slice(b"0000000");
        block[116..123].copy_from_slice(b"0000000");
        block[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        block[136..147].copy_from_slice(b"00000000000");
        block[156] = b'0';
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        block[148..156].copy_from_slice(b"        ");
        let sum: u32 = block.iter().map(|b| *b as u32).sum();
        block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

        out.extend_from_slice(&block);
        out.extend_from_slice(data);
        out.extend(std::iter::repeat_n(0u8, (512 - data.len() % 512) % 512));
    }
    // 归档结束：两个全零块。
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

/// 造一个 tar。
fn make_tar(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, data.as_slice())
            .expect("写 tar 条目");
    }
    builder.into_inner().expect("完成 tar")
}

#[tokio::test]
async fn tar批量写入三百五十个文件() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    // ==== 批量接口是必需的，不是优化 ====
    // 350 次 HTTP 往返光 TLS 与 RTT 就是几十秒。
    let files: Vec<(String, Vec<u8>)> = (0..350)
        .map(|i| {
            (
                format!("scene{:02}/f{:03}.jpg", i / 50, i),
                common::pseudo_content(2048, i as u64),
            )
        })
        .collect();
    let tar = make_tar(&files);

    let started = std::time::Instant::now();
    let r = app
        .post_bytes_auth(
            &format!("/api/canvases/{canvas}/files:batch"),
            tar,
            "application/x-tar",
            &t,
        )
        .await;
    let elapsed = started.elapsed();
    assert_eq!(r.status(), 200);

    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["written"], 350);
    assert_eq!(v["bytes"], 350 * 2048);
    println!("350 文件 tar 批量上传耗时 {elapsed:?}");

    let listed: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{canvas}/files"), &t)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(listed["files"].as_array().expect("有").len(), 350);
}

#[tokio::test]
async fn tar里的内容逐字节正确() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, slug) = mk_canvas(&app, &t).await;

    let files: Vec<(String, Vec<u8>)> = vec![
        ("a.jpg".into(), common::pseudo_content(5000, 1)),
        ("scene01/f001.jpg".into(), common::pseudo_content(3000, 2)),
        ("timeline.json".into(), br#"{"scenes":[1,2,3]}"#.to_vec()),
    ];
    let r = app
        .post_bytes_auth(
            &format!("/api/canvases/{canvas}/files:batch"),
            make_tar(&files),
            "application/x-tar",
            &t,
        )
        .await;
    assert_eq!(r.status(), 200);

    for (path, want) in &files {
        let got = app
            .get(&format!("/f/{slug}/{path}"))
            .await
            .bytes()
            .await
            .expect("body")
            .to_vec();
        assert_eq!(&got, want, "{path} 内容不一致");
    }
}

#[tokio::test]
async fn tar里含非法路径整批拒绝() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    // ==== 整批原子 ====
    // 第 N 个条目路径非法时，前面那些也不该落盘——否则 agent 得去猜
    // 「哪几个成功了」。
    //
    // 必须手工拼 tar 头：`tar::Builder` 自己就拒绝 `..`、还会把 `a//b` 规范化成
    // `a/b`，用它造不出恶意归档。而**真实攻击者不会用这个 crate 打包**。
    let tar = raw_tar(&[
        ("ok1.jpg", b"fine".as_slice()),
        ("../escape.jpg", b"bad".as_slice()),
        ("ok2.jpg", b"fine".as_slice()),
    ]);
    let r = app
        .post_bytes_auth(
            &format!("/api/canvases/{canvas}/files:batch"),
            tar,
            "application/x-tar",
            &t,
        )
        .await;
    assert_eq!(r.status(), 400, "含穿越路径的 tar 应整批拒绝");

    let listed: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{canvas}/files"), &t)
        .await
        .json()
        .await
        .expect("JSON");
    assert!(
        listed["files"].as_array().expect("有").is_empty(),
        "整批拒绝时一个文件都不该写入"
    );
}

#[tokio::test]
async fn 空tar与损坏的tar被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    for body in [
        make_tar(&[]),
        b"not a tar at all".to_vec(),
        vec![0xffu8; 1024],
    ] {
        let r = app
            .post_bytes_auth(
                &format!("/api/canvases/{canvas}/files:batch"),
                body,
                "application/x-tar",
                &t,
            )
            .await;
        assert_eq!(r.status(), 400, "空或损坏的 tar 应被拒");
    }
}

#[tokio::test]
async fn tar批量命中去重() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    // 三个不同路径同一份内容：agent 反复用同一张底图是常态。
    let same = common::pseudo_content(10_000, 99);
    let files: Vec<(String, Vec<u8>)> = (0..3)
        .map(|i| (format!("f{i}.jpg"), same.clone()))
        .collect();

    let r = app
        .post_bytes_auth(
            &format!("/api/canvases/{canvas}/files:batch"),
            make_tar(&files),
            "application/x-tar",
            &t,
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["written"], 3);
    assert_eq!(v["deduplicated"], 2, "后两个应命中去重");
}

#[tokio::test]
async fn agent可用tar批量上传() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    let v: serde_json::Value = app
        .post_json_auth(
            &format!(
                "/api/projects/{}/agent-tokens",
                project_for_canvas(&app, &t, &canvas).await
            ),
            serde_json::json!({}),
            &t,
        )
        .await
        .json()
        .await
        .expect("JSON");
    let agent = v["token"].as_str().expect("有").to_owned();

    let files: Vec<(String, Vec<u8>)> = (0..20)
        .map(|i| (format!("f{i:03}.jpg"), common::pseudo_content(512, i)))
        .collect();
    let r = app
        .post_bytes_auth(
            &format!("/api/canvases/{canvas}/files:batch"),
            make_tar(&files),
            "application/x-tar",
            &agent,
        )
        .await;
    assert_eq!(r.status(), 200, "这是 agent 的主要上传通道");
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["written"], 20);
}

#[tokio::test]
async fn 没有write位的agent不能批量上传() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let (canvas, _) = mk_canvas(&app, &t).await;

    let v: serde_json::Value = app
        .post_json_auth(
            &format!(
                "/api/projects/{}/agent-tokens",
                project_for_canvas(&app, &t, &canvas).await
            ),
            serde_json::json!({ "canvas_files_write": false }),
            &t,
        )
        .await
        .json()
        .await
        .expect("JSON");
    let agent = v["token"].as_str().expect("有").to_owned();

    let r = app
        .post_bytes_auth(
            &format!("/api/canvases/{canvas}/files:batch"),
            make_tar(&[("a.jpg".into(), b"x".to_vec())]),
            "application/x-tar",
            &agent,
        )
        .await;
    assert_eq!(r.status(), 403);
}

#[tokio::test]
async fn 上传接口都在openapi文档里() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let doc: serde_json::Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .expect("JSON");

    for (path, method) in [
        ("/api/canvases/{id}/uploads", "post"),
        ("/api/canvases/{id}/uploads/{upload_id}", "get"),
        ("/api/canvases/{id}/uploads/{upload_id}", "delete"),
        ("/api/canvases/{id}/uploads/{upload_id}/complete", "post"),
        ("/api/canvases/{id}/uploads/{upload_id}/{part_no}", "put"),
        ("/api/canvases/{id}/files:batch", "post"),
    ] {
        assert!(
            doc["paths"][path][method].is_object(),
            "{method} {path} 应出现在文档里"
        );
    }
}
