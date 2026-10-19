//! 画布内目录接口测试（一期：列 / 建 / 删）。
//!
//! 三条要特别注意：
//!
//! 1. **目录树是派生的**：只传一个 `scene01/f001.jpg`、不建任何目录，列根层也要
//!    能看到 `scene01`。老数据兼容全靠这条兜住。
//! 2. **不变量 1**：同一个路径不能同时是文件和目录，两个方向都要拒。
//! 3. **删画布要连带清掉 `fd|` 记录**。这条接口看不出来，必须直接读存储层断言——
//!    漏删的话记录会永久残留，而且没有任何人会发现。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::storage::{Cf, keys};

/// 建一个画布，返回 id。
async fn mk_canvas(app: &common::TestApp, token: &str) -> String {
    let r = app
        .post_json_auth(
            "/api/projects",
            serde_json::json!({ "name": "项目" }),
            token,
        )
        .await;
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.expect("JSON");
    v["canvases"][0]["id"].as_str().expect("有 id").to_owned()
}

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

/// 建目录。
async fn mkdir(app: &common::TestApp, token: &str, canvas: &str, path: &str) -> reqwest::Response {
    app.post_json_auth(
        &format!("/api/canvases/{canvas}/dirs"),
        serde_json::json!({ "path": path }),
        token,
    )
    .await
}

/// 列一层。`prefix` 为空即根层。
async fn entries(
    app: &common::TestApp,
    token: &str,
    canvas: &str,
    prefix: &str,
) -> serde_json::Value {
    let url = if prefix.is_empty() {
        format!("/api/canvases/{canvas}/entries")
    } else {
        format!("/api/canvases/{canvas}/entries?prefix={prefix}")
    };
    let r = app.get_auth(&url, token).await;
    assert_eq!(r.status(), 200, "列 {prefix:?} 应成功");
    r.json().await.expect("JSON")
}

/// 一层里的子目录名。
fn dir_names(v: &serde_json::Value) -> Vec<String> {
    v["dirs"]
        .as_array()
        .expect("有 dirs")
        .iter()
        .map(|d| d["name"].as_str().expect("有 name").to_owned())
        .collect()
}

/// 一层里的文件 path。
fn file_paths(v: &serde_json::Value) -> Vec<String> {
    v["files"]
        .as_array()
        .expect("有 files")
        .iter()
        .map(|f| f["path"].as_str().expect("有 path").to_owned())
        .collect()
}

/// 直接数存储层的目录记录条数。
async fn dir_record_count(app: &common::TestApp, canvas: &str) -> usize {
    app.store
        .scan_prefix_keys(
            Cf::Meta,
            keys::canvas_dir_prefix(keys::CanvasId::parse(canvas).expect("解析")),
            usize::MAX,
        )
        .await
        .expect("扫描")
        .len()
}

// ---------------------------------------------------------------------------
// 建与列
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 建空目录能被列出且幂等() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    let r = mkdir(&app, &t, &canvas, "scene01").await;
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["created"], true);
    assert_eq!(v["dirs_written"], 1);

    // 空目录必须能存在——这是整个方案存在的理由（纯前端按斜杠分组建不出空目录）。
    let root = entries(&app, &t, &canvas, "").await;
    assert_eq!(dir_names(&root), vec!["scene01"]);
    assert_eq!(root["dirs"][0]["file_count"], 0);
    assert_eq!(root["dirs"][0]["bytes"], 0);
    assert!(
        root["dirs"][0]["created_at"].as_u64().is_some(),
        "显式建的目录要有 created_at"
    );
    assert!(file_paths(&root).is_empty());

    // 重复建同一个：幂等返回 200，不报错。目录的身份是路径，重复创建没有歧义。
    let r = mkdir(&app, &t, &canvas, "scene01").await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["created"], false);
    assert_eq!(v["dirs_written"], 0);
    assert_eq!(
        dir_record_count(&app, &canvas).await,
        1,
        "不该写出第二条记录"
    );
}

