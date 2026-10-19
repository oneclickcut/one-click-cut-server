//! 新项目模型的授权与公共资料回归测试。
#[path = "../common/mod.rs"]
mod common;
use serde_json::{Value, json};

async fn project(app: &common::TestApp, user: &str, name: &str) -> Value {
    let r = app
        .post_json_auth("/api/projects", json!({"name":name}), user)
        .await;
    assert_eq!(r.status(), 201);
    r.json().await.unwrap()
}
async fn token(app: &common::TestApp, user: &str, pid: &str, perms: Value) -> String {
    let r = app
        .post_json_auth(&format!("/api/projects/{pid}/agent-tokens"), perms, user)
        .await;
    assert_eq!(r.status(), 201);
    let v: Value = r.json().await.unwrap();
    v["token"].as_str().unwrap().to_owned()
}
fn id(v: &Value) -> &str {
    v.as_str().unwrap()
}

#[tokio::test]
async fn 项目上下文返回真实所有者且保持权限边界() {
    let app = common::spawn_app().await;
    let alice = app.signup("alice", "password123").await;
    let bob = app.signup("bob", "password123").await;
    let me: Value = app.get_auth("/api/me", &alice).await.json().await.unwrap();
    let p = project(&app, &alice, "所有者校验").await;
    let pid = id(&p["id"]);
    let agent = token(&app, &alice, pid, json!({"constraints_write":true})).await;
    let password = one_click_cut_server::domain::identity::ensure_admin(&app.store, "admin", 0)
        .await
        .unwrap()
        .unwrap();
    let login: Value = app.login("admin", &password).await.json().await.unwrap();
    let admin = id(&login["access_token"]);
    let admin_me: Value = app.get_auth("/api/me", admin).await.json().await.unwrap();
    assert_ne!(me["uid"], admin_me["uid"]);

    let route = format!("/api/projects/{pid}/context");
    for credential in [&alice[..], &agent[..], admin] {
        let response = app.get_auth(&route, credential).await;
        assert_eq!(response.status(), 200);
        let context: Value = response.json().await.unwrap();
        assert_eq!(context["owner_uid"], me["uid"]);
        assert_eq!(context["id"], p["id"]);
        assert_eq!(context["status"], "active");
        assert_eq!(context["resources"], p["resources"]);
        assert_eq!(context["canvases"], p["canvases"]);
        assert_eq!(context["permissions"]["constraints_write"], true);
        assert!(context.get("project").is_none(), "保持展开的上下文结构");
    }
    let readonly = token(
        &app,
        &alice,
        pid,
        json!({"canvas_files_write":false,"constraints_write":false}),
    )
    .await;
    let context: Value = app.get_auth(&route, &readonly).await.json().await.unwrap();
    assert_eq!(context["owner_uid"], me["uid"]);
    assert_eq!(context["permissions"]["constraints_write"], false);
    assert_eq!(context["permissions"]["canvas_files_write"], false);

    let other = project(&app, &bob, "其他用户项目").await;
    let other_agent = token(&app, &bob, id(&other["id"]), json!({})).await;
    for credential in [&bob, &other_agent] {
        let response = app.get_auth(&route, credential).await;
        assert_eq!(response.status(), 404);
        let body: Value = response.json().await.unwrap();
        assert!(body.get("owner_uid").is_none());
    }
    assert_eq!(app.get(&route).await.status(), 401);
    assert!(p.get("owner_uid").is_none(), "仅扩展受保护的上下文 DTO");
    let detail: Value = app
        .get_auth(&format!("/api/projects/{pid}"), &alice)
        .await
        .json()
        .await
        .unwrap();
    assert!(detail.get("owner_uid").is_none());
}

