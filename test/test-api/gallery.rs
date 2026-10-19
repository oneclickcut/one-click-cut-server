//! 画廊公开读取、权限隔离、生命周期和故障恢复集成测试。
#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::{
    domain::{gallery, identity, space},
    storage::{
        Batch, Cf,
        keys::{self, CanvasId, ProjectId, ShareId},
        records::{self, GalleryTarget, ProjectStatus},
    },
};
use serde_json::{Value, json};

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}
async fn json_ok(r: reqwest::Response, status: u16) -> Value {
    let actual = r.status();
    let body = r.text().await.unwrap();
    assert_eq!(actual.as_u16(), status, "{body}");
    serde_json::from_str(&body).unwrap()
}
async fn admin(app: &common::TestApp) -> String {
    let password = identity::ensure_admin(&app.store, "admin", 1_000_000)
        .await
        .unwrap()
        .unwrap();
    let v = json_ok(app.login("admin", &password).await, 200).await;
    s(&v["access_token"]).to_owned()
}
async fn paid_signup(app: &common::TestApp, name: &str) -> String {
    let token = app.signup(name, "password123").await;
    let (uid, user) = identity::find_by_username(&app.store, name)
        .await
        .unwrap()
        .unwrap();
    let mut plan = one_click_cut_server::domain::plans::get(&app.store, uid, user.created_at)
        .await
        .unwrap();
    plan.plan = one_click_cut_server::domain::plans::Plan::Pro;
    app.store
        .put(
            Cf::Meta,
            keys::user_plan_key(uid),
            serde_json::to_vec(&plan).unwrap(),
        )
        .await
        .unwrap();
    token
}
async fn ready(app: &common::TestApp, user: &str, parent: Option<&str>) -> Value {
    let p = json_ok(
        app.post_json_auth(
            "/api/projects",
            json!({"name":"作品", "parent_id":parent}),
            user,
        )
        .await,
        201,
    )
    .await;
    prepare(app, user, s(&p["canvases"][0]["id"])).await;
    p
}
async fn prepare(app: &common::TestApp, user: &str, cid: &str) {
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{cid}/files/timeline.json"),
            b"{}".to_vec(),
            user
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        app.patch_json_auth(
            &format!("/api/canvases/{cid}"),
            json!({"visibility":"public","entry":"timeline.json"}),
            user
        )
        .await
        .status(),
        200
    );
}
fn config(p: &Value) -> Value {
    json!({"title":" 展示作品 ", "description":"作品简介", "author_name":" 创作者 ", "canvas_ids":[p["canvases"][0]["id"]], "cover":null})
}
async fn put(app: &common::TestApp, user: &str, pid: &str, body: Value) -> reqwest::Response {
    app.client
        .put(app.url(&format!("/api/projects/{pid}/share")))
        .bearer_auth(user)
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn page(app: &common::TestApp, query: &str) -> Value {
    json_ok(app.get(&format!("/api/gallery/projects{query}")).await, 200).await
}

#[tokio::test]
async fn 画廊复用项目资料封面并随文件更新和删除刷新() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let p = ready(&app, &user, None).await;
    let pid = s(&p["id"]);
    let cid = s(&p["canvases"][0]["id"]);
    let detail_url = format!("/api/gallery/projects/{pid}");
    let resources_url = format!("/api/projects/{pid}/resources/files");
    let encoded_path = "%E5%B0%81%E9%9D%A2%20%23%3F/COVER.PNG";
    let file_url = format!("{resources_url}/{encoded_path}");
    // 先发布无封面的历史作品，再上传项目封面，无需重新共享。
    json_ok(put(&app, &user, pid, config(&p)).await, 201).await;
    assert!(json_ok(app.get(&detail_url).await, 200).await["cover_url"].is_null());
    assert_eq!(
        app.put_bytes_auth(&file_url, b"project cover".to_vec(), &user)
            .await
            .status(),
        200
    );
    // 即使旧配置指定了制作画布封面，也优先使用与“我的项目”相同的图片。
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{cid}/files/old.jpg"),
            b"old".to_vec(),
            &user
        )
        .await
        .status(),
        200
    );
    let mut body = config(&p);
    body["cover"] = json!({"canvas_id":cid,"path":"old.jpg"});
    json_ok(put(&app, &user, pid, body).await, 200).await;
    let detail = json_ok(app.get(&detail_url).await, 200).await;
    let expected = format!(
        "{}{encoded_path}?v={}",
        s(&p["resources"]["base_url"]),
        &blake3::hash(b"project cover").to_hex()[..8]
    );
    assert_eq!(detail["cover_url"], expected);
    assert_eq!(page(&app, "").await["items"][0]["cover_url"], expected);
    assert_eq!(
        app.get(&expected).await.bytes().await.unwrap().as_ref(),
        b"project cover"
    );
    assert!(detail.get("resources").is_none());
    assert_eq!(app.get(&resources_url).await.status(), 401);
    assert_eq!(
        app.put_bytes_auth(&file_url, b"updated cover".to_vec(), &user)
            .await
            .status(),
        200
    );
    let updated = json_ok(app.get(&detail_url).await, 200).await;
    assert_ne!(updated["cover_url"], expected);
    assert_eq!(
        app.get(s(&updated["cover_url"]))
            .await
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        b"updated cover"
    );
    assert_eq!(app.delete_auth(&file_url, &user).await.status(), 204);
    let fallback = json_ok(app.get(&detail_url).await, 200).await;
    assert!(s(&fallback["cover_url"]).contains("/old.jpg?v="));
    assert_eq!(
        app.delete_auth(&format!("/api/canvases/{cid}/files/old.jpg"), &user)
            .await
            .status(),
        204
    );
    assert!(json_ok(app.get(&detail_url).await, 200).await["cover_url"].is_null());
    assert_eq!(page(&app, "").await["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn 匿名浏览共享更新取消与重新共享() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let p = ready(&app, &user, None).await;
    let pid = s(&p["id"]);
    let share_url = format!("/api/projects/{pid}/share");
    let detail_url = format!("/api/gallery/projects/{pid}");
    assert_eq!(p["is_shared"], false);
    let before = json_ok(app.get_auth(&share_url, &user).await, 200).await;
    assert_eq!(
        before,
        json!({"project_id":pid,"is_shared":false,"is_visible":false,"unavailable_reasons":[],"share":null,"can_toggle_share":true,"auto_share_suppressed":false,"suppression_reason":null})
    );
    assert!(page(&app, "").await["items"].as_array().unwrap().is_empty());
    assert_eq!(app.get(&detail_url).await.status(), 404);
    let usage = json_ok(app.get_auth("/api/me", &user).await, 200).await["bytes_used"].clone();
    let shared = json_ok(put(&app, &user, pid, config(&p)).await, 201).await;
    assert_eq!(shared["is_visible"], true);
    assert_eq!(shared["share"]["title"], "展示作品");
    let mut changed = config(&p);
    changed["title"] = json!("修改后的作品");
    let updated = json_ok(put(&app, &user, pid, changed).await, 200).await;
    assert_eq!(updated["share"]["share_id"], shared["share"]["share_id"]);
    assert_eq!(updated["share"]["shared_at"], shared["share"]["shared_at"]);
    assert_eq!(page(&app, "").await["items"].as_array().unwrap().len(), 1);
    let r = app.get(&detail_url).await;
    assert_eq!(r.headers()["cache-control"], "no-store");
    let detail = json_ok(r, 200).await;
    assert_eq!(detail["title"], "修改后的作品");
    assert_eq!(detail["canvas_count"], 1);
    for private in [
        "owner_uid",
        "username",
        "resources",
        "parent_id",
        "permissions",
        "token",
    ] {
        assert!(detail.get(private).is_none(), "{private}");
    }
    assert_eq!(
        app.get(s(&detail["canvases"][0]["manifest_url"]))
            .await
            .status(),
        200
    );
    assert_eq!(app.get_auth(&detail_url, "expired-jwt").await.status(), 200);
    assert_eq!(
        app.get_auth("/api/gallery/projects", "expired-jwt")
            .await
            .status(),
        200
    );
    assert_eq!(
        json_ok(app.get_auth("/api/me", &user).await, 200).await["bytes_used"],
        usage
    );
    assert_eq!(
        json_ok(
            app.get_auth(&format!("/api/projects/{pid}"), &user).await,
            200
        )
        .await["is_shared"],
        true
    );
    // 新增画布，即使可播放也不自动公开到这条作品。
    let extra = json_ok(
        app.post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"新画布"}),
            &user,
        )
        .await,
        201,
    )
    .await;
    prepare(&app, &user, s(&extra["id"])).await;
    assert_eq!(
        json_ok(app.get(&detail_url).await, 200).await["canvas_count"],
        1
    );
    for _ in 0..2 {
        assert_eq!(app.delete_auth(&share_url, &user).await.status(), 204);
    }
    assert!(page(&app, "").await["items"].as_array().unwrap().is_empty());
    assert_eq!(app.get(&detail_url).await.status(), 404);
    assert_eq!(
        app.get(s(&detail["canvases"][0]["manifest_url"]))
            .await
            .status(),
        200
    );
    let again = json_ok(put(&app, &user, pid, config(&p)).await, 201).await;
    assert_ne!(again["share"]["share_id"], shared["share"]["share_id"]);
    assert_eq!(
        app.store
            .scan_prefix(Cf::Idx, keys::gallery_feed_prefix())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn 管理权限隔离和管理员真实项目归属() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let other = paid_signup(&app, "bob").await;
    let adm = admin(&app).await;
    let p = ready(&app, &user, None).await;
    let pid = s(&p["id"]);
    let url = format!("/api/projects/{pid}/share");
    let agent = json_ok(
        app.post_json_auth(
            &format!("/api/projects/{pid}/agent-tokens"),
            json!({}),
            &user,
        )
        .await,
        201,
    )
    .await;
    let agent = s(&agent["token"]);
    assert_eq!(app.get(&url).await.status(), 401);
    assert_eq!(
        app.client
            .put(app.url(&url))
            .json(&config(&p))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        app.client
            .delete(app.url(&url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for (token, expected) in [(&other[..], 404), (agent, 403), ("bad-jwt", 401)] {
        assert_eq!(app.get_auth(&url, token).await.status(), expected);
        assert_eq!(put(&app, token, pid, config(&p)).await.status(), expected);
        assert_eq!(app.delete_auth(&url, token).await.status(), expected);
    }
    let shared = json_ok(put(&app, &adm, pid, config(&p)).await, 201).await;
    assert_eq!(shared["is_visible"], true);
    let owner = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(
        gallery::get(&app.store, ProjectId::parse(pid).unwrap())
            .await
            .unwrap()
            .unwrap()
            .owner_uid,
        owner
    );
    assert_eq!(app.get_auth(&url, &adm).await.status(), 200);
    let mut updated = config(&p);
    updated["title"] = json!("管理员修改");
    assert_eq!(put(&app, &adm, pid, updated).await.status(), 200);
    assert_eq!(
        json_ok(app.get_auth(&url, &user).await, 200).await["share"]["title"],
        "管理员修改"
    );
    assert_eq!(
        app.get_auth(&format!("/api/gallery/projects/{pid}"), agent)
            .await
            .status(),
        200
    );
    assert_eq!(
        app.get_auth(&format!("/api/projects/{pid}"), &other)
            .await
            .status(),
        404
    );
    assert_eq!(app.delete_auth(&url, &adm).await.status(), 204);
    assert_eq!(
        app.get(&format!("/api/gallery/projects/{pid}"))
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn 配置校验失败不部分写入() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let p = ready(&app, &user, None).await;
    let other = ready(&app, &user, None).await;
    let pid = s(&p["id"]);
    let cid = s(&p["canvases"][0]["id"]);
    let mut invalid = vec![];
    for (field, value) in [
        ("title", json!(" ")),
        ("title", json!("中".repeat(101))),
        ("author_name", json!("中".repeat(41))),
        ("description", json!("中".repeat(2001))),
        ("canvas_ids", json!([])),
        ("canvas_ids", json!([cid, cid])),
        ("canvas_ids", json!([other["canvases"][0]["id"]])),
        ("canvas_ids", json!([p["resources"]["id"]])),
        ("canvas_ids", json!(["ZZZZZZZZZZZZZZZZZZZZZZZZZZ"])),
        ("cover", json!({"canvas_id":cid,"path":"missing.png"})),
        ("cover", json!({"canvas_id":cid,"path":"../image.png"})),
        ("cover", json!({"canvas_id":cid,"path":"timeline.json"})),
        ("owner_uid", json!("injected")),
    ] {
        let mut b = config(&p);
        b[field] = value;
        invalid.push(b);
    }
    let mut missing = config(&p);
    missing.as_object_mut().unwrap().remove("cover");
    invalid.push(missing);
    for body in invalid {
        json_ok(put(&app, &user, pid, body).await, 400).await;
    }
    let mut too_large = config(&p);
    too_large["description"] = json!("x".repeat(33000));
    json_ok(put(&app, &user, pid, too_large).await, 413).await;
    assert!(
        gallery::get(&app.store, ProjectId::parse(pid).unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        app.store
            .scan_prefix(Cf::Idx, keys::gallery_feed_prefix())
            .await
            .unwrap()
            .is_empty()
    );
    app.patch_json_auth(
        &format!("/api/canvases/{cid}"),
        json!({"visibility":"private"}),
        &user,
    )
    .await;
    assert_eq!(put(&app, &user, pid, config(&p)).await.status(), 409);
    app.patch_json_auth(
        &format!("/api/canvases/{cid}"),
        json!({"visibility":"public","entry":"missing.json"}),
        &user,
    )
    .await;
    assert_eq!(put(&app, &user, pid, config(&p)).await.status(), 409);
}

#[tokio::test]
async fn 动态隐藏画布入口封面与被禁用的作者() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let adm = admin(&app).await;
    let p = ready(&app, &user, None).await;
    let pid = s(&p["id"]);
    let cid = s(&p["canvases"][0]["id"]);
    let detail_url = format!("/api/gallery/projects/{pid}");
    let canvas_url = format!("/api/canvases/{cid}");
    // 封面路径中的 URL 特殊字符必须转义。
    assert_eq!(
        app.put_bytes_auth(
            &format!("{canvas_url}/files/cover%20%23%3F.png"),
            b"png".to_vec(),
            &user
        )
        .await
        .status(),
        200
    );
    let mut body = config(&p);
    body["cover"] = json!({"canvas_id":cid,"path":"cover #?.png"});
    json_ok(put(&app, &user, pid, body).await, 201).await;
    let detail = json_ok(app.get(&detail_url).await, 200).await;
    let cover_url = s(&detail["cover_url"]);
    assert!(cover_url.contains("cover%20%23%3F.png?v="));
    assert_eq!(app.get(cover_url).await.status(), 200);
    assert_eq!(
        app.delete_auth(&format!("{canvas_url}/files/cover%20%23%3F.png"), &user)
            .await
            .status(),
        204
    );
    assert!(json_ok(app.get(&detail_url).await, 200).await["cover_url"].is_null());
    app.patch_json_auth(&canvas_url, json!({"visibility":"private"}), &user)
        .await;
    assert_eq!(app.get(&detail_url).await.status(), 404);
    assert!(page(&app, "").await["items"].as_array().unwrap().is_empty());
    let state = json_ok(
        app.get_auth(&format!("/api/projects/{pid}/share"), &user)
            .await,
        200,
    )
    .await;
    assert_eq!(state["is_shared"], false);
    assert_eq!(state["suppression_reason"], "owner");
    assert_eq!(state["is_visible"], false);
    app.patch_json_auth(&canvas_url, json!({"visibility":"public"}), &user)
        .await;
    assert_eq!(app.get(&detail_url).await.status(), 404);
    json_ok(
        app.patch_json_auth(
            &format!("/api/projects/{pid}/sharing"),
            json!({"enabled":true}),
            &user,
        )
        .await,
        200,
    )
    .await;
    assert_eq!(app.get(&detail_url).await.status(), 200);
    app.delete_auth(&format!("{canvas_url}/files/timeline.json"), &user)
        .await;
    assert_eq!(app.get(&detail_url).await.status(), 404);
    prepare(&app, &user, cid).await;
    let owner = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap()
        .0;
    for (disabled, expected) in [(true, 404), (false, 200)] {
        assert_eq!(
            app.patch_json_auth(
                &format!("/api/admin/users/{owner}"),
                json!({"disabled":disabled}),
                &adm
            )
            .await
            .status(),
            200
        );
        assert_eq!(app.get(&detail_url).await.status(), expected);
    }
    app.delete_auth(&canvas_url, &user).await;
    assert_eq!(app.get(&detail_url).await.status(), 404);
}

#[tokio::test]
async fn 删除失败先下架且可重试并覆盖文件夹级联() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let p = ready(&app, &user, None).await;
    let pid = ProjectId::parse(s(&p["id"])).unwrap();
    let cid = CanvasId::parse(s(&p["canvases"][0]["id"])).unwrap();
    json_ok(put(&app, &user, &pid.to_text(), config(&p)).await, 201).await;
    // 故障注入：文件记录损坏使后续清理失败，但 deleting 与下架必须已持久化。
    let file_key = keys::file_key(cid, "timeline.json").unwrap();
    let original = app
        .store
        .get(Cf::Meta, file_key.clone())
        .await
        .unwrap()
        .unwrap();
    app.store
        .put(Cf::Meta, file_key.clone(), vec![255])
        .await
        .unwrap();
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}"), &user)
            .await
            .status(),
        500
    );
    let owner = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(
        space::get_project(&app.store, owner, pid)
            .await
            .unwrap()
            .unwrap()
            .status,
        ProjectStatus::Deleting
    );
    assert!(gallery::get(&app.store, pid).await.unwrap().is_none());
    assert_eq!(
        app.get(&format!("/api/gallery/projects/{pid}"))
            .await
            .status(),
        404
    );
    assert_eq!(
        put(&app, &user, &pid.to_text(), config(&p)).await.status(),
        409
    );
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}/share"), &user)
            .await
            .status(),
        204
    );
    app.store.put(Cf::Meta, file_key, original).await.unwrap();
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}"), &user)
            .await
            .status(),
        204
    );
    assert!(
        app.store
            .scan_prefix(Cf::Idx, keys::gallery_feed_prefix())
            .await
            .unwrap()
            .is_empty()
    );
    let folder = json_ok(
        app.post_json_auth("/api/folders", json!({"name":"目录"}), &user)
            .await,
        201,
    )
    .await;
    let p = ready(&app, &user, Some(s(&folder["id"]))).await;
    json_ok(put(&app, &user, s(&p["id"]), config(&p)).await, 201).await;
    assert_eq!(
        app.delete_auth(
            &format!("/api/folders/{}?recursive=true", s(&folder["id"])),
            &user
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        app.get(&format!("/api/gallery/projects/{}", s(&p["id"])))
            .await
            .status(),
        404
    );
    assert!(
        app.store
            .scan_prefix(Cf::Idx, keys::gallery_feed_prefix())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn 分页越过无效索引预算与已删除游标() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let mut shared = Vec::new();
    for _ in 0..3 {
        let p = ready(&app, &user, None).await;
        let share = json_ok(put(&app, &user, s(&p["id"]), config(&p)).await, 201).await;
        shared.push((
            s(&share["share"]["share_id"]).to_owned(),
            s(&p["id"]).to_owned(),
        ));
    }
    shared.sort();
    shared.reverse();
    let first = page(&app, "?limit=1").await;
    assert_eq!(first["items"][0]["project_id"], shared[0].1);
    assert_eq!(first["has_more"], true);
    app.delete_auth(&format!("/api/projects/{}/share", shared[0].1), &user)
        .await;
    let second = page(
        &app,
        &format!("?limit=100&cursor={}", s(&first["next_cursor"])),
    )
    .await;
    assert_eq!(second["items"].as_array().unwrap().len(), 2);
    assert_eq!(second["items"][0]["project_id"], shared[1].1);
    assert_eq!(second["has_more"], false);
    let owner = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap()
        .0;
    let mut batch = Batch::new();
    // 比当前发布更大的 1001 个旧索引，指向存在项目但不匹配共享周期。
    for i in 0..1001 {
        let sid = ShareId::parse(&format!("7{}{:04}", "Z".repeat(21), i)).unwrap();
        batch.put(
            Cf::Idx,
            keys::gallery_feed_key(sid),
            records::encode(&GalleryTarget {
                owner_uid: owner,
                project_id: ProjectId::parse(&shared[1].1).unwrap(),
            })
            .unwrap(),
        );
    }
    app.store.write(batch).await.unwrap();
    let empty = page(&app, "?limit=2").await;
    assert!(empty["items"].as_array().unwrap().is_empty());
    assert_eq!(empty["has_more"], true);
    let rest = page(
        &app,
        &format!("?limit=2&cursor={}", s(&empty["next_cursor"])),
    )
    .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 2);
    assert_eq!(rest["has_more"], false);
    for query in [
        "limit=0",
        "limit=101",
        "limit=-1",
        "limit=abc",
        "cursor=",
        "cursor=bad",
        "cursor=v1_ZZZZZZZZZZZZZZZZZZZZZZZZZZ",
        "limit=1&limit=2",
        "order=asc",
    ] {
        json_ok(
            app.get(&format!("/api/gallery/projects?{query}")).await,
            400,
        )
        .await;
    }
}