#[tokio::test]
async fn 只传文件也能派生出目录() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    // 一个目录都没建，只传文件。老数据就是这个形态。
    put(&app, &t, &canvas, "scene01/f001.jpg", b"aaa").await;
    put(&app, &t, &canvas, "scene01/raw/x.png", b"bb").await;
    put(&app, &t, &canvas, "final.mp4", b"c").await;

    let root = entries(&app, &t, &canvas, "").await;
    assert_eq!(dir_names(&root), vec!["scene01"], "目录应从 path 派生出来");
    assert_eq!(file_paths(&root), vec!["final.mp4"]);
    assert_eq!(
        dir_record_count(&app, &canvas).await,
        0,
        "派生目录不落盘，写文件不该产生目录记录"
    );

    // 子目录的统计是**整棵子树**的合计：3+2 字节、2 个文件。
    assert_eq!(root["dirs"][0]["file_count"], 2);
    assert_eq!(root["dirs"][0]["bytes"], 5);
    assert!(
        root["dirs"][0]["created_at"].is_null(),
        "派生目录没有创建时间"
    );

    // 下一层。
    let sub = entries(&app, &t, &canvas, "scene01").await;
    assert_eq!(dir_names(&sub), vec!["raw"]);
    assert_eq!(file_paths(&sub), vec!["scene01/f001.jpg"]);
    assert_eq!(sub["prefix"], "scene01");
    // 带尾斜杠等价。
    let same = entries(&app, &t, &canvas, "scene01/").await;
    assert_eq!(same["dirs"], sub["dirs"]);
}

#[tokio::test]
async fn 目录在前文件在后且各自字典序() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    for p in ["b.jpg", "a.jpg", "zoo/x.png", "art/y.png"] {
        put(&app, &t, &canvas, p, b"x").await;
    }
    mkdir(&app, &t, &canvas, "mid").await;

    let root = entries(&app, &t, &canvas, "").await;
    // 显式建的 mid 与派生的 art / zoo 混在一起排序，看不出差别。
    assert_eq!(dir_names(&root), vec!["art", "mid", "zoo"]);
    assert_eq!(file_paths(&root), vec!["a.jpg", "b.jpg"]);
}

#[tokio::test]
async fn 建多层目录自动补齐中间层() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    let r = mkdir(&app, &t, &canvas, "a/b/c").await;
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["dirs_written"], 3, "a、a/b、a/b/c 三条");

    assert_eq!(dir_names(&entries(&app, &t, &canvas, "").await), vec!["a"]);
    assert_eq!(dir_names(&entries(&app, &t, &canvas, "a").await), vec!["b"]);
    assert_eq!(
        dir_names(&entries(&app, &t, &canvas, "a/b").await),
        vec!["c"]
    );
    assert!(dir_names(&entries(&app, &t, &canvas, "a/b/c").await).is_empty());
}

#[tokio::test]
async fn 列不存在的目录返回404() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    // 静默返回空列表会让前端以为那是个空目录，与 /api/tree 对不存在父节点的
    // 处理保持一致：报 404。
    let r = app
        .get_auth(&format!("/api/canvases/{canvas}/entries?prefix=nope"), &t)
        .await;
    assert_eq!(r.status(), 404);

    // 空画布的根层是 200 空列表，不是 404。
    let root = entries(&app, &t, &canvas, "").await;
    assert!(dir_names(&root).is_empty() && file_paths(&root).is_empty());
}

// ---------------------------------------------------------------------------
// 不变量与限额
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 同名不能既是目录又是文件() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    // 方向一：先建目录 a，再传文件 a。
    assert_eq!(mkdir(&app, &t, &canvas, "a").await.status(), 201);
    assert_eq!(put(&app, &t, &canvas, "a", b"x").await.status(), 409);

    // 方向二：先传文件 b，再建目录 b。
    assert_eq!(put(&app, &t, &canvas, "b", b"x").await.status(), 200);
    assert_eq!(mkdir(&app, &t, &canvas, "b").await.status(), 409);

    // 祖先段已经是文件：已有文件 b，再写 b/c。
    assert_eq!(put(&app, &t, &canvas, "b/c", b"x").await.status(), 409);
    assert_eq!(mkdir(&app, &t, &canvas, "b/c").await.status(), 409);

    // 派生目录同样算目录：有了 d/x.jpg 之后不能再写文件 d。
    assert_eq!(put(&app, &t, &canvas, "d/x.jpg", b"x").await.status(), 200);
    assert_eq!(put(&app, &t, &canvas, "d", b"x").await.status(), 409);
}

