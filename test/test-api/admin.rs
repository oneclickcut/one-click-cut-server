//! S8 管理接口、配额、后台维护测试。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::domain::identity;
use one_click_cut_server::storage::Cf;

/// 建一个管理员并登录，返回 token。
async fn admin_token(app: &common::TestApp) -> String {
    let pw = identity::ensure_admin(&app.store, "admin", 1_000_000)
        .await
        .expect("创建")
        .expect("应返回密码");
    let v: serde_json::Value = app.login("admin", &pw).await.json().await.expect("JSON");
    v["access_token"].as_str().expect("有").to_owned()
}

/// 建画布，返回 id。
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

/// 取某用户的 uid。
async fn uid_of(app: &common::TestApp, username: &str) -> String {
    let (uid, _) = identity::find_by_username(&app.store, username)
        .await
        .expect("查")
        .expect("有");
    uid.to_text()
}

// ---------------------------------------------------------------------------
// 鉴权
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 管理接口拒绝普通用户() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;

    // 普通用户拿到的是 403（凭证没问题，是权限不够），无凭证是 401。
    for path in ["/api/admin/users", "/api/admin/stats"] {
        assert_eq!(app.get_auth(path, &user).await.status(), 403, "{path}");
        assert_eq!(app.get(path).await.status(), 401, "{path} 无凭证");
    }
    assert_eq!(
        app.post_json_auth("/api/admin/gc/run", serde_json::json!({}), &user)
            .await
            .status(),
        403
    );
    assert_eq!(
        app.post_json_auth("/api/admin/compact", serde_json::json!({}), &user)
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn 管理接口拒绝agent_token() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &user).await;

    let v: serde_json::Value = app
        .post_json_auth(
            &format!(
                "/api/projects/{}/agent-tokens",
                project_for_canvas(&app, &user, &canvas).await
            ),
            serde_json::json!({}),
            &user,
        )
        .await
        .json()
        .await
        .expect("JSON");
    let agent = v["token"].as_str().expect("有").to_owned();

    // 三条鉴权分支彼此独立：agent token 到不了管理面。
    assert_eq!(app.get_auth("/api/admin/users", &agent).await.status(), 403);
    assert_eq!(app.get_auth("/api/admin/stats", &agent).await.status(), 403);
}

// ---------------------------------------------------------------------------
// 用户管理
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 用户列表含容量占用() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &alice).await;
    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/a.jpg"),
        vec![7u8; 5000],
        &alice,
    )
    .await;

    let v: serde_json::Value = app
        .get_auth("/api/admin/users", &admin)
        .await
        .json()
        .await
        .expect("JSON");
    let users = v["users"].as_array().expect("有 users");
    assert_eq!(users.len(), 2, "admin + alice");

    let a = users
        .iter()
        .find(|u| u["username"] == "alice")
        .expect("应有 alice");
    assert_eq!(a["role"], "user");
    assert_eq!(a["bytes_used"], 5000, "写入应反映在容量统计里");
    assert_eq!(a["quota_bytes"], 5_000_000_000_u64, "新用户为 Basic 5 GB");
    assert_eq!(a["disabled"], false);

    // 响应里不该有任何密码字段。
    let text = serde_json::to_string(&v).expect("序列化");
    assert!(!text.contains("pwd"), "不该泄露密码哈希字段");
    assert!(!text.contains("argon2"), "不该泄露密码哈希");
}

#[tokio::test]
async fn 调整配额与禁用账户() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;

    let r = app
        .patch_json_auth(
            &format!("/api/admin/users/{uid}"),
            serde_json::json!({ "quota_bytes": 10_000 }),
            &admin,
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["quota_bytes"], 10_000);

    // 禁用**立即生效**：alice 手上的 token 马上失效。
    app.patch_json_auth(
        &format!("/api/admin/users/{uid}"),
        serde_json::json!({ "disabled": true }),
        &admin,
    )
    .await;
    assert_eq!(
        app.get_auth("/api/me", &alice).await.status(),
        401,
        "禁用后已签发的 token 应立即失效"
    );

    // 解禁后恢复。
    app.patch_json_auth(
        &format!("/api/admin/users/{uid}"),
        serde_json::json!({ "disabled": false }),
        &admin,
    )
    .await;
    assert_eq!(app.get_auth("/api/me", &alice).await.status(), 200);
}