#[tokio::test]
async fn 并发共享取消和删除不会留下幽灵作品() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    for _ in 0..4 {
        let p = ready(&app, &user, None).await;
        let pid = s(&p["id"]);
        let share_url = format!("/api/projects/{pid}/share");
        let (a, b) = tokio::join!(
            put(&app, &user, pid, config(&p)),
            app.delete_auth(&share_url, &user)
        );
        assert_eq!(a.status(), 201);
        assert_eq!(b.status(), 204);
        let state = json_ok(app.get_auth(&share_url, &user).await, 200).await;
        let rows = page(&app, "").await;
        assert_eq!(
            rows["items"].as_array().unwrap().len(),
            usize::from(state["is_shared"].as_bool().unwrap())
        );
        let project_url = format!("/api/projects/{pid}");
        let (a, b) = tokio::join!(
            put(&app, &user, pid, config(&p)),
            app.delete_auth(&project_url, &user)
        );
        assert!([200, 201, 404, 409].contains(&a.status().as_u16()));
        assert_eq!(b.status(), 204);
        assert_eq!(
            app.get(&format!("/api/gallery/projects/{pid}"))
                .await
                .status(),
            404
        );
        assert!(
            app.store
                .scan_prefix(Cf::Idx, keys::gallery_feed_prefix())
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn 接口文档区分匿名读取与共享管理() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let spec = json_ok(app.get("/api-docs/openapi.json").await, 200).await;
    for path in [
        "/api/gallery/projects",
        "/api/gallery/projects/{project_id}",
    ] {
        let op = &spec["paths"][path]["get"];
        assert!(op.is_object());
        assert!(
            op.get("security")
                .is_none_or(|v| v.as_array().unwrap().is_empty())
        );
    }
    for method in ["get", "put", "delete"] {
        assert_eq!(
            spec["paths"]["/api/projects/{id}/share"][method]["security"][0],
            json!({"bearer_jwt":[]})
        );
    }
    let required = spec["components"]["schemas"]["ShareRequest"]["required"]
        .as_array()
        .unwrap();
    assert!(required.contains(&json!("cover")));
    let r = app.get("/api/gallery/projects").await;
    assert_eq!(r.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn 多画布筛选排序和修改失败保留原共享() {
    let app = common::spawn_app().await;
    let user = paid_signup(&app, "alice").await;
    let p = ready(&app, &user, None).await;
    let pid = s(&p["id"]);
    let first = s(&p["canvases"][0]["id"]);
    let extra = json_ok(
        app.post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"第二集"}),
            &user,
        )
        .await,
        201,
    )
    .await;
    let second = s(&extra["id"]);
    prepare(&app, &user, second).await;
    app.patch_json_auth(
        &format!("/api/canvases/{second}"),
        json!({"sort_order":-1}),
        &user,
    )
    .await;
    let mut body = config(&p);
    body["canvas_ids"] = json!([first, second]);
    json_ok(put(&app, &user, pid, body.clone()).await, 201).await;
    let detail_url = format!("/api/gallery/projects/{pid}");
    let detail = json_ok(app.get(&detail_url).await, 200).await;
    assert_eq!(detail["canvases"][0]["id"], second);
    assert_eq!(detail["canvas_count"], 2);
    app.patch_json_auth(
        &format!("/api/canvases/{second}"),
        json!({"entry":"missing.json"}),
        &user,
    )
    .await;
    let detail = json_ok(app.get(&detail_url).await, 200).await;
    assert_eq!(detail["canvas_count"], 1);
    assert_eq!(detail["canvases"][0]["id"], first);
    assert_eq!(page(&app, "").await["items"][0]["canvas_count"], 1);
    body["title"] = json!("不应写入的标题");
    assert_eq!(put(&app, &user, pid, body).await.status(), 409);
    assert_eq!(
        json_ok(app.get(&detail_url).await, 200).await["title"],
        "展示作品"
    );
    let adm = admin(&app).await;
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}"), &adm)
            .await
            .status(),
        204
    );
    assert!(page(&app, "").await["items"].as_array().unwrap().is_empty());
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}/share"), &user)
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn 数据库重开保持共享与下架且删除重试清理残留() {
    use one_click_cut_server::domain::canvas;
    use one_click_cut_server::storage::records::{ShareConfig, Visibility};
    let db = common::temp_store();
    identity::ensure_admin(&db.store, "admin", 1_000_000)
        .await
        .unwrap();
    let uid = identity::find_by_username(&db.store, "admin")
        .await
        .unwrap()
        .unwrap()
        .0;
    let (pid, mut p, cid, _) =
        space::create_project(&db.store, uid, keys::ParentId::Root, "持久化项目", None, 1)
            .await
            .unwrap()
            .unwrap();
    canvas::put_file(
        &db.store,
        uid,
        cid,
        "timeline.json",
        "application/json",
        b"{}",
        2,
    )
    .await
    .unwrap()
    .unwrap();
    space::update_canvas(
        &db.store,
        uid,
        uid,
        cid,
        None,
        Some(Visibility::Public),
        Some("timeline.json"),
        None,
        3,
    )
    .await
    .unwrap()
    .unwrap();
    let (share, _) = gallery::put(
        &db.store,
        uid,
        pid,
        ShareConfig {
            title: "作品".into(),
            description: String::new(),
            author_name: "作者".into(),
            canvas_ids: vec![cid],
            cover: None,
        },
        4,
    )
    .await
    .unwrap();
    let db = db.reopen();
    let stored = gallery::get(&db.store, pid).await.unwrap().unwrap();
    assert_eq!(stored.share_id, share.share_id);
    assert_eq!(
        gallery::inspect(&db.store, &stored)
            .await
            .unwrap()
            .canvases
            .len(),
        1
    );
    // 模拟中断留下 deleting 项目与残留共享；重启后不展示，重试删除仍负责清理。
    p.status = ProjectStatus::Deleting;
    db.store
        .put(
            Cf::Meta,
            keys::project_key(uid, pid),
            records::encode(&p).unwrap(),
        )
        .await
        .unwrap();
    let db = db.reopen();
    assert!(
        gallery::inspect(&db.store, &stored)
            .await
            .unwrap()
            .canvases
            .is_empty()
    );
    space::delete_project(&db.store, uid, pid, 5)
        .await
        .unwrap()
        .unwrap();
    let db = db.reopen();
    assert!(gallery::get(&db.store, pid).await.unwrap().is_none());
    assert!(
        db.store
            .scan_prefix(Cf::Idx, keys::gallery_feed_prefix())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        space::get_project(&db.store, uid, pid)
            .await
            .unwrap()
            .is_none()
    );
}