#[tokio::test]
async fn 三条写入通道都守不变量一() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;
    mkdir(&app, &t, &canvas, "taken").await;

    // 通道一：单文件 PUT。
    assert_eq!(put(&app, &t, &canvas, "taken", b"x").await.status(), 409);

    // 通道二：tar 批量。
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "taken", &b"x"[..])
        .expect("写 tar");
    let body = tar.into_inner().expect("收尾");
    let r = app
        .post_bytes_auth(
            &format!("/api/canvases/{canvas}/files:batch"),
            body,
            "application/x-tar",
            &t,
        )
        .await;
    assert_eq!(r.status(), 409, "tar 批量也要守住");

    // 通道三：分块上传。在 initiate 就失败，别让客户端白传 90MB。
    let r = app
        .post_json_auth(
            &format!("/api/canvases/{canvas}/uploads"),
            serde_json::json!({ "path": "taken" }),
            &t,
        )
        .await;
    assert_eq!(r.status(), 409, "分块上传应在 initiate 就拒绝");

    // 一个文件都不该落进去。
    let files = app
        .get_auth(&format!("/api/canvases/{canvas}/files"), &t)
        .await
        .json::<serde_json::Value>()
        .await
        .expect("JSON");
    assert!(files["files"].as_array().expect("有").is_empty());
}

#[tokio::test]
async fn 上传窗口期内被建成目录则complete失败() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    // initiate 时路径还是干净的。
    let r = app
        .post_json_auth(
            &format!("/api/canvases/{canvas}/uploads"),
            serde_json::json!({ "path": "late" }),
            &t,
        )
        .await;
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.expect("JSON");
    let upload = v["upload_id"].as_str().expect("有 upload_id").to_owned();

    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/uploads/{upload}/0"),
        b"payload".to_vec(),
        &t,
    )
    .await;

    // 会话有 24 小时有效期，期间有人把这个路径建成了目录。
    assert_eq!(mkdir(&app, &t, &canvas, "late").await.status(), 201);

    let r = app
        .post_json_auth(
            &format!("/api/canvases/{canvas}/uploads/{upload}/complete"),
            serde_json::json!({}),
            &t,
        )
        .await;
    assert_eq!(r.status(), 409, "complete 必须再查一次，initiate 那次不够");
}

#[tokio::test]
async fn 目录段数上限是十五() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    // 15 段通过：留最后一段给文件名，刚好凑满 16 段的 path 上限。
    let ok = (0..15)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert_eq!(mkdir(&app, &t, &canvas, &ok).await.status(), 201);
    // 那个目录里确实还能放文件。
    assert_eq!(
        put(&app, &t, &canvas, &format!("{ok}/f.jpg"), b"x")
            .await
            .status(),
        200
    );

    // 16 段被拒：否则会建出一个永远放不进文件的目录。
    let too_deep = (0..16)
        .map(|i| format!("d{i}"))
        .collect::<Vec<_>>()
        .join("/");
    assert_eq!(mkdir(&app, &t, &canvas, &too_deep).await.status(), 400);
}

#[tokio::test]
async fn 目录路径不合规被拒() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    for bad in ["../escape", "a//b", "a/./b", ""] {
        assert_eq!(
            mkdir(&app, &t, &canvas, bad).await.status(),
            400,
            "{bad:?} 不该被接受"
        );
    }
    assert_eq!(dir_record_count(&app, &canvas).await, 0);
}

#[tokio::test]
async fn 目录数达到上限后拒绝新建() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    // 记录虽小，但没有上限就等于开放一个「无限写小 key」的接口。
    let limit = 500;
    for i in 0..limit {
        let r = mkdir(&app, &t, &canvas, &format!("d{i:04}")).await;
        assert_eq!(r.status(), 201, "第 {i} 个目录应建成功");
    }
    assert_eq!(dir_record_count(&app, &canvas).await, limit);

    let r = mkdir(&app, &t, &canvas, "one-too-many").await;
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["code"], "conflict");

    // 已存在的那些仍然可以幂等「再建」——幂等命中不写记录，不该被上限挡住。
    assert_eq!(mkdir(&app, &t, &canvas, "d0000").await.status(), 200);
}

