//! 自动共享的套餐约束、幂等、恢复及原子性验收。
#[path = "../common/mod.rs"]
mod common;
use one_click_cut_server::{
    domain::{gallery, gallery_policy, identity},
    storage::{
        Cf,
        keys::{self, CanvasId, ProjectId, Uid},
        records,
    },
};
use serde_json::{Value, json};
const COMPLETE: &str = "/api/internal/generation-completions";
const PREF: &str = "/api/me/gallery-preferences";
fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}
async fn ok(r: reqwest::Response, status: u16) -> Value {
    let actual = r.status();
    let text = r.text().await.unwrap();
    assert_eq!(actual.as_u16(), status, "{text}");
    serde_json::from_str(&text).unwrap()
}
struct Fixture {
    app: common::TestApp,
    user: String,
    worker: String,
    admin: String,
    uid: String,
}
impl Fixture {
    async fn new(plan: &str) -> Self {
        let app = common::spawn_app_with(|c| {
            c.gallery.publisher_usernames = vec!["worker".into()];
            c.task_queue.reader_usernames = vec!["worker".into(), "reader".into()];
        })
        .await;
        let user = app.signup("alice", "password123").await;
        let worker = app.signup("worker", "password123").await;
        let password = identity::ensure_admin(&app.store, "admin", 0)
            .await
            .unwrap()
            .unwrap();
        let admin =
            s(&ok(app.login("admin", &password).await, 200).await["access_token"]).to_owned();
        let uid = s(&ok(app.get_auth("/api/me", &user).await, 200).await["uid"]).to_owned();
        let f = Self {
            app,
            user,
            worker,
            admin,
            uid,
        };
        if plan != "basic" {
            f.plan(plan).await;
        }
        f
    }
    async fn plan(&self, plan: &str) {
        ok(
            self.app
                .patch_json_auth(
                    &format!("/api/admin/users/{}", self.uid),
                    json!({"plan":plan}),
                    &self.admin,
                )
                .await,
            200,
        )
        .await;
    }
    async fn project(&self, parent: Option<&str>) -> Value {
        ok(
            self.app
                .post_json_auth(
                    "/api/projects",
                    json!({"name":"作品", "parent_id":parent}),
                    &self.user,
                )
                .await,
            201,
        )
        .await
    }
    async fn request(&self, p: &Value, cid: &str) -> Value {
        ok(
            self.app
                .put_bytes_auth(
                    &format!("/api/canvases/{cid}/files/story.json"),
                    b"{}".to_vec(),
                    &self.user,
                )
                .await,
            200,
        )
        .await;
        json!({"generation_id":keys::GenerationId::new().to_text(), "task_user_id":self.uid, "project_id":p["id"], "canvas_id":cid, "entry":"story.json", "entry_hash":blake3::hash(b"{}").to_hex().to_string(), "cover_path":null})
    }
    async fn finish(&self, r: Value) -> Value {
        ok(
            self.app.post_json_auth(COMPLETE, r, &self.worker).await,
            200,
        )
        .await
    }
    async fn toggle(&self, p: &Value, enabled: bool, token: &str, status: u16) -> Value {
        ok(
            self.app
                .patch_json_auth(
                    &format!("/api/projects/{}/sharing", s(&p["id"])),
                    json!({"enabled":enabled}),
                    token,
                )
                .await,
            status,
        )
        .await
    }
    async fn share(&self, p: &Value) -> Value {
        ok(
            self.app
                .get_auth(&format!("/api/projects/{}/share", s(&p["id"])), &self.user)
                .await,
            200,
        )
        .await
    }
    async fn detail(&self, p: &Value) -> reqwest::Response {
        self.app
            .get(&format!("/api/gallery/projects/{}", s(&p["id"])))
            .await
    }
    async fn more(&self, p: &Value) -> String {
        s(&ok(
            self.app
                .post_json_auth(
                    &format!("/api/projects/{}/canvases", s(&p["id"])),
                    json!({"name":"下一集"}),
                    &self.user,
                )
                .await,
            201,
        )
        .await["id"])
        .to_owned()
    }
}