#[tokio::test]
async fn 项目上下文文档声明必填所有者字符串() {
    let app = common::spawn_app().await;
    let response = app.get("/api-docs/openapi.json").await;
    assert_eq!(response.status(), 200);
    let spec: Value = response.json().await.unwrap();
    let schema = &spec["components"]["schemas"]["ContextDto"];
    // flatten 的派生 schema 可以将新增字段放在 allOf 的 object 中。
    let object = if schema["properties"]["owner_uid"].is_object() {
        schema
    } else {
        schema["allOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["properties"]["owner_uid"].is_object())
            .unwrap()
    };
    assert_eq!(object["properties"]["owner_uid"]["type"], "string");
    assert!(
        object["required"]
            .as_array()
            .unwrap()
            .contains(&json!("owner_uid"))
    );
}

#[tokio::test]
async fn 项目初始化与跨本项目画布授权() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "森林邮局").await;
    let pid = id(&p["id"]);
    let initial = id(&p["canvases"][0]["id"]);
    assert_eq!(p["resources"]["role"], "resources");
    assert_eq!(p["canvases"].as_array().unwrap().len(), 1);
    assert!(p["canvases"][0].get("episode_number").is_none());
    let agent = token(&app, &user, pid, json!({"canvas_create":true})).await;
    let new: Value = app
        .post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"第02集"}),
            &agent,
        )
        .await
        .json()
        .await
        .unwrap();
    let second = id(&new["id"]);
    for cid in [initial, second] {
        assert_eq!(
            app.put_bytes_auth(
                &format!("/api/canvases/{cid}/files/a.txt"),
                b"ok".to_vec(),
                &agent
            )
            .await
            .status(),
            200
        )
    }
    let other = project(&app, &user, "另一个项目").await;
    let other_id = id(&other["canvases"][0]["id"]);
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{other_id}/files/a.txt"),
            b"x".to_vec(),
            &agent
        )
        .await
        .status(),
        404
    );
    assert_eq!(
        app.get_auth(
            &format!("/api/projects/{}/context", id(&other["id"])),
            &agent
        )
        .await
        .status(),
        404
    );
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/canvases/{}/files/a.txt", id(&p["resources"]["id"])),
            b"x".to_vec(),
            &agent
        )
        .await
        .status(),
        403
    );
}

#[tokio::test]
async fn 已取消的归档版本与集数不再开放() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "项目").await;
    let pid = id(&p["id"]);
    let cid = id(&p["canvases"][0]["id"]);
    let context: Value = app
        .get_auth(&format!("/api/projects/{pid}/context"), &user)
        .await
        .json()
        .await
        .unwrap();
    assert!(context.get("latest_revision_id").is_none());
    assert!(context["permissions"].get("constraints_publish").is_none());
    assert_eq!(
        app.get_auth(&format!("/api/projects/{pid}/constraint-revisions"), &user)
            .await
            .status(),
        404
    );
    assert_eq!(
        app.patch_json_auth(
            &format!("/api/projects/{pid}"),
            json!({"status":"archived"}),
            &user,
        )
        .await
        .status(),
        422
    );
    assert_eq!(
        app.patch_json_auth(
            &format!("/api/canvases/{cid}"),
            json!({"episode_number":2}),
            &user,
        )
        .await
        .status(),
        422
    );
    assert_eq!(
        app.post_json_auth(
            &format!("/api/projects/{pid}/agent-tokens"),
            json!({"constraints_publish":true}),
            &user,
        )
        .await
        .status(),
        422
    );
}

#[tokio::test]
async fn 创建重试返回同一个项目和画布() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let send = || {
        app.client
            .post(app.url("/api/projects"))
            .bearer_auth(&user)
            .header("idempotency-key", "project-001")
            .json(&json!({"name":"项目"}))
            .send()
    };
    let a: Value = send().await.unwrap().json().await.unwrap();
    let b: Value = send().await.unwrap().json().await.unwrap();
    assert_eq!(a["id"], b["id"]);
    assert_eq!(a["canvases"][0]["id"], b["canvases"][0]["id"]);
    let pid = id(&a["id"]);
    let send = || {
        app.client
            .post(app.url(&format!("/api/projects/{pid}/canvases")))
            .bearer_auth(&user)
            .header("idempotency-key", "episode-002")
            .json(&json!({"name":"第02集"}))
            .send()
    };
    let first: Value = send().await.unwrap().json().await.unwrap();
    let second: Value = send().await.unwrap().json().await.unwrap();
    assert_eq!(first["id"], second["id"]);
}

#[tokio::test]
async fn 删除画布保留项目凭证删除项目吊销凭证() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "项目").await;
    let pid = id(&p["id"]);
    let first = id(&p["canvases"][0]["id"]);
    let agent = token(&app, &user, pid, json!({"canvas_create":true})).await;
    assert_eq!(
        app.delete_auth(&format!("/api/canvases/{first}"), &user)
            .await
            .status(),
        200
    );
    let r = app
        .post_json_auth(
            &format!("/api/projects/{pid}/canvases"),
            json!({"name":"新集"}),
            &agent,
        )
        .await;
    assert_eq!(r.status(), 201);
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}"), &user)
            .await
            .status(),
        204
    );
    assert_eq!(
        app.get_auth(&format!("/api/projects/{pid}/context"), &agent)
            .await
            .status(),
        401
    );
}