// ---------------------------------------------------------------------------
// 删
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 删空目录不需要recursive() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;
    mkdir(&app, &t, &canvas, "scene01").await;

    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}/dirs?path=scene01"), &t)
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["deleted_files"], 0);
    assert_eq!(v["deleted_dirs"], 1);

    assert!(dir_names(&entries(&app, &t, &canvas, "").await).is_empty());
    assert_eq!(dir_record_count(&app, &canvas).await, 0);
}

#[tokio::test]
async fn 删非空目录要带recursive且统计同步下降() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    mkdir(&app, &t, &canvas, "scene01/raw").await;
    put(&app, &t, &canvas, "scene01/f001.jpg", b"aaaa").await;
    put(&app, &t, &canvas, "scene01/raw/x.png", b"bb").await;
    put(&app, &t, &canvas, "keep.mp4", b"ccc").await;

    let used_before: u64 = app
        .get_auth("/api/me", &t)
        .await
        .json::<serde_json::Value>()
        .await
        .expect("JSON")["bytes_used"]
        .as_u64()
        .expect("有 bytes_used");
    assert_eq!(used_before, 9);

    // 不带 recursive：409，一个字节都不该动。手感与删非空文件夹对齐。
    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}/dirs?path=scene01"), &t)
        .await;
    assert_eq!(r.status(), 409);
    assert_eq!(
        file_paths(&entries(&app, &t, &canvas, "scene01").await).len(),
        1
    );

    let r = app
        .delete_auth(
            &format!("/api/canvases/{canvas}/dirs?path=scene01&recursive=true"),
            &t,
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["deleted_files"], 2);
    assert_eq!(v["deleted_dirs"], 2, "scene01 与 scene01/raw 两条记录");
    assert_eq!(v["freed_bytes"], 6);

    // 目录没了，同层别的文件完好。
    let root = entries(&app, &t, &canvas, "").await;
    assert!(dir_names(&root).is_empty());
    assert_eq!(file_paths(&root), vec!["keep.mp4"]);
    assert_eq!(dir_record_count(&app, &canvas).await, 0);

    // 画布统计与用户已用字节都要同步下降——配额是按后者执行的。
    let projects: serde_json::Value = app
        .get_auth("/api/projects", &t)
        .await
        .json()
        .await
        .expect("JSON");
    let node = projects["items"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["canvases"].as_array().unwrap())
        .find(|n| n["id"] == canvas.as_str())
        .expect("应找到画布");
    assert_eq!(node["file_count"], 1);
    assert_eq!(node["bytes"], 3);

    let me: serde_json::Value = app
        .get_auth("/api/me", &t)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(me["bytes_used"], 3);
}

#[tokio::test]
async fn 删目录时内容引用归零进gc队列() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    let r = put(
        &app,
        &t,
        &canvas,
        "scene01/only.jpg",
        b"unique content here",
    )
    .await;
    let v: serde_json::Value = r.json().await.expect("JSON");
    let hash = keys::ContentHash::parse_hex(v["hash"].as_str().expect("有")).expect("解析");

    app.delete_auth(
        &format!("/api/canvases/{canvas}/dirs?path=scene01&recursive=true"),
        &t,
    )
    .await;

    // 复用的是 canvas::stage_delete_files 那条路径，所以减引用与 GC 排队的行为
    // 必须与删单文件完全一致。
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
}

#[tokio::test]
async fn 删不存在的目录返回404() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}/dirs?path=nope"), &t)
        .await;
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn 删画布连带清掉全部目录记录() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    mkdir(&app, &t, &canvas, "a/b/c").await;
    mkdir(&app, &t, &canvas, "empty").await;
    put(&app, &t, &canvas, "a/f.jpg", b"x").await;
    assert_eq!(dir_record_count(&app, &canvas).await, 4);

    let r = app
        .delete_auth(&format!("/api/canvases/{canvas}"), &t)
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["files"], 1);
    assert_eq!(v["dirs"], 4);

    // ==== 这条必须直接读存储层 ====
    // 目录记录不参与任何统计、也没有接口能扫到，漏删的话会在画布消失后永久残留，
    // 而且没有任何人会发现。
    assert_eq!(
        dir_record_count(&app, &canvas).await,
        0,
        "画布删掉后不该剩下任何目录记录"
    );
}

