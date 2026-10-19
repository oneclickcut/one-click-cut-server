//! 使用真正的 CLI 子进程，覆盖列族升级后重置密码及 HTTP 登录。
#[path = "../common/mod.rs"]
mod common;

use std::path::Path;
use std::process::{Command, Output};

use one_click_cut_server::domain::{identity, task_queue};
use one_click_cut_server::storage::{StoreOptions, records::Role};
use one_click_cut_server::{AppState, Config, Store};

fn reset(data_dir: &Path, username: &str, password: &str) -> Output {
    let config_path = data_dir.join("reset.toml");
    std::fs::write(&config_path, "").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_reset_admin_password"));
    // 避免开发机 OCC_* 配置干扰，单独验证 OCC_CONFIG 与 OCC_DATA_DIR。
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("OCC_") {
            cmd.env_remove(key);
        }
    }
    cmd.args([username, password])
        .current_dir(data_dir)
        .env("OCC_CONFIG", config_path)
        .env("OCC_DATA_DIR", data_dir)
        .output()
        .expect("运行密码重置工具")
}

#[tokio::test]
async fn 含任务队列的数据库重置后可登录且保留数据() {
    let db = common::temp_store();
    let (uid, before) =
        identity::register(&db.store, "admin", "old-password", Role::Admin, 1_000_000)
            .await
            .unwrap()
            .unwrap();
    task_queue::push(&db.store, uid, serde_json::json!({"keep": true}))
        .await
        .unwrap();
    db.store.flush().await.unwrap();
    let dir = db.dir;
    drop(db.store);

    let result = reset(dir.path(), "admin", "new-password");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let store = Store::open_with(dir.path().join("rocksdb"), &StoreOptions::small()).unwrap();
    let (after_uid, after) = identity::find_by_username(&store, "admin")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(uid, after_uid);
    assert_eq!(before.created_at, after.created_at);
    assert_eq!(before.role, after.role);
    assert_eq!(before.disabled, after.disabled);
    assert_eq!(before.quota_bytes, after.quota_bytes);
    assert_eq!(before.bytes_used, after.bytes_used);
    assert_ne!(before.pwd_hash, after.pwd_hash);
    assert_eq!(task_queue::list(&store).await.unwrap().len(), 1);

    let config = Config {
        data_dir: dir.path().to_owned(),
        jwt: one_click_cut_server::config::JwtConfig {
            secret: "test-secret-not-for-production-32+".to_owned(),
        },
        ..Config::default()
    };
    let app = one_click_cut_server::http::router(AppState::new(config, store).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    for (password, status) in [("old-password", 401), ("new-password", 200)] {
        let response = client
            .post(format!("http://{addr}/api/auth/login"))
            .json(&serde_json::json!({"username": "admin", "password": password}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        if status == 200 {
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(body["role"], "admin");
            let me = client
                .get(format!("http://{addr}/api/me"))
                .bearer_auth(body["access_token"].as_str().unwrap())
                .send()
                .await
                .unwrap();
            assert_eq!(me.status(), 200);
        }
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn 拒绝普通用户且不改密码() {
    let db = common::temp_store();
    identity::register(&db.store, "alice", "old-password", Role::User, 1)
        .await
        .unwrap()
        .unwrap();
    let dir = db.dir;
    drop(db.store);
    let result = reset(dir.path(), "alice", "new-password");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("不是管理员"));
    let store = Store::open_with(dir.path().join("rocksdb"), &StoreOptions::small()).unwrap();
    let (_, user) = identity::find_by_username(&store, "alice")
        .await
        .unwrap()
        .unwrap();
    assert!(identity::verify_password("old-password", &user.pwd_hash).unwrap());
}

#[test]
fn 错误的数据目录不会创建空数据库() {
    let dir = tempfile::tempdir().unwrap();
    let result = reset(dir.path(), "admin", "new-password");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("数据库不存在"));
    assert!(!dir.path().join("rocksdb").exists());
}