#[tokio::test]
async fn 删除项目释放公共资料配额() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "项目").await;
    let pid = id(&p["id"]);
    assert_eq!(
        app.put_bytes_auth(
            &format!("/api/projects/{pid}/resources/files/a.md"),
            b"hello".to_vec(),
            &user
        )
        .await
        .status(),
        200
    );
    let me: Value = app.get_auth("/api/me", &user).await.json().await.unwrap();
    assert_eq!(me["bytes_used"], 5);
    assert_eq!(
        app.delete_auth(&format!("/api/projects/{pid}"), &user)
            .await
            .status(),
        204
    );
    let me: Value = app.get_auth("/api/me", &user).await.json().await.unwrap();
    assert_eq!(me["bytes_used"], 0);
}

#[tokio::test]
async fn 公共资料与画布单文件上传使用相同配置上限() {
    const MAX_SINGLE: usize = 3 << 20;
    let app = common::spawn_app_with(|config| config.upload.max_single_bytes = MAX_SINGLE).await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "项目").await;
    let pid = id(&p["id"]);
    let resources_id = id(&p["resources"]["id"]);

    // 与线上失败请求同样大小：超过 Axum 默认 2 MiB，但低于配置的 3 MiB。
    let content = common::pseudo_content(2_320_824, 42);
    for path in [
        format!("/api/projects/{pid}/resources/files/images/char-youyou.png"),
        format!("/api/canvases/{resources_id}/files/images/char-youyou.png"),
    ] {
        let response = app.put_bytes_auth(&path, content.clone(), &user).await;
        assert_eq!(response.status(), 200, "{path}");
        let result: Value = response.json().await.unwrap();
        assert_eq!(result["size"], content.len());
    }

    let too_large = vec![0; MAX_SINGLE + 1];
    for path in [
        format!("/api/projects/{pid}/resources/files/too-large.bin"),
        format!("/api/canvases/{resources_id}/files/too-large.bin"),
    ] {
        assert_eq!(
            app.put_bytes_auth(&path, too_large.clone(), &user)
                .await
                .status(),
            413,
            "{path}"
        );
    }
}

#[tokio::test]
async fn 管理员可更新其他用户项目并记录项目归属() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let p = project(&app, &user, "原名").await;
    let pid = id(&p["id"]);
    let password =
        one_click_cut_server::domain::identity::ensure_admin(&app.store, "admin", 1_000_000)
            .await
            .unwrap()
            .unwrap();
    let auth: Value = app.login("admin", &password).await.json().await.unwrap();
    let admin = id(&auth["access_token"]);
    let response = app
        .patch_json_auth(
            &format!("/api/projects/{pid}"),
            json!({"name":"新名"}),
            admin,
        )
        .await;
    assert_eq!(response.status(), 200);
    let updated: Value = response.json().await.unwrap();
    assert_eq!(updated["name"], "新名");
    let user_view: Value = app
        .get_auth(&format!("/api/projects/{pid}"), &user)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(user_view["name"], "新名");
}

async fn project_page(app: &common::TestApp, user: &str, query: &str) -> Value {
    let r = app.get_auth(&format!("/api/projects{query}"), user).await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

fn page_ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| id(&p["id"]).to_owned())
        .collect()
}