#[tokio::test]
async fn 重设密码() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;

    let r = app
        .patch_json_auth(
            &format!("/api/admin/users/{uid}"),
            serde_json::json!({ "password": "brand-new-password" }),
            &admin,
        )
        .await;
    assert_eq!(r.status(), 200);
    // 响应里不回显新密码。
    let text = r.text().await.expect("文本");
    assert!(!text.contains("brand-new-password"));

    assert_eq!(
        app.login("alice", "password123").await.status(),
        401,
        "旧密码应失效"
    );
    assert_eq!(app.login("alice", "brand-new-password").await.status(), 200);
}

#[tokio::test]
async fn 重设密码要过强度校验() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;

    // 管理员也不能绕过密码规则。
    let r = app
        .patch_json_auth(
            &format!("/api/admin/users/{uid}"),
            serde_json::json!({ "password": "short" }),
            &admin,
        )
        .await;
    assert_eq!(r.status(), 400);
    // 原密码仍可用。
    assert_eq!(app.login("alice", "password123").await.status(), 200);
}

#[tokio::test]
async fn 不存在的用户返回404() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;

    for uid in ["01JBXY7Z3K8QW5N2M4P6R8T0V1", "not-a-ulid"] {
        let r = app
            .patch_json_auth(
                &format!("/api/admin/users/{uid}"),
                serde_json::json!({ "quota_bytes": 1 }),
                &admin,
            )
            .await;
        assert_eq!(r.status(), 404, "{uid}");
    }
}

// ---------------------------------------------------------------------------
// 配额执行
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 超出配额返回507() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;
    let canvas = mk_canvas(&app, &alice).await;

    app.patch_json_auth(
        &format!("/api/admin/users/{uid}"),
        serde_json::json!({ "quota_bytes": 10_000 }),
        &admin,
    )
    .await;

    // 写 8000 字节，还剩 2000。
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/a.jpg"),
            common::pseudo_content(8000, 1),
            &alice,
        )
        .await;
    assert_eq!(r.status(), 200);

    // 再写 5000 会超限 → 507（接口文档第 9 节指定的状态码）。
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/b.jpg"),
            common::pseudo_content(5000, 2),
            &alice,
        )
        .await;
    assert_eq!(r.status(), 507);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["code"], "quota_exceeded");
    // 错误信息里要有数字，否则用户不知道自己该删多少。
    assert!(
        v["message"].as_str().expect("有").contains("8000"),
        "应告知已用多少: {v}"
    );

    // 超限的那次写入不该留下任何东西。
    let listed: serde_json::Value = app
        .get_auth(&format!("/api/canvases/{canvas}/files"), &alice)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(listed["files"].as_array().expect("有").len(), 1);
}

#[tokio::test]
async fn 恰好填满配额可以写() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;
    let canvas = mk_canvas(&app, &alice).await;

    app.patch_json_auth(
        &format!("/api/admin/users/{uid}"),
        serde_json::json!({ "quota_bytes": 5000 }),
        &admin,
    )
    .await;

    // 边界必须是「超过才拒」，否则用户永远填不满自己的配额。
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/a.jpg"),
            common::pseudo_content(5000, 1),
            &alice,
        )
        .await;
    assert_eq!(r.status(), 200, "恰好等于限额应放行");

    // 再多一个字节就不行。
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/b.jpg"),
            vec![1u8],
            &alice,
        )
        .await;
    assert_eq!(r.status(), 507);
}

#[tokio::test]
async fn 删除后释放配额() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;
    let canvas = mk_canvas(&app, &alice).await;

    app.patch_json_auth(
        &format!("/api/admin/users/{uid}"),
        serde_json::json!({ "quota_bytes": 10_000 }),
        &admin,
    )
    .await;

    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/a.jpg"),
        common::pseudo_content(9000, 1),
        &alice,
    )
    .await;
    // 满了。
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/b.jpg"),
            common::pseudo_content(5000, 2),
            &alice
        )
        .await
        .status(),
        507
    );

    app.delete_auth(&format!("/api/canvases/{canvas}/files/a.jpg"), &alice)
        .await;

    // 删掉之后额度应当回来。
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/b.jpg"),
            common::pseudo_content(5000, 2),
            &alice
        )
        .await
        .status(),
        200,
        "删除后应释放配额"
    );
}

