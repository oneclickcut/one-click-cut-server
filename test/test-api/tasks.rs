//! 公共任务队列的鉴权、输入边界、并发领取与 OpenAPI 契约。
#[path = "../common/mod.rs"]
mod common;

use std::collections::HashSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use one_click_cut_server::domain::identity;
use one_click_cut_server::domain::task_queue;
use one_click_cut_server::storage::{Cf, keys, records};
use serde_json::{Value, json};

async fn app() -> common::TestApp {
    common::spawn_app_with(|c| {
        c.task_queue.reader_usernames = vec!["worker1".into(), "worker2".into()];
        c.plans.basic.monthly_push_limit = 1000;
    })
    .await
}

async fn pop(app: &common::TestApp, token: &str, body: Value) -> Value {
    let r = app.post_json_auth("/api/tasks/pop", body, token).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["cache-control"], "no-store");
    r.json().await.unwrap()
}

#[tokio::test]
async fn 查看完整任务列表不消费且领取后列表更新() {
    let app = app().await;
    let writer = app.signup("alice", "password123").await;
    let worker1 = app.signup("worker1", "password123").await;
    let worker2 = app.signup("worker2", "password123").await;
    let (uid, _) = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap();

    let empty = app.get_auth("/api/tasks", &worker1).await;
    assert_eq!(empty.status(), 200);
    assert_eq!(empty.json::<Value>().await.unwrap(), json!({"tasks":[]}));

    // 超过领取接口单次 100 条的上限，确认列表没有隐式分页或截断。
    let mut expected = HashSet::new();
    for i in 0..105 {
        let id = task_queue::push(&app.store, uid, json!({"index":i}))
            .await
            .unwrap();
        expected.insert(id.to_string());
    }
    let first = app.get_auth("/api/tasks", &worker1).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.headers()["cache-control"], "no-store");
    let first: Value = first.json().await.unwrap();
    let tasks = first["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 105);
    assert_eq!(
        tasks
            .iter()
            .map(|t| t["task_id"].as_str().unwrap().to_owned())
            .collect::<HashSet<_>>(),
        expected
    );
    assert!(
        tasks
            .windows(2)
            .all(|pair| pair[0]["task_id"].as_str() < pair[1]["task_id"].as_str())
    );
    assert!(tasks.iter().all(|task| task["user_id"] == uid.to_string()));

    let again: Value = app
        .get_auth("/api/tasks", &worker2)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again, first, "查看不会消费任务，名单内用户看到同一队列");

    let claimed = pop(&app, &worker1, json!({"limit":100})).await;
    assert_eq!(claimed["tasks"].as_array().unwrap().len(), 100);
    let remaining: Value = app
        .get_auth("/api/tasks", &worker1)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(remaining["tasks"].as_array().unwrap().len(), 5);
    assert_eq!(
        pop(&app, &worker2, json!({"limit":100})).await["tasks"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let empty: Value = app
        .get_auth("/api/tasks", &worker1)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(empty, json!({"tasks":[]}));

    // 提交者可写但不能查看完整队列。
    assert_eq!(
        app.post_json_auth("/api/tasks/push", json!({}), &writer)
            .await
            .status(),
        201
    );
    assert_eq!(app.get_auth("/api/tasks", &writer).await.status(), 403);
}

#[tokio::test]
async fn 多用户提交保留原始json且领取后删除() {
    let app = app().await;
    let alice = app.signup("alice", "password123").await;
    let bob = app.signup("bob", "password123").await;
    let worker = app.signup("worker1", "password123").await;
    let (alice_id, _) = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap();
    let (bob_id, _) = identity::find_by_username(&app.store, "bob")
        .await
        .unwrap()
        .unwrap();
    let inputs = [
        json!({"user_id":"伪造身份", "task_id":"伪造ID", "nested":[1,true,null,"中文"]}),
        json!(["array", {"x":2}]),
        json!(null),
        json!(false),
        json!(12.5),
        json!("文本"),
    ];
    let mut expected = std::collections::HashMap::new();
    for (i, payload) in inputs.into_iter().enumerate() {
        let (token, uid) = if i % 2 == 0 {
            (&alice, alice_id)
        } else {
            (&bob, bob_id)
        };
        let r = app
            .post_json_auth("/api/tasks/push", payload.clone(), token)
            .await;
        assert_eq!(r.status(), 201);
        let body: Value = r.json().await.unwrap();
        let id = body["task_id"].as_str().unwrap().to_owned();
        keys::TaskId::parse(&id).unwrap();
        assert!(expected.insert(id, (uid.to_string(), payload)).is_none());
    }
    let first = pop(&app, &worker, json!({})).await;
    assert_eq!(first["tasks"].as_array().unwrap().len(), 1);
    let rest = pop(&app, &worker, json!({"limit":100})).await;
    assert_eq!(rest["tasks"].as_array().unwrap().len(), 5);
    for task in first["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .chain(rest["tasks"].as_array().unwrap())
    {
        let (uid, payload) = expected.remove(task["task_id"].as_str().unwrap()).unwrap();
        assert_eq!(task["user_id"], uid);
        assert_eq!(task["payload"], payload);
        assert!(task["created_at"].as_u64().unwrap() > 0);
        assert!(task.get("v").is_none());
    }
    assert!(expected.is_empty());
    assert_eq!(pop(&app, &worker, json!({})).await, json!({"tasks":[]}));
}

#[tokio::test]
async fn 登录与白名单权限且拒绝agent和禁用账号() {
    let app = app().await;
    let alice = app.signup("alice", "password123").await;
    let worker = app.signup("worker1", "password123").await;
    for path in ["/api/tasks/push", "/api/tasks/pop"] {
        assert_eq!(app.post_json(path, json!({})).await.status(), 401);
        assert_eq!(
            app.post_json_auth(path, json!({}), "bad-token")
                .await
                .status(),
            401
        );
    }
    assert_eq!(app.get("/api/tasks").await.status(), 401);
    assert_eq!(app.get_auth("/api/tasks", "bad-token").await.status(), 401);
    assert_eq!(app.get_auth("/api/tasks", &alice).await.status(), 403);
    assert_eq!(
        app.post_json_auth("/api/tasks/pop", json!({}), &alice)
            .await
            .status(),
        403
    );

    let project: Value = app
        .post_json_auth("/api/projects", json!({"name":"queue-test"}), &worker)
        .await
        .json()
        .await
        .unwrap();
    let issued = app
        .post_json_auth(
            &format!(
                "/api/projects/{}/agent-tokens",
                project["id"].as_str().unwrap()
            ),
            json!({}),
            &worker,
        )
        .await;
    assert_eq!(issued.status(), 201);
    let issued: Value = issued.json().await.unwrap();
    for path in ["/api/tasks/push", "/api/tasks/pop"] {
        assert_eq!(
            app.post_json_auth(path, json!({}), issued["token"].as_str().unwrap())
                .await
                .status(),
            403
        );
    }
    assert_eq!(
        app.get_auth("/api/tasks", issued["token"].as_str().unwrap())
            .await
            .status(),
        403
    );
    let (uid, mut user) = identity::find_by_username(&app.store, "worker1")
        .await
        .unwrap()
        .unwrap();
    user.disabled = true;
    app.store
        .put(
            Cf::Meta,
            keys::user_key(uid),
            records::encode(&user).unwrap(),
        )
        .await
        .unwrap();
    for path in ["/api/tasks/push", "/api/tasks/pop"] {
        assert_eq!(
            app.post_json_auth(path, json!({}), &worker).await.status(),
            401
        );
    }
    assert_eq!(app.get_auth("/api/tasks", &worker).await.status(), 401);
}

#[tokio::test]
async fn 空白名单不允许管理员绕过() {
    let app = common::spawn_app().await;
    let admin = app.signup("admin", "password123").await;
    let (uid, mut user) = identity::find_by_username(&app.store, "admin")
        .await
        .unwrap()
        .unwrap();
    user.role = records::Role::Admin;
    app.store
        .put(
            Cf::Meta,
            keys::user_key(uid),
            records::encode(&user).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        app.post_json_auth("/api/tasks/push", json!({}), &admin)
            .await
            .status(),
        201
    );
    assert_eq!(
        app.post_json_auth("/api/tasks/pop", json!({}), &admin)
            .await
            .status(),
        403
    );
    assert_eq!(app.get_auth("/api/tasks", &admin).await.status(), 403);
}

#[tokio::test]
async fn 请求大小与领取数量受限且失败不消费任务() {
    let app = common::spawn_app_with(|c| {
        c.task_queue.reader_usernames = vec!["worker1".into()];
        c.task_queue.max_payload_bytes = 64;
    })
    .await;
    let worker = app.signup("worker1", "password123").await;
    // JSON 字符串引号也计入大小：62 + 2 恰好 64 字节。
    assert_eq!(
        app.post_json_auth("/api/tasks/push", json!("x".repeat(62)), &worker)
            .await
            .status(),
        201
    );
    assert_eq!(
        app.post_json_auth("/api/tasks/push", json!("x".repeat(63)), &worker)
            .await
            .status(),
        413
    );
    assert_eq!(
        app.post_bytes_auth(
            "/api/tasks/push",
            b"{".to_vec(),
            "application/json",
            &worker
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        app.post_bytes_auth("/api/tasks/push", b"{}".to_vec(), "text/plain", &worker)
            .await
            .status(),
        415
    );
    for limit in [0, 101] {
        assert_eq!(
            app.post_json_auth("/api/tasks/pop", json!({"limit":limit}), &worker)
                .await
                .status(),
            400
        );
    }
    for body in [
        json!({"limit":-1}),
        json!({"limit":"1"}),
        json!({"limit":null}),
        json!({"typo":1}),
    ] {
        assert_eq!(
            app.post_json_auth("/api/tasks/pop", body, &worker)
                .await
                .status(),
            422
        );
    }
    assert_eq!(
        pop(&app, &worker, json!({"limit":100})).await["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(pop(&app, &worker, json!({})).await, json!({"tasks":[]}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn 多个提交者和领取者同时调用不丢失不重复() {
    let app = app().await;
    let writer = app.signup("alice", "password123").await;
    let worker1 = app.signup("worker1", "password123").await;
    let worker2 = app.signup("worker2", "password123").await;
    let finished = Arc::new(AtomicBool::new(false));
    let mut readers = tokio::task::JoinSet::new();
    for i in 0..8 {
        let client = app.client.clone();
        let url = app.url("/api/tasks/pop");
        let token = if i % 2 == 0 {
            worker1.clone()
        } else {
            worker2.clone()
        };
        let finished = Arc::clone(&finished);
        readers.spawn(async move {
            let mut tasks = Vec::new();
            loop {
                // 在请求之前观察提交完成；完成后的空响应才能证明已经排空。
                let done = finished.load(Ordering::Acquire);
                let r = client
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&json!({"limit": if i % 2 == 0 { 1 } else { 7 }}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 200);
                let body: Value = r.json().await.unwrap();
                let batch = body["tasks"].as_array().unwrap();
                if done && batch.is_empty() {
                    break;
                }
                tasks.extend(batch.iter().cloned());
                tokio::task::yield_now().await;
            }
            tasks
        });
    }
    let mut writers = tokio::task::JoinSet::new();
    for producer in 0..8 {
        let client = app.client.clone();
        let url = app.url("/api/tasks/push");
        let token = writer.clone();
        writers.spawn(async move {
            let mut ids = Vec::new();
            for sequence in 0..16 {
                let r = client
                    .post(&url)
                    .bearer_auth(&token)
                    .json(&json!({"producer":producer,"sequence":sequence}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 201);
                let body: Value = r.json().await.unwrap();
                ids.push(body["task_id"].as_str().unwrap().to_owned());
            }
            ids
        });
    }
    let mut submitted = HashSet::new();
    while let Some(result) = writers.join_next().await {
        for id in result.unwrap() {
            assert!(submitted.insert(id));
        }
    }
    finished.store(true, Ordering::Release);
    let mut received = HashSet::new();
    let mut payloads = HashSet::new();
    while let Some(result) = readers.join_next().await {
        for task in result.unwrap() {
            assert!(
                received.insert(task["task_id"].as_str().unwrap().to_owned()),
                "重复领取"
            );
            assert!(payloads.insert((
                task["payload"]["producer"].as_u64().unwrap(),
                task["payload"]["sequence"].as_u64().unwrap()
            )));
        }
    }
    assert_eq!(submitted.len(), 128);
    assert_eq!(received, submitted);
    assert_eq!(payloads.len(), 128);
    assert_eq!(pop(&app, &worker1, json!({})).await, json!({"tasks":[]}));
}

#[tokio::test]
async fn 队列接口登记到openapi且声明jwt() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let doc: Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .unwrap();
    for path in ["/api/tasks/push", "/api/tasks/pop"] {
        let operation = &doc["paths"][path]["post"];
        assert!(operation.is_object());
        assert_eq!(operation["security"][0]["bearer_jwt"], json!([]));
        assert!(operation["requestBody"].is_object());
    }
    let list = &doc["paths"]["/api/tasks"]["get"];
    assert!(list.is_object());
    assert_eq!(list["security"][0]["bearer_jwt"], json!([]));
}

#[test]
fn 队列配置默认拒绝读取并支持toml覆盖与校验() {
    let cfg = one_click_cut_server::Config::default();
    assert!(cfg.task_queue.reader_usernames.is_empty());
    assert_eq!(cfg.task_queue.max_payload_bytes, 65536);
    let mut cfg: one_click_cut_server::Config = toml::from_str(
        "[task_queue]\nreader_usernames = ['worker1', 'worker2']\nmax_payload_bytes = 128\n",
    )
    .unwrap();
    assert_eq!(cfg.task_queue.reader_usernames, ["worker1", "worker2"]);
    assert_eq!(cfg.task_queue.max_payload_bytes, 128);
    cfg.task_queue.validate().unwrap();
    cfg.task_queue.max_payload_bytes = 0;
    assert!(cfg.task_queue.validate().is_err());
    cfg.task_queue.max_payload_bytes = 128;
    cfg.task_queue.reader_usernames.push("bad/user".into());
    assert!(cfg.task_queue.validate().is_err());
}