#[tokio::test]
async fn 项目分页默认值双向遍历与用户隔离() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let other = app.signup("bob", "password123").await;
    let empty = project_page(&app, &user, "").await;
    assert_eq!(
        empty,
        json!({"items":[], "next_cursor":null, "has_more":false})
    );
    let mut ids = Vec::new();
    for n in 0..23 {
        let p = project(&app, &user, &format!("项目{n}")).await;
        ids.push(id(&p["id"]).to_owned());
    }
    ids.sort();
    let foreign = project(&app, &other, "其他用户项目").await;
    let default_page = project_page(&app, &user, "").await;
    assert_eq!(
        page_ids(&default_page),
        ids.iter().rev().take(20).cloned().collect::<Vec<_>>()
    );
    assert_eq!(default_page["has_more"], true);
    assert_eq!(default_page["next_cursor"], default_page["items"][19]["id"]);
    for item in default_page["items"].as_array().unwrap() {
        assert_eq!(item["resources"]["role"], "resources");
        assert_eq!(item["production_canvas_count"], 1);
        assert_eq!(item["canvases"].as_array().unwrap().len(), 1);
    }
    for order in ["asc", "desc"] {
        for limit in [1, 5, 23, 100] {
            let mut got = Vec::new();
            let mut cursor = String::new();
            for _ in 0..=ids.len() {
                let page = project_page(
                    &app,
                    &user,
                    &format!("?limit={limit}&order={order}{cursor}"),
                )
                .await;
                let part = page_ids(&page);
                assert!(part.len() <= limit);
                got.extend(part);
                if !page["has_more"].as_bool().unwrap() {
                    assert!(page["next_cursor"].is_null());
                    break;
                }
                assert_eq!(page["items"].as_array().unwrap().len(), limit);
                assert_eq!(page["next_cursor"], page["items"][limit - 1]["id"]);
                cursor = format!("&cursor={}", id(&page["next_cursor"]));
            }
            let mut expected = ids.clone();
            if order == "desc" {
                expected.reverse();
            }
            assert_eq!(got, expected, "order={order}, limit={limit}");
        }
        // 其他用户的 ID 只能作为当前用户键空间中的位置，不能越权读取。
        let page = project_page(
            &app,
            &user,
            &format!("?order={order}&cursor={}", id(&foreign["id"])),
        )
        .await;
        assert!(page_ids(&page).iter().all(|pid| ids.contains(pid)));
        let last = if order == "asc" {
            ids.last().unwrap()
        } else {
            &ids[0]
        };
        let page = project_page(&app, &user, &format!("?order={order}&cursor={last}")).await;
        assert_eq!(page, empty);
    }
}

#[tokio::test]
async fn 项目游标删除后仍可双向续页() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    let mut ids = Vec::new();
    for n in 0..5 {
        ids.push(id(&project(&app, &user, &format!("项目{n}")).await["id"]).to_owned());
    }
    ids.sort();
    for order in ["asc", "desc"] {
        let first = project_page(&app, &user, &format!("?limit=1&order={order}")).await;
        let cursor = id(&first["next_cursor"]);
        assert_eq!(
            app.delete_auth(&format!("/api/projects/{cursor}"), &user)
                .await
                .status(),
            204
        );
        ids.retain(|pid| pid != cursor);
        let second = project_page(
            &app,
            &user,
            &format!("?limit=100&order={order}&cursor={cursor}"),
        )
        .await;
        let mut expected = ids.clone();
        if order == "desc" {
            expected.reverse();
        }
        assert_eq!(page_ids(&second), expected);
        assert_eq!(second["has_more"], false);
    }
}

#[tokio::test]
async fn 项目分页参数校验和认证() {
    let app = common::spawn_app().await;
    let user = app.signup("alice", "password123").await;
    for query in [
        "limit=0",
        "limit=101",
        "limit=-1",
        "limit=abc",
        "limit=",
        "limit=18446744073709551616",
        "order=invalid",
        "order=",
        "cursor=",
        "cursor=bad",
        "cursor=ZZZZZZZZZZZZZZZZZZZZZZZZZZ",
        "limit=1&limit=2",
        "page=2",
        "sort=updated_at",
    ] {
        let r = app.get_auth(&format!("/api/projects?{query}"), &user).await;
        assert_eq!(r.status(), 400, "{query}");
        assert_eq!(r.json::<Value>().await.unwrap()["code"], "bad_request");
    }
    assert_eq!(app.get("/api/projects").await.status(), 401);
    let p = project(&app, &user, "项目").await;
    let agent = token(&app, &user, id(&p["id"]), json!({})).await;
    assert_eq!(app.get_auth("/api/projects", &agent).await.status(), 403);
}

#[tokio::test]
async fn 项目分页开放接口文档与响应一致() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let spec: Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .unwrap();
    let operation = &spec["paths"]["/api/projects"]["get"];
    let params = operation["parameters"].as_array().unwrap();
    for name in ["limit", "cursor", "order"] {
        assert!(
            params
                .iter()
                .any(|p| p["name"] == name && p["in"] == "query")
        );
    }
    assert_eq!(
        operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/ListProjectsResponse"
    );
    let properties = &spec["components"]["schemas"]["ListProjectsResponse"]["properties"];
    for name in ["items", "next_cursor", "has_more"] {
        assert!(properties.get(name).is_some());
    }
}