#[tokio::test]
async fn 覆盖写按差值算配额() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;
    let canvas = mk_canvas(&app, &alice).await;

    app.patch_json_auth(
        &format!("/api/admin/users/{uid}"),
        serde_json::json!({ "quota_bytes": 10_000 }),
        &admin,
    )
    .await;

    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/a.jpg"),
        common::pseudo_content(9000, 1),
        &alice,
    )
    .await;

    // 用同样大小覆盖：delta 是 0，即使已经快满了也该放行。
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/a.jpg"),
            common::pseudo_content(9000, 2),
            &alice,
        )
        .await;
    assert_eq!(r.status(), 200, "同大小覆盖 delta 为 0，应放行");

    // 用更小的内容覆盖，应当释放出空间。
    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/a.jpg"),
        common::pseudo_content(1000, 3),
        &alice,
    )
    .await;
    let r = app
        .put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/b.jpg"),
            common::pseudo_content(8000, 4),
            &alice,
        )
        .await;
    assert_eq!(r.status(), 200, "缩小后应释放出空间");
}

#[tokio::test]
async fn 对账修正漂移的统计() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let uid = uid_of(&app, "alice").await;
    let canvas = mk_canvas(&app, &alice).await;

    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/a.jpg"),
        common::pseudo_content(3000, 1),
        &alice,
    )
    .await;

    // 人为把统计改错，模拟某次崩溃或 bug 造成的漂移。
    let (uid_parsed, mut user) = identity::find_by_username(&app.store, "alice")
        .await
        .expect("查")
        .expect("有");
    user.bytes_used = 999_999;
    identity::save(&app.store, uid_parsed, &user)
        .await
        .expect("保存");

    // 配额是靠这个数字执行的，所以漂移必须能被修正。
    let r = app
        .patch_json_auth(
            &format!("/api/admin/users/{uid}"),
            serde_json::json!({ "recount": true }),
            &admin,
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(v["bytes_used"], 3000, "对账应修正为实际值");
}

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 全局统计的数字与实际一致() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let c1 = mk_canvas(&app, &alice).await;
    let c2 = mk_canvas(&app, &alice).await;

    for (canvas, n) in [(&c1, 3), (&c2, 2)] {
        for i in 0..n {
            app.put_bytes_auth(
                &format!("/api/canvases/{canvas}/files/f{i}.jpg"),
                common::pseudo_content(1000, i as u64 + 100),
                &alice,
            )
            .await;
        }
    }

    let v: serde_json::Value = app
        .get_auth("/api/admin/stats", &admin)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(v["users"], 2);
    assert_eq!(v["canvases"], 4);
    assert_eq!(v["files"], 5);
    assert_eq!(v["logical_bytes"], 5000);
    assert!(v["blobs"].as_u64().expect("有") >= 3, "至少 3 份不同内容");
    assert!(v["disk_bytes"].as_u64().expect("有") > 0);
    assert!(v["dedup_ratio"].as_f64().is_some());
    assert_eq!(v["gc_pending"], 0);
    assert_eq!(v["upload_sessions"], 0);
}

#[tokio::test]
async fn 去重率大于一() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &alice).await;

    // 20 个路径同一份 200KB 内容：逻辑 4MB，物理只有 200KB。
    let same = common::pseudo_content(200_000, 42);
    for i in 0..20 {
        app.put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/f{i:02}.jpg"),
            same.clone(),
            &alice,
        )
        .await;
    }

    let v: serde_json::Value = app
        .get_auth("/api/admin/stats", &admin)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(v["logical_bytes"], 4_000_000);
    assert_eq!(v["blobs"], 1, "20 个条目共享一份内容");

    // ==== 去重率是「内容寻址」这条设计唯一能被直接观测到的收益 ====
    let ratio = v["dedup_ratio"].as_f64().expect("有");
    println!(
        "去重率 {ratio:.2}×（逻辑 4MB / 物理 {} 字节）",
        v["disk_bytes"]
    );
    assert!(
        ratio > 3.0,
        "20 份相同内容的去重率应明显大于 1，实际 {ratio}"
    );
}

#[tokio::test]
async fn 待删队列深度可见() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &alice).await;

    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/a.jpg"),
        common::pseudo_content(1000, 1),
        &alice,
    )
    .await;
    app.delete_auth(&format!("/api/canvases/{canvas}/files/a.jpg"), &alice)
        .await;

    let v: serde_json::Value = app
        .get_auth("/api/admin/stats", &admin)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(v["gc_pending"], 1, "归零的内容应出现在队列深度里");
}

