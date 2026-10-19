//! 项目级 Agent Token 的生命周期与权限测试。
#[path = "../common/mod.rs"]
mod common;
use serde_json::{Value, json};

async fn create(app: &common::TestApp, user: &str) -> Value {
    let r = app
        .post_json_auth("/api/projects", json!({"name":"动画项目"}), user)
        .await;
    assert_eq!(r.status(), 201);
    r.json().await.unwrap()
}
async fn issue(app: &common::TestApp, user: &str, pid: &str, body: Value) -> Value {
    let r = app
        .post_json_auth(&format!("/api/projects/{pid}/agent-tokens"), body, user)
        .await;
    assert_eq!(r.status(), 201);
    r.json().await.unwrap()
}
fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[tokio::test]
async fn 签发只返回一次明文且列表不回显() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let pid = s(&p["id"]);
    let token = issue(&app, &user, pid, json!({})).await;
    let plaintext = s(&token["token"]);
    assert!(plaintext.starts_with("at_"));
    assert_eq!(plaintext.len(), 35);
    assert_eq!(token["canvas_files_write"], true);
    assert_eq!(token["canvas_files_delete"], false);
    let list: Value = app
        .get_auth(&format!("/api/projects/{pid}/agent-tokens"), &user)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list["tokens"][0]["project_id"], pid);
    assert!(list["tokens"][0].get("token").is_none());
    assert_eq!(list["tokens"][0]["prefix"], token["prefix"]);
}

#[tokio::test]
async fn 同项目多画布可写跨项目不可写() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let a = create(&app, &user).await;
    let b = create(&app, &user).await;
    let pid = s(&a["id"]);
    let first = s(&a["canvases"][0]["id"]);
    let other = s(&b["canvases"][0]["id"]);
    let agent =
        s(&issue(&app, &user, pid, json!({"canvas_create":true})).await["token"]).to_owned();
    let second: Value = app
        .post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"预告片"}),
            &agent,
        )
        .await
        .json()
        .await
        .unwrap();
    for cid in [first, s(&second["id"])] {
        assert_eq!(
            app.put_bytes_auth(
                &format!("/api/canvases/{cid}/files/a.txt"),
                b"ok".to_vec(),
                &agent
            )
            .await
            .status(),
            200
        );
    }
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{other}/files/a.txt"),
            b"x".to_vec(),
            &agent
        )
        .await
        .status(),
        404
    );
}

#[tokio::test]
async fn 公共草稿与制作写权限独立() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let pid = s(&p["id"]);
    let production = s(&p["canvases"][0]["id"]);
    let maker = s(&issue(&app, &user, pid, json!({})).await["token"]).to_owned();
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/projects/{pid}/resources/files/a.md"),
            b"x".to_vec(),
            &maker
        )
        .await
        .status(),
        403
    );
    let editor = s(&issue(
        &app,
        &user,
        pid,
        json!({"canvas_files_write":false,"constraints_write":true}),
    )
    .await["token"])
    .to_owned();
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/projects/{pid}/resources/files/a.md"),
            b"x".to_vec(),
            &editor
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{production}/files/a.md"),
            b"x".to_vec(),
            &editor
        )
        .await
        .status(),
        403
    );
}

#[tokio::test]
async fn 吊销立即生效且其他用户不能吊销() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    let bob = app.signup("bob", "password123").await;
    let p = create(&app, &alice).await;
    let pid = s(&p["id"]);
    let canvas = s(&p["canvases"][0]["id"]);
    let t = issue(&app, &alice, pid, json!({})).await;
    let id = s(&t["token_id"]);
    let agent = s(&t["token"]);
    assert_eq!(
        app.delete_auth(&format!("/api/agent-tokens/{id}"), &bob)
            .await
            .status(),
        404
    );
    assert_eq!(
        app.delete_auth(&format!("/api/agent-tokens/{id}"), &alice)
            .await
            .status(),
        204
    );
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{canvas}/files/a.md"),
            b"x".to_vec(),
            agent
        )
        .await
        .status(),
        401
    );
}

#[tokio::test]
async fn agent不能删除项目和管理凭证() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let pid = s(&p["id"]);
    let agent = s(&issue(&app, &user, pid, json!({})).await["token"]).to_owned();
    assert_eq!(app.get_auth("/api/tree", &agent).await.status(), 403);
    assert_eq!(
        app.post_json_auth(
            &format!("/api/projects/{pid}/agent-tokens"),
            json!({}),
            &agent
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}"), &agent)
            .await
            .status(),
        403
    );
}

#[tokio::test]
async fn agent可改绑定项目和制作画布名称且不依赖文件写权限() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let pid = s(&p["id"]);
    let cid = s(&p["canvases"][0]["id"]);
    let second: Value = app
        .post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"第二集"}),
            &user,
        )
        .await
        .json()
        .await
        .unwrap();
    for perms in [json!({}), json!({"canvas_files_write":false})] {
        let t = issue(&app, &user, pid, perms).await;
        let agent = s(&t["token"]);
        for path in [
            format!("/api/projects/{pid}"),
            format!("/api/canvases/{cid}"),
            format!("/api/canvases/{}", s(&second["id"])),
        ] {
            let response = app
                .patch_json_auth(&path, json!({"name":"新名称"}), agent)
                .await;
            assert_eq!(response.status(), 200, "{path}");
            let updated: Value = response.json().await.unwrap();
            assert_eq!(updated["name"], "新名称");
        }
    }
    let updated: Value = app
        .get_auth(&format!("/api/projects/{pid}"), &user)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(updated["name"], "新名称");
    for canvas in updated["canvases"].as_array().unwrap() {
        assert_eq!(canvas["name"], "新名称");
    }
    assert_eq!(updated["canvases"][0]["slug"], p["canvases"][0]["slug"]);
    assert_eq!(
        updated["canvases"][0]["base_url"],
        p["canvases"][0]["base_url"]
    );
    assert_eq!(updated["resources"], p["resources"]);
}

