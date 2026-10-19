//! 普通文件夹只组织项目；制作画布属于项目。
#[path = "../common/mod.rs"]
mod common;
use serde_json::{Value, json};
fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}
async fn folder(app: &common::TestApp, user: &str, name: &str, parent: Option<&str>) -> Value {
    let r = app
        .post_json_auth(
            "/api/folders",
            json!({"name":name,"parent_id":parent}),
            user,
        )
        .await;
    assert_eq!(r.status(), 201);
    r.json().await.unwrap()
}
async fn project(app: &common::TestApp, user: &str, name: &str, parent: Option<&str>) -> Value {
    let r = app
        .post_json_auth(
            "/api/projects",
            json!({"name":name,"parent_id":parent}),
            user,
        )
        .await;
    assert_eq!(r.status(), 201);
    r.json().await.unwrap()
}
async fn nodes(app: &common::TestApp, user: &str, parent: Option<&str>) -> Vec<Value> {
    let path = parent
        .map(|id| format!("/api/tree?parent={id}"))
        .unwrap_or("/api/tree".into());
    let r = app.get_auth(&path, user).await;
    assert_eq!(r.status(), 200);
    r.json::<Value>().await.unwrap()["nodes"]
        .as_array()
        .unwrap()
        .clone()
}

#[tokio::test]
async fn 树只显示文件夹和项目() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let f = folder(&app, &user, "作品集", None).await;
    let p = project(&app, &user, "森林邮局", Some(s(&f["id"]))).await;
    let root = nodes(&app, &user, None).await;
    assert_eq!(root.len(), 1);
    assert_eq!(root[0]["kind"], "folder");
    let children = nodes(&app, &user, Some(s(&f["id"]))).await;
    assert_eq!(children.len(), 1);
    assert_eq!(children[0]["kind"], "project");
    assert_eq!(children[0]["id"], p["id"]);
    assert!(children[0].get("slug").is_none());
    let pid = s(&p["id"]);
    let detail: Value = app
        .get_auth(&format!("/api/projects/{pid}"), &user)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["canvases"].as_array().unwrap().len(), 1);
    assert_eq!(detail["resources"]["role"], "resources");
}

#[tokio::test]
async fn 项目移动和改名不改变画布slug() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let f = folder(&app, &user, "目标", None).await;
    let p = project(&app, &user, "原名", None).await;
    let pid = s(&p["id"]);
    let slug = p["canvases"][0]["slug"].clone();
    let r = app
        .patch_json_auth(
            &format!("/api/projects/{pid}"),
            json!({"name":"新名","parent_id":f["id"]}),
            &user,
        )
        .await;
    assert_eq!(r.status(), 200);
    let updated: Value = r.json().await.unwrap();
    assert_eq!(updated["canvases"][0]["slug"], slug);
    assert_eq!(nodes(&app, &user, Some(s(&f["id"]))).await[0]["id"], pid);
}

#[tokio::test]
async fn 新画布必须属于项目且公共区不可删除() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "项目", None).await;
    let pid = s(&p["id"]);
    let r = app
        .post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"预告片"}),
            &user,
        )
        .await;
    assert_eq!(r.status(), 201);
    let canvas: Value = r.json().await.unwrap();
    assert_eq!(canvas["project_id"], pid);
    assert_eq!(
        app.delete_auth(
            &format!("/api/canvases/{}", s(&p["resources"]["id"])),
            &user
        )
        .await
        .status(),
        404
    );
    assert_eq!(
        app.post_json_auth("/api/canvases", json!({"name":"游离画布"}), &user)
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn 删除最后一个制作画布仍保留项目() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "项目", None).await;
    let pid = s(&p["id"]);
    let cid = s(&p["canvases"][0]["id"]);
    assert_eq!(
        app.delete_auth(&format!("/api/canvases/{cid}"), &user)
            .await
            .status(),
        200
    );
    let detail: Value = app
        .get_auth(&format!("/api/projects/{pid}"), &user)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(detail["canvases"].as_array().unwrap().len(), 0);
    assert_eq!(detail["resources"]["role"], "resources");
}

#[tokio::test]
async fn 删除文件夹可级联项目且吊销凭证() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let f = folder(&app, &user, "分组", None).await;
    let fid = s(&f["id"]);
    let p = project(&app, &user, "项目", Some(fid)).await;
    let pid = s(&p["id"]);
    assert_eq!(
        app.delete_auth(&format!("/api/folders/{fid}"), &user)
            .await
            .status(),
        409
    );
    assert_eq!(
        app.delete_auth(&format!("/api/folders/{fid}?recursive=true"), &user)
            .await
            .status(),
        200
    );
    assert_eq!(
        app.get_auth(&format!("/api/projects/{pid}"), &user)
            .await
            .status(),
        404
    );
}

#[tokio::test]
async fn 跨用户项目返回404() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    let bob = app.signup("bob", "password123").await;
    let p = project(&app, &alice, "项目", None).await;
    let pid = s(&p["id"]);
    assert_eq!(
        app.get_auth(&format!("/api/projects/{pid}"), &bob)
            .await
            .status(),
        404
    );
    assert_eq!(
        app.post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"非法"}),
            &bob
        )
        .await
        .status(),
        404
    );
}