// ---------------------------------------------------------------------------
// GC 与 compaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 手动触发gc返回结果() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;

    let r = app
        .post_json_auth("/api/admin/gc/run", serde_json::json!({}), &admin)
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    // 空库跑一轮应该什么都没删，也不该有损坏。
    assert_eq!(v["blobs_deleted"], 0);
    assert_eq!(v["dangling_blobs"], 0);
    assert_eq!(v["orphans_purged"], 0);
}

#[tokio::test]
async fn gc清理孤儿块() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;

    // 造一个孤儿：chunk 列族有值，chunkmeta 没有计数。等价于崩在 WriteBatch
    // 提交之前。
    let data = common::pseudo_content(1000, 7);
    let hash = one_click_cut_server::storage::keys::ContentHash::from_bytes(
        *blake3::hash(&data).as_bytes(),
    );
    app.store
        .put(
            Cf::Chunk,
            one_click_cut_server::storage::keys::content_key(hash),
            data,
        )
        .await
        .expect("put");

    let v: serde_json::Value = app
        .post_json_auth("/api/admin/gc/run", serde_json::json!({}), &admin)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(v["orphans_purged"], 1);
    assert_eq!(v["dangling_blobs"], 0, "孤儿不是数据损坏");
}

#[tokio::test]
async fn gc可跳过对账() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;

    let data = common::pseudo_content(1000, 7);
    let hash = one_click_cut_server::storage::keys::ContentHash::from_bytes(
        *blake3::hash(&data).as_bytes(),
    );
    app.store
        .put(
            Cf::Chunk,
            one_click_cut_server::storage::keys::content_key(hash),
            data,
        )
        .await
        .expect("put");

    // 对账要扫全部内容键，所以给个开关：高频调用时跳过。
    let v: serde_json::Value = app
        .post_json_auth(
            "/api/admin/gc/run?audit=false",
            serde_json::json!({}),
            &admin,
        )
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(v["orphans_purged"], 0, "跳过对账时不该清理孤儿");
}

#[tokio::test]
async fn compaction回收删除后的空间() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &alice).await;

    // 写 20MB 再全删。
    for i in 0..5 {
        app.put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/f{i}.mp4"),
            common::pseudo_content(4 * 1024 * 1024, i),
            &alice,
        )
        .await;
    }
    app.delete_auth(&format!("/api/canvases/{canvas}"), &alice)
        .await;

    // GC 两轮把 blob 与它的块都收掉（各有一小时延迟，这里用直接调用绕过时间）。
    let far_future = one_click_cut_server::util::now_secs() + 3 * 3600;
    for _ in 0..2 {
        one_click_cut_server::storage::gc::run_once(&app.store, far_future, 10_000)
            .await
            .expect("GC");
    }

    // ==== RocksDB 的删除只是写 tombstone ====
    // 不跑 compaction，磁盘不会真的空出来。
    let r = app
        .post_json_auth("/api/admin/compact", serde_json::json!({}), &admin)
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    let before = v["disk_bytes_before"].as_u64().expect("有");
    let after = v["disk_bytes_after"].as_u64().expect("有");
    println!(
        "compaction: {before} → {after} 字节，回收 {}",
        v["reclaimed"]
    );
    assert!(after < before, "compaction 后占用应下降");
}

// ---------------------------------------------------------------------------
// 画布列表
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 管理员可查任意用户的画布() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;

    // 建一棵树：根层一个画布，文件夹里再一个。
    mk_canvas(&app, &alice).await;
    let folder: serde_json::Value = app
        .post_json_auth("/api/folders", serde_json::json!({ "name": "组" }), &alice)
        .await
        .json()
        .await
        .expect("JSON");
    app.post_json_auth(
        "/api/projects",
        serde_json::json!({ "parent_id": folder["id"], "name": "嵌套项目" }),
        &alice,
    )
    .await;

    let uid = uid_of(&app, "alice").await;
    let v: serde_json::Value = app
        .get_auth(&format!("/api/admin/canvases?uid={uid}"), &admin)
        .await
        .json()
        .await
        .expect("JSON");

    // 递归收集：管理员关心「这个用户有哪些画布」，不关心它们在哪层。
    assert_eq!(
        v["canvases"].as_array().expect("有").len(),
        2,
        "应含嵌套在文件夹里的那个"
    );
    for c in v["canvases"].as_array().expect("有") {
        assert_eq!(c["slug"].as_str().expect("有").len(), 12);
        assert!(c["visibility"].is_string());
    }
}

