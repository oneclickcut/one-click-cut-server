#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::domain::{identity, plans};
use serde_json::{Value, json};

async fn admin_token(app: &common::TestApp) -> String {
    let password = identity::ensure_admin(&app.store, "admin", 1_000_000)
        .await
        .unwrap()
        .unwrap();
    let response: Value = app.login("admin", &password).await.json().await.unwrap();
    response["access_token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn 注册上限只统计成功创建且并发不超额() {
    let app = common::spawn_app_with(|c| c.registration.daily_limit = 2).await;
    assert_eq!(app.register("alice", "password123").await.status(), 201);
    assert_eq!(app.register("alice", "password123").await.status(), 409);
    let (a, b) = tokio::join!(
        app.register("bob", "password123"),
        app.register("carol", "password123")
    );
    let statuses = [a.status().as_u16(), b.status().as_u16()];
    assert!(statuses.contains(&201));
    assert!(statuses.contains(&429));
    assert_eq!(
        plans::registered_today(&app.store, one_click_cut_server::util::now_secs())
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn 基础版次数用尽升级后清零且同值更新不清零() {
    let app = common::spawn_app().await;
    let admin = admin_token(&app).await;
    let alice = app.signup("alice", "password123").await;
    let (uid, _) = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap();
    let me: Value = app.get_auth("/api/me", &alice).await.json().await.unwrap();
    assert_eq!(me["plan"], "basic");
    assert_eq!(me["quota_bytes"], 5_000_000_000_u64);
    for _ in 0..5 {
        assert_eq!(
            app.post_json_auth("/api/tasks/push", json!({}), &alice)
                .await
                .status(),
            201
        );
    }
    let rejected = app
        .post_json_auth("/api/tasks/push", json!({}), &alice)
        .await;
    assert_eq!(rejected.status(), 429);
    let body: Value = rejected.json().await.unwrap();
    assert_eq!(body["code"], "monthly_push_limit_exceeded");
    let patched = app
        .patch_json_auth(
            &format!("/api/admin/users/{uid}"),
            json!({"plan":"pro"}),
            &admin,
        )
        .await;
    assert_eq!(patched.status(), 200);
    let updated: Value = patched.json().await.unwrap();
    assert_eq!(updated["quota_bytes"], 50_000_000_000_u64);
    assert_eq!(updated["push_used"], 0);
    assert_eq!(
        app.post_json_auth("/api/tasks/push", json!({}), &alice)
            .await
            .status(),
        201
    );
    let same: Value = app
        .patch_json_auth(
            &format!("/api/admin/users/{uid}"),
            json!({"plan":"pro"}),
            &admin,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(same["push_used"], 1);

    let opened = one_click_cut_server::util::now_secs() - 3600;
    let reset: Value = app
        .patch_json_auth(
            &format!("/api/admin/users/{uid}"),
            json!({"plan_started_at": opened}),
            &admin,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(reset["push_used"], 0);
    assert_eq!(reset["plan_started_at"], opened);
}

#[test]
fn 月末周期以原始开通日为锚点() {
    use chrono::{TimeZone, Utc};
    let anchor = Utc
        .with_ymd_and_hms(2024, 1, 31, 10, 0, 0)
        .unwrap()
        .timestamp() as u64;
    let feb = Utc
        .with_ymd_and_hms(2024, 2, 29, 10, 0, 0)
        .unwrap()
        .timestamp() as u64;
    let mar = Utc
        .with_ymd_and_hms(2024, 3, 31, 10, 0, 0)
        .unwrap()
        .timestamp() as u64;
    assert_eq!(plans::period(anchor, feb).unwrap(), (feb, mar));
    let mut state = plans::PlanState::basic(anchor);
    state.push_used = 5;
    let (reset, next) = state.current(feb).unwrap();
    assert_eq!(reset.push_used, 0);
    assert_eq!(reset.push_period_start, feb);
    assert_eq!(next, mar);
}

#[test]
fn 注册日按北京时间零点切换() {
    use chrono::{TimeZone, Utc};
    let before = Utc
        .with_ymd_and_hms(2026, 10, 3, 15, 59, 59)
        .unwrap()
        .timestamp() as u64;
    assert_eq!(plans::registration_day(before).unwrap(), "2026-10-03");
    assert_eq!(plans::registration_day(before + 1).unwrap(), "2026-10-04");
}

#[tokio::test]
async fn 配置的套餐容量满额后删除文件可以继续写() {
    let app = common::spawn_app_with(|c| c.plans.basic.storage_bytes = 10).await;
    let token = app.signup("alice", "password123").await;
    let project: Value = app
        .post_json_auth("/api/projects", json!({"name":"p"}), &token)
        .await
        .json()
        .await
        .unwrap();
    let canvas = project["canvases"][0]["id"].as_str().unwrap();
    let a = format!("/api/canvases/{canvas}/files/a.jpg");
    let b = format!("/api/canvases/{canvas}/files/b.jpg");
    assert_eq!(
        app.put_bytes_auth(&a, vec![1; 10], &token).await.status(),
        200
    );
    assert_eq!(app.put_bytes_auth(&b, vec![2], &token).await.status(), 507);
    assert_eq!(app.delete_auth(&a, &token).await.status(), 204);
    assert_eq!(app.put_bytes_auth(&b, vec![2], &token).await.status(), 200);
}

#[tokio::test]
async fn 并发提交不会突破月度上限() {
    let app = common::spawn_app().await;
    let token = app.signup("alice", "password123").await;
    let path = "/api/tasks/push";
    let (a, b, c, d, e, f) = tokio::join!(
        app.post_json_auth(path, json!({}), &token),
        app.post_json_auth(path, json!({}), &token),
        app.post_json_auth(path, json!({}), &token),
        app.post_json_auth(path, json!({}), &token),
        app.post_json_auth(path, json!({}), &token),
        app.post_json_auth(path, json!({}), &token),
    );
    let statuses = [a, b, c, d, e, f].map(|r| r.status().as_u16());
    assert_eq!(statuses.iter().filter(|&&s| s == 201).count(), 5);
    assert_eq!(statuses.iter().filter(|&&s| s == 429).count(), 1);
    assert_eq!(
        one_click_cut_server::domain::task_queue::list(&app.store)
            .await
            .unwrap()
            .len(),
        5
    );
}

#[tokio::test]
async fn 重启时按配置同步已有套餐用户的容量() {
    use one_click_cut_server::storage::records;
    let temp = common::temp_store();
    let (uid, user) = identity::register(
        &temp.store,
        "alice",
        "password123",
        records::Role::User,
        1_000_000,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(user.quota_bytes, 0);
    let mut config = one_click_cut_server::config::PlansConfig::default();
    config.basic.storage_bytes = 7_000_000_000;
    plans::sync_plan_limits(&temp.store, &config).await.unwrap();
    let updated = identity::find_by_uid(&temp.store, uid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.quota_bytes, 7_000_000_000);
    assert_eq!(
        plans::get(&temp.store, uid, updated.created_at)
            .await
            .unwrap()
            .plan,
        plans::Plan::Basic
    );
}

#[tokio::test]
async fn 每日注册计数重开数据库后仍存在() {
    let temp = common::temp_store();
    let now = one_click_cut_server::util::now_secs();
    let outcome =
        identity::register_public(&temp.store, "alice", "password123", now, 5_000_000_000, 100)
            .await
            .unwrap();
    assert!(matches!(
        outcome,
        identity::RegistrationOutcome::Created(..)
    ));
    let reopened = temp.reopen();
    assert_eq!(
        plans::registered_today(&reopened.store, now).await.unwrap(),
        1
    );
}