#[tokio::test]
async fn agent改名拒绝其他字段且不部分写入() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let pid = s(&p["id"]);
    let cid = s(&p["canvases"][0]["id"]);
    let t = issue(&app, &user, pid, json!({})).await;
    let agent = s(&t["token"]);
    for (path, bodies) in [
        (
            format!("/api/projects/{pid}"),
            vec![
                json!({"name":"不应保存","parent_id":""}),
                json!({"name":"不应保存","resources_name":"不应保存"}),
                json!({"parent_id":""}),
                json!({}),
                json!({"name":null}),
            ],
        ),
        (
            format!("/api/canvases/{cid}"),
            vec![
                json!({"name":"不应保存","visibility":"public"}),
                json!({"name":"不应保存","entry":"timeline.json"}),
                json!({"name":"不应保存","sort_order":42}),
                json!({"sort_order":42}),
                json!({}),
                json!({"name":null}),
            ],
        ),
    ] {
        for body in bodies {
            assert_eq!(
                app.patch_json_auth(&path, body.clone(), agent)
                    .await
                    .status(),
                403,
                "{path}: {body}"
            );
        }
        assert_eq!(
            app.patch_json_auth(&path, json!({"name":""}), agent)
                .await
                .status(),
            400
        );
        assert_eq!(
            app.patch_json_auth(&path, json!({"name":"不应保存","unknown":true}), agent)
                .await
                .status(),
            422
        );
    }
    let after: Value = app
        .get_auth(&format!("/api/projects/{pid}"), &user)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after, p, "拒绝请求不能修改任何项目或画布数据");
    // 用户 JWT 仍可修改原有字段。
    assert_eq!(
        app.patch_json_auth(
            &format!("/api/projects/{pid}"),
            json!({"name":"用户改名","parent_id":"","resources_name":"公共区新名"}),
            &user
        )
        .await
        .status(),
        200
    );
    assert_eq!(app.patch_json_auth(&format!("/api/canvases/{cid}"),
        json!({"name":"用户改名","visibility":"private","entry":"timeline.json","sort_order":42}), &user).await.status(), 200);
}

#[tokio::test]
async fn agent改名不能跨项目跨用户或修改公共区() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    let bob = app.signup("bob", "password123").await;
    let p = create(&app, &alice).await;
    let t = issue(&app, &alice, s(&p["id"]), json!({})).await;
    let agent = s(&t["token"]);
    for owner in [&alice, &bob] {
        let other = create(&app, owner).await;
        for path in [
            format!("/api/projects/{}", s(&other["id"])),
            format!("/api/canvases/{}", s(&other["canvases"][0]["id"])),
        ] {
            assert_eq!(
                app.patch_json_auth(&path, json!({"name":"越权"}), agent)
                    .await
                    .status(),
                404
            );
        }
        let after: Value = app
            .get_auth(&format!("/api/projects/{}", s(&other["id"])), owner)
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(after, other);
    }
    assert_eq!(
        app.patch_json_auth(
            &format!("/api/canvases/{}", s(&p["resources"]["id"])),
            json!({"name":"越权"}),
            agent
        )
        .await
        .status(),
        404
    );
}

#[tokio::test]
async fn agent吊销后两个改名接口均拒绝() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let t = issue(&app, &user, s(&p["id"]), json!({})).await;
    assert_eq!(
        app.delete_auth(&format!("/api/agent-tokens/{}", s(&t["token_id"])), &user)
            .await
            .status(),
        204
    );
    for path in [
        format!("/api/projects/{}", s(&p["id"])),
        format!("/api/canvases/{}", s(&p["canvases"][0]["id"])),
    ] {
        assert_eq!(
            app.patch_json_auth(&path, json!({"name":"不能改名"}), s(&t["token"]))
                .await
                .status(),
            401
        );
    }
}

#[tokio::test]
async fn token有效期和权限字段可配置() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let pid = s(&p["id"]);
    for ttl in [0u64, 31 * 86400] {
        assert_eq!(
            app.post_json_auth(
                &format!("/api/projects/{pid}/agent-tokens"),
                json!({"ttl_secs":ttl}),
                &user
            )
            .await
            .status(),
            400
        );
    }
    assert_ne!(
        app.post_json_auth(
            &format!("/api/projects/{pid}/agent-tokens"),
            json!({"write":false}),
            &user
        )
        .await
        .status(),
        201
    );
    let t = issue(
        &app,
        &user,
        pid,
        json!({"canvas_files_write":false,"canvas_files_delete":true,"ttl_secs":3600}),
    )
    .await;
    assert_eq!(t["canvas_files_write"], false);
    assert_eq!(t["canvas_files_delete"], true);
}

#[tokio::test]
async fn 删除公共资料需独立权限() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = create(&app, &user).await;
    let pid = s(&p["id"]);
    let path = format!("/api/projects/{pid}/resources/files/a.md");
    assert_eq!(
        app.put_bytes_auth(&path, b"draft".to_vec(), &user)
            .await
            .status(),
        200
    );
    let remover = s(&issue(
        &app,
        &user,
        pid,
        json!({"canvas_files_write":false,"constraints_delete":true}),
    )
    .await["token"])
    .to_owned();
    assert_eq!(app.delete_auth(&path, &remover).await.status(), 204);
}