#[tokio::test]
async fn 查不存在的用户返回404() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;

    let r = app
        .get_auth("/api/admin/canvases?uid=01JBXY7Z3K8QW5N2M4P6R8T0V1", &admin)
        .await;
    assert_eq!(r.status(), 404);
}

// ---------------------------------------------------------------------------
// 备份
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkpoint生成可打开的快照() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &alice).await;
    let content = common::pseudo_content(50_000, 3);
    app.put_bytes_auth(
        &format!("/api/canvases/{canvas}/files/a.jpg"),
        content.clone(),
        &alice,
    )
    .await;

    let target = app.dir.path().join("snapshot-1");
    let r = app
        .post_json_auth(
            "/api/admin/checkpoint",
            serde_json::json!({ "target": target.display().to_string() }),
            &admin,
        )
        .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = r.json().await.expect("JSON");
    assert!(v["files"].as_u64().expect("有") > 0, "快照应含文件");

    // ==== 真正的验证：快照能被打开，且数据完整 ====
    // 一份从没被验证过的备份等于没有备份。
    let snap = one_click_cut_server::Store::open_with(
        &target,
        &one_click_cut_server::storage::StoreOptions::small(),
    )
    .expect("快照应能打开");

    let entry = one_click_cut_server::domain::canvas::get_file(
        &snap,
        one_click_cut_server::storage::keys::CanvasId::parse(&canvas).expect("解析"),
        "a.jpg",
    )
    .await
    .expect("查")
    .expect("快照里应有这个文件");

    let back = one_click_cut_server::storage::blob::read_all(&snap, entry.hash)
        .await
        .expect("读")
        .expect("内容应存在");
    assert_eq!(back, content, "从快照读出的内容应与原始一致");
}

#[tokio::test]
async fn checkpoint不覆盖已有目录() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;

    let target = app.dir.path().join("snapshot-2");
    std::fs::create_dir_all(&target).expect("建目录");

    // 拒绝覆盖是 RocksDB 防止「用一份坏快照盖掉好快照」的方式。
    let r = app
        .post_json_auth(
            "/api/admin/checkpoint",
            serde_json::json!({ "target": target.display().to_string() }),
            &admin,
        )
        .await;
    assert_eq!(r.status(), 400);
}

// ---------------------------------------------------------------------------
// 后台维护
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 清理过期凭证() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    let canvas = mk_canvas(&app, &alice).await;

    // 签一个 agent token，然后把它的过期时间推到过去。
    let v: serde_json::Value = app
        .post_json_auth(
            &format!(
                "/api/projects/{}/agent-tokens",
                project_for_canvas(&app, &alice, &canvas).await
            ),
            serde_json::json!({}),
            &alice,
        )
        .await
        .json()
        .await
        .expect("JSON");
    assert!(v["token"].is_string());

    let rows = app
        .store
        .scan_prefix_limit(Cf::Auth, vec![b'a', b't', 0x00], usize::MAX)
        .await
        .expect("扫描");
    let (key, value) = rows.into_iter().next().expect("应有一条");
    let mut record: one_click_cut_server::storage::records::AgentToken =
        one_click_cut_server::storage::records::decode(&value).expect("解码");
    record.expires_at = 1;
    app.store
        .put(
            Cf::Auth,
            key,
            one_click_cut_server::storage::records::encode(&record).expect("编码"),
        )
        .await
        .expect("写回");

    let report = one_click_cut_server::domain::maintenance::purge_credentials(
        &app.store,
        one_click_cut_server::util::now_secs(),
    )
    .await
    .expect("清理");
    assert_eq!(report.agent_tokens, 1);

    // 索引也要清掉，否则列表会留下一个查不到的条目。
    let left = app
        .store
        .scan_prefix_limit(Cf::Auth, vec![b'a', b't', b'c', 0x00], usize::MAX)
        .await
        .expect("扫描");
    assert!(left.is_empty(), "token 的画布索引应一并清除");
}

#[tokio::test]
async fn 管理接口都在openapi文档里() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let doc: serde_json::Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .expect("JSON");

    for (path, method) in [
        ("/api/admin/users", "get"),
        ("/api/admin/users/{uid}", "patch"),
        ("/api/admin/stats", "get"),
        ("/api/admin/gc/run", "post"),
        ("/api/admin/compact", "post"),
        ("/api/admin/canvases", "get"),
        ("/api/admin/checkpoint", "post"),
    ] {
        assert!(
            doc["paths"][path][method].is_object(),
            "{method} {path} 应出现在文档里"
        );
    }
}