#[tokio::test]
async fn 自动共享未传封面时读取项目公共资料封面() {
    let f = Fixture::new("basic").await;
    let p = f.project(None).await;
    let pid = s(&p["id"]);
    let request = f.request(&p, s(&p["canvases"][0]["id"])).await;
    assert_eq!(request["cover_path"], Value::Null);
    ok(
        f.app
            .put_bytes_auth(
                &format!("/api/projects/{pid}/resources/files/cover.png"),
                b"project cover".to_vec(),
                &f.user,
            )
            .await,
        200,
    )
    .await;
    assert_eq!(f.finish(request).await["auto_share_applied"], true);
    let detail = ok(f.detail(&p).await, 200).await;
    let expected = format!(
        "{}cover.png?v={}",
        s(&p["resources"]["base_url"]),
        &blake3::hash(b"project cover").to_hex()[..8]
    );
    assert_eq!(detail["cover_url"], expected);
    let page = ok(f.app.get("/api/gallery/projects").await, 200).await;
    assert_eq!(page["items"][0]["cover_url"], expected);
    assert_eq!(f.app.get(&expected).await.status(), 200);
}

#[tokio::test]
async fn 基础版自动合并剧集且旧接口与级联不可绕过() {
    let f = Fixture::new("basic").await;
    let me = ok(f.app.get_auth("/api/me", &f.user).await, 200).await;
    assert_eq!(me["auto_share_gallery"], true);
    assert_eq!(me["can_disable_auto_share_gallery"], false);
    assert_eq!(
        ok(
            f.app
                .patch_json_auth(PREF, json!({"auto_share_gallery":false}), &f.user)
                .await,
            403
        )
        .await["code"],
        "auto_share_required_by_plan"
    );
    ok(
        f.app
            .patch_json_auth(PREF, json!({"auto_share_gallery":true}), &f.user)
            .await,
        200,
    )
    .await;
    let folder = ok(
        f.app
            .post_json_auth("/api/folders", json!({"name":"文件夹"}), &f.user)
            .await,
        201,
    )
    .await;
    let p = f.project(Some(s(&folder["id"]))).await;
    let cid = s(&p["canvases"][0]["id"]);
    assert_eq!(f.detail(&p).await.status(), 404);
    let r = f.request(&p, cid).await;
    let first = f.finish(r.clone()).await;
    assert_eq!(first["auto_share_applied"], true);
    let share = f.share(&p).await;
    assert_eq!(share["can_toggle_share"], false);
    assert_eq!(share["share"]["author_name"], "创作者");
    let usage = ok(f.app.get_auth("/api/me", &f.user).await, 200).await;
    assert_eq!(usage["bytes_used"], 2);
    assert_eq!(usage["push_used"], 0);
    let cid2 = f.more(&p).await;
    let r2 = f.request(&p, &cid2).await;
    f.finish(r2).await;
    let after = f.share(&p).await;
    assert_eq!(share["share"]["share_id"], after["share"]["share_id"]);
    assert_eq!(after["share"]["canvas_ids"].as_array().unwrap().len(), 2);
    assert_eq!(ok(f.detail(&p).await, 200).await["canvas_count"], 2);
    f.toggle(&p, false, &f.user, 403).await;
    let url = format!("/api/projects/{}/share", s(&p["id"]));
    assert_eq!(f.app.delete_auth(&url, &f.user).await.status(), 403);
    let body =
        json!({"title":"x","description":"","author_name":"x","canvas_ids":[cid],"cover":null});
    assert_eq!(
        f.app
            .client
            .put(f.app.url(&url))
            .bearer_auth(&f.user)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        f.app
            .patch_json_auth(
                &format!("/api/canvases/{cid}"),
                json!({"name":"不应修改", "visibility":"private"}),
                &f.user
            )
            .await
            .status(),
        403
    );
    assert_eq!(
        f.app
            .patch_json_auth(
                &format!("/api/folders/{}?apply_to_children=true", s(&folder["id"])),
                json!({"name":"不应修改", "visibility":"private"}),
                &f.user
            )
            .await
            .status(),
        403
    );
    let unchanged = ok(
        f.app
            .get_auth(&format!("/api/projects/{}", s(&p["id"])), &f.user)
            .await,
        200,
    )
    .await;
    assert_eq!(unchanged["canvases"][0]["name"], p["canvases"][0]["name"]);
    assert_eq!(f.finish(r.clone()).await["already_processed"], true);
    assert_eq!(
        f.app
            .delete_auth(
                &format!("/api/folders/{}?recursive=true", s(&folder["id"])),
                &f.user
            )
            .await
            .status(),
        200
    );
    assert_eq!(f.detail(&p).await.status(), 404);
    assert_eq!(
        f.app.post_json_auth(COMPLETE, r, &f.worker).await.status(),
        404
    );
    let pid = ProjectId::parse(s(&p["id"])).unwrap();
    for prefix in [
        keys::gallery_ready_prefix(pid),
        keys::gallery_done_prefix(pid),
    ] {
        assert!(
            f.app
                .store
                .scan_prefix(Cf::Meta, prefix)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        f.app
            .store
            .get(Cf::Meta, keys::gallery_control_key(pid))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn 付费用户全局开关单独关闭与重试保持用户意图() {
    let f = Fixture::new("pro").await;
    let p = f.project(None).await;
    let cid = s(&p["canvases"][0]["id"]);
    ok(
        f.app
            .patch_json_auth(PREF, json!({"auto_share_gallery":false}), &f.user)
            .await,
        200,
    )
    .await;
    let r = f.request(&p, cid).await;
    assert_eq!(f.finish(r.clone()).await["auto_share_applied"], false);
    assert_eq!(f.detail(&p).await.status(), 404);
    ok(
        f.app
            .patch_json_auth(PREF, json!({"auto_share_gallery":true}), &f.user)
            .await,
        200,
    )
    .await;
    assert_eq!(f.finish(r.clone()).await["auto_share_applied"], false);
    assert_eq!(f.detail(&p).await.status(), 404);
    let opened = f.toggle(&p, true, &f.user, 200).await;
    let repeat = f.toggle(&p, true, &f.user, 200).await;
    assert_eq!(opened["share"]["share_id"], repeat["share"]["share_id"]);
    // 关闭全局不撤回已共享作品。
    ok(
        f.app
            .patch_json_auth(PREF, json!({"auto_share_gallery":false}), &f.user)
            .await,
        200,
    )
    .await;
    assert_eq!(f.detail(&p).await.status(), 200);
    let cid2 = f.more(&p).await;
    let r2 = f.request(&p, &cid2).await;
    assert_eq!(f.finish(r2.clone()).await["auto_share_applied"], false);
    assert_eq!(ok(f.detail(&p).await, 200).await["canvas_count"], 1);
    let closed = f.toggle(&p, false, &f.user, 200).await;
    assert_eq!(closed["suppression_reason"], "owner");
    ok(
        f.app
            .patch_json_auth(PREF, json!({"auto_share_gallery":true}), &f.user)
            .await,
        200,
    )
    .await;
    let mut r3 = r2.clone();
    r3["generation_id"] = json!(keys::GenerationId::new().to_text());
    assert_eq!(f.finish(r3).await["auto_share_applied"], false);
    let retry = f.finish(r2).await;
    assert_eq!(retry["current_is_shared"], false);
    assert_eq!(f.detail(&p).await.status(), 404);
    let reopen = f.toggle(&p, true, &f.user, 200).await;
    assert_ne!(opened["share"]["share_id"], reopen["share"]["share_id"]);
    assert_eq!(reopen["share"]["canvas_ids"].as_array().unwrap().len(), 2);
    // 私有操作现在表达整部作品下架。
    ok(
        f.app
            .patch_json_auth(
                &format!("/api/canvases/{cid}"),
                json!({"visibility":"private"}),
                &f.user,
            )
            .await,
        200,
    )
    .await;
    assert_eq!(f.share(&p).await["suppression_reason"], "owner");
    assert_eq!(f.detail(&p).await.status(), 404);
}

#[tokio::test]
async fn 降级生效但管理员下架不可被自动发布覆盖() {
    let f = Fixture::new("max").await;
    let p = f.project(None).await;
    let cid = s(&p["canvases"][0]["id"]);
    let mut r = f.request(&p, cid).await;
    f.finish(r.clone()).await;
    f.toggle(&p, false, &f.user, 200).await;
    ok(
        f.app
            .patch_json_auth(PREF, json!({"auto_share_gallery":false}), &f.user)
            .await,
        200,
    )
    .await;
    f.plan("basic").await;
    assert_eq!(f.detail(&p).await.status(), 404);
    r["generation_id"] = json!(keys::GenerationId::new().to_text());
    assert_eq!(f.finish(r.clone()).await["auto_share_applied"], true);
    assert_eq!(f.share(&p).await["auto_share_suppressed"], false);
    f.toggle(&p, false, &f.admin, 200).await;
    r["generation_id"] = json!(keys::GenerationId::new().to_text());
    assert_eq!(f.finish(r.clone()).await["auto_share_applied"], false);
    f.plan("pro").await;
    assert_eq!(
        ok(f.app.get_auth("/api/me", &f.user).await, 200).await["auto_share_gallery"],
        false
    );
    f.toggle(&p, true, &f.user, 403).await;
    assert_eq!(f.share(&p).await["can_toggle_share"], false);
    f.toggle(&p, true, &f.admin, 200).await;
    assert_eq!(f.detail(&p).await.status(), 200);
}

#[tokio::test]
async fn 完成通知鉴权归属严格请求与版本冲突() {
    let f = Fixture::new("basic").await;
    let p = f.project(None).await;
    let cid = s(&p["canvases"][0]["id"]);
    let r = f.request(&p, cid).await;
    let reader = f.app.signup("reader", "password123").await;
    let agent = ok(
        f.app
            .post_json_auth(
                &format!("/api/projects/{}/agent-tokens", s(&p["id"])),
                json!({}),
                &f.user,
            )
            .await,
        201,
    )
    .await;
    for token in [&f.user, &f.admin, &reader] {
        let denied = ok(f.app.post_json_auth(COMPLETE, r.clone(), token).await, 403).await;
        assert_eq!(denied["code"], "gallery_publisher_required");
        assert!(s(&denied["message"]).contains("gallery.publisher_usernames"));
    }
    let denied = ok(
        f.app
            .post_json_auth(COMPLETE, r.clone(), s(&agent["token"]))
            .await,
        403,
    )
    .await;
    assert_eq!(denied["code"], "forbidden");
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, r.clone(), "bad")
            .await
            .status(),
        401
    );
    for field in ["task_user_id", "project_id", "canvas_id"] {
        let mut bad = r.clone();
        bad[field] = json!(Uid::new().to_text());
        assert_eq!(
            f.app
                .post_json_auth(COMPLETE, bad, &f.worker)
                .await
                .status(),
            404
        );
    }
    let mut resource = r.clone();
    resource["canvas_id"] = p["resources"]["id"].clone();
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, resource, &f.worker)
            .await
            .status(),
        404
    );
    let mut bad = r.clone();
    bad["auto_share"] = json!(true);
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, bad, &f.worker)
            .await
            .status(),
        400
    );
    let mut stale = r.clone();
    stale["entry_hash"] = json!("0".repeat(64));
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, stale, &f.worker)
            .await
            .status(),
        409
    );
    let mut cover = r.clone();
    cover["cover_path"] = json!("../bad.png");
    f.finish(cover.clone()).await;
    assert!(f.share(&p).await["share"]["cover"].is_null());
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, r.clone(), &f.worker)
            .await
            .status(),
        409
    );
    // 大小写规范化不会产生另一份幂等摘要。
    cover["entry_hash"] = json!(s(&cover["entry_hash"]).to_ascii_uppercase());
    assert_eq!(f.finish(cover).await["already_processed"], true);
    let worker_uid = identity::find_by_username(&f.app.store, "worker")
        .await
        .unwrap()
        .unwrap()
        .0;
    ok(
        f.app
            .patch_json_auth(
                &format!("/api/admin/users/{worker_uid}"),
                json!({"disabled":true}),
                &f.admin,
            )
            .await,
        200,
    )
    .await;
    assert_eq!(
        f.app.post_json_auth(COMPLETE, r, &f.worker).await.status(),
        401
    );
    for body in [
        json!({}),
        json!({"auto_share_gallery":null}),
        json!({"auto_share_gallery":true,"extra":1}),
    ] {
        assert_eq!(
            f.app.patch_json_auth(PREF, body, &f.user).await.status(),
            400
        );
    }
}