// ---------------------------------------------------------------------------
// 鉴权与隔离
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 目录不跨画布() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let a = mk_canvas(&app, &t).await;
    let b = mk_canvas(&app, &t).await;

    mkdir(&app, &t, &a, "only-in-a").await;
    put(&app, &t, &a, "only-in-a/x.jpg", b"x").await;

    // 键里带 canvas_id 的那道保险。
    assert!(
        dir_names(&entries(&app, &t, &b, "").await).is_empty(),
        "A 画布的目录不该出现在 B 画布里"
    );
    assert_eq!(dir_record_count(&app, &b).await, 0);
}

#[tokio::test]
async fn 跨用户操作目录返回404() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    let bob = app.signup("bob", "password123").await;
    let canvas = mk_canvas(&app, &alice).await;
    mkdir(&app, &alice, &canvas, "scene01").await;

    // 与既有接口一致：不存在与不属于你合并成 404。
    assert_eq!(
        app.get_auth(&format!("/api/canvases/{canvas}/entries"), &bob)
            .await
            .status(),
        404
    );
    assert_eq!(mkdir(&app, &bob, &canvas, "hack").await.status(), 404);
    assert_eq!(
        app.delete_auth(
            &format!("/api/canvases/{canvas}/dirs?path=scene01&recursive=true"),
            &bob
        )
        .await
        .status(),
        404
    );

    assert_eq!(
        dir_record_count(&app, &canvas).await,
        1,
        "alice 的目录应完好"
    );
}

#[tokio::test]
async fn 目录接口都需要凭证() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;

    assert_eq!(
        app.get(&format!("/api/canvases/{canvas}/entries"))
            .await
            .status(),
        401
    );
    assert_eq!(
        app.post_json(
            &format!("/api/canvases/{canvas}/dirs"),
            serde_json::json!({ "path": "x" })
        )
        .await
        .status(),
        401
    );
}

#[tokio::test]
async fn agent权限位约束目录操作() {
    let app = common::spawn_app().await;
    let t = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &t).await;
    let other = mk_canvas(&app, &t).await;

    // 默认权限：能写，不能删；Token 绑定项目。
    let projects: serde_json::Value = app
        .get_auth("/api/projects", &t)
        .await
        .json()
        .await
        .unwrap();
    let project_id = projects["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["canvases"][0]["id"] == canvas.as_str())
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let r = app
        .post_json_auth(
            &format!("/api/projects/{project_id}/agent-tokens"),
            serde_json::json!({"canvas_files_write":true,"canvas_files_delete":false}),
            &t,
        )
        .await;
    assert_eq!(r.status(), 201);
    let v: serde_json::Value = r.json().await.expect("JSON");
    let agent = v["token"].as_str().expect("有 token").to_owned();

    // 读与建目录可以。
    assert_eq!(
        app.get_auth(&format!("/api/canvases/{canvas}/entries"), &agent)
            .await
            .status(),
        200
    );
    assert_eq!(mkdir(&app, &agent, &canvas, "scene01").await.status(), 201);

    // 删目录需要 delete 位：403（凭证没问题，是权限不够）。
    assert_eq!(
        app.delete_auth(&format!("/api/canvases/{canvas}/dirs?path=scene01"), &agent)
            .await
            .status(),
        403
    );

    // 碰别的画布一律 404，不让它探测别的画布是否存在。
    assert_eq!(mkdir(&app, &agent, &other, "x").await.status(), 404);
    assert_eq!(
        app.get_auth(&format!("/api/canvases/{other}/entries"), &agent)
            .await
            .status(),
        404
    );
}

// ---------------------------------------------------------------------------
// 文档
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 目录接口都在openapi文档里() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let doc: serde_json::Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .expect("JSON");

    for (path, method) in [
        ("/api/canvases/{id}/entries", "get"),
        ("/api/canvases/{id}/dirs", "post"),
        ("/api/canvases/{id}/dirs", "delete"),
    ] {
        assert!(
            doc["paths"][path][method].is_object(),
            "{method} {path} 应出现在文档里"
        );
    }

    // 目录 path 走 query / body，所以不该出现任何 `{*path}` 式的通配参数。
    let paths: Vec<&str> = doc["paths"]
        .as_object()
        .expect("有 paths")
        .keys()
        .map(|s| s.as_str())
        .collect();
    assert!(!paths.iter().any(|p| p.contains('*')), "{paths:?}");
}