#[tokio::test]
async fn 损坏状态拒绝发布且恢复失败不部分提交() {
    let f = Fixture::new("pro").await;
    let p = f.project(None).await;
    let cid = s(&p["canvases"][0]["id"]);
    let r = f.request(&p, cid).await;
    let uid = Uid::parse(&f.uid).unwrap();
    let pid = ProjectId::parse(s(&p["id"])).unwrap();
    f.app
        .store
        .put(
            Cf::Meta,
            keys::gallery_preference_key(uid),
            b"{broken".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, r.clone(), &f.worker)
            .await
            .status(),
        500
    );
    assert!(gallery::get(&f.app.store, pid).await.unwrap().is_none());
    assert!(
        f.app
            .store
            .scan_prefix(Cf::Meta, keys::gallery_done_prefix(pid))
            .await
            .unwrap()
            .is_empty()
    );
    let before = records::decode::<records::Canvas>(
        &f.app
            .store
            .get(
                Cf::Meta,
                keys::canvas_key(uid, CanvasId::parse(cid).unwrap()),
            )
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(before.entry.is_none());
    assert_eq!(before.visibility, records::Visibility::Private);
    f.app
        .store
        .put(
            Cf::Meta,
            keys::gallery_preference_key(uid),
            br#"{"v":1,"auto_share_gallery":true,"updated_at":0}"#.to_vec(),
        )
        .await
        .unwrap();
    f.finish(r.clone()).await;
    f.toggle(&p, false, &f.user, 200).await;
    // 内容被覆盖，旧完成版本不再可恢复，不能提前清理下架标记。
    ok(
        f.app
            .put_bytes_auth(
                &format!("/api/canvases/{cid}/files/story.json"),
                b"changed".to_vec(),
                &f.user,
            )
            .await,
        200,
    )
    .await;
    f.toggle(&p, true, &f.user, 409).await;
    assert_eq!(f.share(&p).await["suppression_reason"], "owner");
    assert_eq!(f.finish(r).await["current_is_shared"], false);
    assert_eq!(
        f.app
            .delete_auth(&format!("/api/canvases/{cid}"), &f.user)
            .await
            .status(),
        200
    );
    assert!(
        f.app
            .store
            .get(
                Cf::Meta,
                keys::gallery_ready_key(pid, CanvasId::parse(cid).unwrap())
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.app
            .store
            .scan_prefix(Cf::Meta, keys::gallery_done_prefix(pid))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn 并发完成合并且关闭删除与回调按锁序执行() {
    let f = Fixture::new("pro").await;
    let p = f.project(None).await;
    let cid = s(&p["canvases"][0]["id"]);
    let cid2 = f.more(&p).await;
    let a = f.request(&p, cid).await;
    let b = f.request(&p, &cid2).await;
    let (a_result, b_result) = tokio::join!(f.finish(a.clone()), f.finish(b));
    assert_eq!(a_result["current_is_shared"], true);
    assert_eq!(b_result["current_is_shared"], true);
    assert_eq!(
        f.share(&p).await["share"]["canvas_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut fresh = a.clone();
    fresh["generation_id"] = json!(keys::GenerationId::new().to_text());
    tokio::join!(f.finish(fresh), f.toggle(&p, false, &f.user, 200));
    assert_eq!(f.detail(&p).await.status(), 404);
    assert_eq!(f.finish(a.clone()).await["current_is_shared"], false);
    let mut fresh = a;
    fresh["generation_id"] = json!(keys::GenerationId::new().to_text());
    let url = format!("/api/projects/{}", s(&p["id"]));
    let (response, deleted) = tokio::join!(
        f.app.post_json_auth(COMPLETE, fresh, &f.worker),
        f.app.delete_auth(&url, &f.user)
    );
    assert!([200, 404].contains(&response.status().as_u16()));
    assert_eq!(deleted.status(), 204);
    assert_eq!(f.detail(&p).await.status(), 404);
}

#[tokio::test]
async fn 新状态重开持久且缺失新键兼容旧共享() {
    use one_click_cut_server::domain::{canvas, space};
    let db = common::temp_store();
    identity::ensure_admin(&db.store, "admin", 0).await.unwrap();
    let uid = identity::find_by_username(&db.store, "admin")
        .await
        .unwrap()
        .unwrap()
        .0;
    let (pid, _, cid, _) =
        space::create_project(&db.store, uid, keys::ParentId::Root, "旧作品", None, 1)
            .await
            .unwrap()
            .unwrap();
    canvas::put_file(
        &db.store,
        uid,
        cid,
        "entry.json",
        "application/json",
        b"{}",
        1,
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
        Some(records::Visibility::Public),
        Some("entry.json"),
        None,
        1,
    )
    .await
    .unwrap()
    .unwrap();
    let (old, _) = gallery::put(
        &db.store,
        uid,
        pid,
        records::ShareConfig {
            title: "旧标题".into(),
            description: "旧简介".into(),
            author_name: "旧作者".into(),
            canvas_ids: vec![cid],
            cover: None,
        },
        1,
    )
    .await
    .unwrap();
    let db = db.reopen();
    assert_eq!(
        gallery_policy::preferences(&db.store, uid).await.unwrap(),
        (true, true)
    );
    assert_eq!(
        gallery::get(&db.store, pid)
            .await
            .unwrap()
            .unwrap()
            .share_id,
        old.share_id
    );
    gallery_policy::toggle(&db.store, uid, uid, pid, false, 2)
        .await
        .unwrap();
    let db = db.reopen();
    gallery_policy::toggle(&db.store, uid, uid, pid, true, 3)
        .await
        .unwrap();
    assert_eq!(
        gallery::get(&db.store, pid)
            .await
            .unwrap()
            .unwrap()
            .config
            .title,
        "旧标题"
    );
    let gid = keys::GenerationId::new();
    let request = || gallery_policy::Completion {
        generation_id: gid,
        task_user_id: uid,
        project_id: pid,
        canvas_id: cid,
        entry: "entry.json".into(),
        entry_hash: blake3::hash(b"{}").to_hex().to_string(),
        cover_path: None,
    };
    gallery_policy::complete(&db.store, request(), 4)
        .await
        .unwrap();
    gallery_policy::set_preference(&db.store, uid, false, 5)
        .await
        .unwrap();
    gallery_policy::toggle(&db.store, uid, uid, pid, false, 5)
        .await
        .unwrap();
    let db = db.reopen();
    assert_eq!(
        gallery_policy::preferences(&db.store, uid).await.unwrap(),
        (false, true)
    );
    assert_eq!(
        gallery_policy::complete(&db.store, request(), 6)
            .await
            .unwrap(),
        (true, true)
    );
    assert!(gallery::get(&db.store, pid).await.unwrap().is_none());
}

#[tokio::test]
async fn 新接口文档包含鉴权与请求响应字段() {
    let app = common::spawn_app().await;
    let doc = ok(app.get("/api-docs/openapi.json").await, 200).await;
    for (path, method) in [
        (COMPLETE, "post"),
        (PREF, "patch"),
        ("/api/projects/{id}/sharing", "patch"),
    ] {
        assert_eq!(
            doc["paths"][path][method]["security"][0],
            json!({"bearer_jwt":[]})
        );
        assert!(doc["paths"][path][method]["requestBody"].is_object());
    }
    assert_eq!(
        doc["components"]["schemas"]["MeResponse"]["properties"]["auto_share_gallery"]["type"],
        "boolean"
    );
    assert!(
        doc["components"]["schemas"]["ShareResponse"]["properties"]["can_toggle_share"].is_object()
    );
}

#[tokio::test]
async fn 发布校验失败不会提交画布及回执且大请求被拒绝() {
    let f = Fixture::new("pro").await;
    let p = f.project(None).await;
    let cid = CanvasId::parse(s(&p["canvases"][0]["id"])).unwrap();
    let pid = ProjectId::parse(s(&p["id"])).unwrap();
    let uid = Uid::parse(&f.uid).unwrap();
    let r = f.request(&p, &cid.to_text()).await;
    // 合法可解码但不符合共享校验的旧配置：在准备画布修改之后失败。
    let share = records::ProjectShare {
        v: 1,
        project_id: pid,
        owner_uid: uid,
        share_id: keys::ShareId::new(),
        shared_at: 1,
        updated_at: 1,
        config: records::ShareConfig {
            title: "".into(),
            description: String::new(),
            author_name: "作者".into(),
            canvas_ids: vec![cid],
            cover: None,
        },
    };
    f.app
        .store
        .put(
            Cf::Meta,
            keys::project_share_key(pid),
            records::encode(&share).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, r.clone(), &f.worker)
            .await
            .status(),
        400
    );
    assert!(
        f.app
            .store
            .scan_prefix(Cf::Meta, keys::gallery_done_prefix(pid))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        f.app
            .store
            .get(Cf::Meta, keys::gallery_ready_key(pid, cid))
            .await
            .unwrap()
            .is_none()
    );
    let canvas = one_click_cut_server::domain::space::get_canvas(&f.app.store, uid, cid)
        .await
        .unwrap()
        .unwrap();
    assert!(canvas.entry.is_none());
    assert_eq!(canvas.visibility, records::Visibility::Private);
    let mut batch = one_click_cut_server::storage::Batch::new();
    gallery::stage_remove(&f.app.store, &mut batch, pid)
        .await
        .unwrap();
    f.app.store.write(batch).await.unwrap();
    // 不支持的新状态版本不能按默认允许发布。
    f.app
        .store
        .put(
            Cf::Meta,
            keys::gallery_control_key(pid),
            br#"{"v":2,"suppression":"none","last_share_config":null,"updated_at":0}"#.to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, r.clone(), &f.worker)
            .await
            .status(),
        500
    );
    let mut batch = one_click_cut_server::storage::Batch::new();
    batch.delete(Cf::Meta, keys::gallery_control_key(pid));
    f.app.store.write(batch).await.unwrap();
    let mut too_big = r.clone();
    too_big["cover_path"] = json!("x".repeat(33000));
    assert_eq!(
        f.app
            .post_json_auth(COMPLETE, too_big, &f.worker)
            .await
            .status(),
        413
    );
    let response = f.finish(r).await;
    assert_eq!(response["auto_share_applied"], true);
    let stats = ok(f.app.get_auth("/api/admin/stats", &f.admin).await, 200).await;
    assert_eq!(stats["gallery_completion_receipts"], 1);
}

#[test]
fn 发布名单配置默认关闭且用户名必须合法() {
    use one_click_cut_server::Config;
    let cfg = Config::default();
    assert!(cfg.gallery.publisher_usernames.is_empty());
    let mut cfg: Config = toml::from_str("[gallery]\npublisher_usernames=['worker1']\n").unwrap();
    cfg.gallery.validate().unwrap();
    assert!(cfg.task_queue.reader_usernames.is_empty());
    cfg.gallery.publisher_usernames = vec!["bad name".into()];
    assert!(cfg.gallery.validate().is_err());
}
