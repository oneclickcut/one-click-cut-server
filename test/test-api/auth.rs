//! S3 认证接口测试。
//!
//! 有两条是**安全属性**而不是功能，写在最前面：
//!
//! 1. 登录失败的响应不能区分「用户不存在」与「密码错误」，否则这个接口就是
//!    一个用户名枚举工具；
//! 2. 无期限 JWT 只有当前读取名单内的账号可用，禁用立即生效。

#[path = "../common/mod.rs"]
mod common;

use one_click_cut_server::domain::identity;
use one_click_cut_server::storage::records::{Role, User};
use one_click_cut_server::storage::{Cf, keys, records};

const TEST_SECRET: &str = "test-secret-not-for-production-32+";

fn sign_claims(claims: serde_json::Value) -> String {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(TEST_SECRET.as_bytes()),
    )
    .expect("签名")
}

#[tokio::test]
async fn jwt第8天和第89天可用第91天失效() {
    let app = common::spawn_app().await;
    app.signup("alice", "password123").await;
    let (uid, _) = identity::find_by_username(&app.store, "alice")
        .await
        .unwrap()
        .unwrap();
    let issuer = identity::Identity::new(TEST_SECRET).unwrap();
    let now = one_click_cut_server::util::now_secs();
    for (days, expected) in [(8, 200), (89, 200), (91, 401)] {
        let token = issuer
            .issue_access(
                uid,
                Role::User,
                now - days * 86400,
                Some(identity::ACCESS_TTL_SECS),
            )
            .unwrap();
        assert_eq!(app.get_auth("/api/me", &token).await.status(), expected);
    }
}

#[tokio::test]
async fn 名单账号登录签发永久jwt且不创建刷新记录() {
    let app = common::spawn_app_with(|c| {
        c.task_queue.reader_usernames = vec!["worker1".into(), "worker2".into()];
    })
    .await;
    let issuer = identity::Identity::new(TEST_SECRET).unwrap();
    for name in ["worker1", "worker2"] {
        app.register(name, "password123").await;
        let body: serde_json::Value = app.login(name, "password123").await.json().await.unwrap();
        assert!(body.get("expires_in").unwrap().is_null());
        assert!(body.get("refresh_token").is_none());
        let token = body["access_token"].as_str().unwrap();
        let claims = issuer.verify_access(token).unwrap();
        assert!(serde_json::to_value(&claims).unwrap().get("exp").is_none());
        assert_eq!(app.get_auth("/api/me", token).await.status(), 200);
        assert_eq!(app.get_auth("/api/tasks", token).await.status(), 200);
    }
    app.signup("alice", "password123").await;
    assert!(
        app.store
            .scan_prefix_limit(Cf::Auth, b"rt\0".to_vec(), 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn 无期限必须在名单内且不能绕过旧凭证的exp() {
    let app =
        common::spawn_app_with(|c| c.task_queue.reader_usernames = vec!["worker1".into()]).await;
    let now = one_click_cut_server::util::now_secs();
    for name in ["alice", "worker1", "worker999"] {
        app.signup(name, "password123").await;
        let (uid, _) = identity::find_by_username(&app.store, name)
            .await
            .unwrap()
            .unwrap();
        let claims = serde_json::json!({"sub": uid.to_text(), "role": "user", "iat": now - 366 * 86400, "jti": "old-permanent"});
        let token = sign_claims(claims.clone());
        assert_eq!(
            app.get_auth("/api/me", &token).await.status(),
            if name == "worker1" { 200 } else { 401 }
        );
        for invalid_exp in [
            serde_json::json!(now - 3600),
            serde_json::json!("forever"),
            serde_json::json!(-1),
        ] {
            let mut invalid = claims.clone();
            invalid["exp"] = invalid_exp;
            assert_eq!(
                app.get_auth("/api/me", &sign_claims(invalid))
                    .await
                    .status(),
                401
            );
        }
    }
}

#[tokio::test]
async fn 永久jwt禁用或删除账号立即失效() {
    let app =
        common::spawn_app_with(|c| c.task_queue.reader_usernames = vec!["worker1".into()]).await;
    let token = app.signup("worker1", "password123").await;
    let (uid, mut user) = identity::find_by_username(&app.store, "worker1")
        .await
        .unwrap()
        .unwrap();
    user.disabled = true;
    identity::save(&app.store, uid, &user).await.unwrap();
    assert_eq!(app.get_auth("/api/me", &token).await.status(), 401);
    assert_eq!(app.get_auth("/api/tasks", &token).await.status(), 401);
    app.store
        .delete(Cf::Meta, keys::user_key(uid))
        .await
        .unwrap();
    assert_eq!(app.get_auth("/api/me", &token).await.status(), 401);
}

#[tokio::test]
async fn 重开数据库保留永久jwt移出名单或换密钥后拒绝() {
    use axum::extract::FromRequestParts;
    use one_click_cut_server::http::auth::UserCaller;
    use one_click_cut_server::{AppState, Config};

    let ts = common::temp_store();
    let now = one_click_cut_server::util::now_secs();
    let issuer = identity::Identity::new(TEST_SECRET).unwrap();
    let (uid, _) = identity::register(&ts.store, "worker1", "password123", Role::User, now)
        .await
        .unwrap()
        .unwrap();
    let token = issuer
        .issue_access(uid, Role::User, now - 366 * 86400, None)
        .unwrap();
    let ts = ts.reopen();
    for (in_list, secret, expected) in [
        (true, TEST_SECRET, true),
        (false, TEST_SECRET, false),
        (true, "a-different-secret-at-least-32-bytes", false),
    ] {
        let mut cfg = Config::default();
        cfg.jwt.secret = secret.into();
        if in_list {
            cfg.task_queue.reader_usernames = vec!["worker1".into()];
        }
        let state = AppState::new(cfg, ts.store.clone()).unwrap();
        let (mut parts, _) = axum::http::Request::builder()
            .uri("/api/me")
            .header("Authorization", format!("Bearer {token}"))
            .body(())
            .unwrap()
            .into_parts();
        assert_eq!(
            UserCaller::from_request_parts(&mut parts, &state)
                .await
                .is_ok(),
            expected
        );
    }
}

#[tokio::test]
async fn 旧刷新数据不再用于登录且不干扰其他清理() {
    let app = common::spawn_app().await;
    let legacy_key = b"rt\0old-record".to_vec();
    app.store
        .put(Cf::Auth, legacy_key.clone(), b"legacy-data".to_vec())
        .await
        .unwrap();
    for path in ["/api/auth/refresh", "/api/auth/logout"] {
        assert_eq!(
            app.post_json(path, serde_json::json!({"refresh_token":"old-token"}))
                .await
                .status(),
            404
        );
    }
    one_click_cut_server::domain::maintenance::purge_credentials(
        &app.store,
        one_click_cut_server::util::now_secs(),
    )
    .await
    .unwrap();
    assert!(app.store.get(Cf::Auth, legacy_key).await.unwrap().is_some());
}

// ---------------------------------------------------------------------------
// 注册
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 注册成功返回uid() {
    let app = common::spawn_app().await;

    let r = app.register("alice", "correct horse battery").await;
    assert_eq!(r.status(), 201);

    let body: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(body["username"], "alice");
    let uid = body["uid"].as_str().expect("应有 uid");
    assert_eq!(uid.len(), 26, "uid 应是 26 字符 ULID");
}

#[tokio::test]
async fn 用户名重复返回409() {
    let app = common::spawn_app().await;
    assert_eq!(app.register("alice", "password123").await.status(), 201);

    let r = app.register("alice", "different-password").await;
    assert_eq!(r.status(), 409, "重名应返回 409 而不是覆盖");
}

#[tokio::test]
async fn 弱密码与非法用户名被拒() {
    let app = common::spawn_app().await;

    // 密码规则。
    for pw in ["", "short", "1234567", "        "] {
        let r = app.register("bob", pw).await;
        assert_eq!(r.status(), 400, "密码 {pw:?} 应被拒");
    }
    // 用户名规则：进 key 的字段，规则严格。
    for name in ["", "has space", "中文名", "a/b", "a@b", "with\u{0}null"] {
        let r = app.register(name, "password123").await;
        assert_eq!(r.status(), 400, "用户名 {name:?} 应被拒");
    }
}

#[tokio::test]
async fn 密码不以明文落盘() {
    let app = common::spawn_app().await;
    let password = "correct horse battery staple";
    app.register("alice", password).await;

    // 直接翻存储，确认落盘的是 argon2id 的 PHC 串而不是明文。
    let (_, user) = identity::find_by_username(&app.store, "alice")
        .await
        .expect("查询")
        .expect("应存在");
    assert!(
        !user.pwd_hash.contains(password),
        "落盘的哈希里不该出现明文密码"
    );
    assert!(
        user.pwd_hash.starts_with("$argon2id$"),
        "应是 argon2id 的 PHC 串: {}",
        user.pwd_hash
    );
}

#[tokio::test]
async fn 每个用户独立salt() {
    let app = common::spawn_app().await;
    let same = "identical password";
    app.register("alice", same).await;
    app.register("bob", same).await;

    let (_, a) = identity::find_by_username(&app.store, "alice")
        .await
        .expect("查")
        .expect("有");
    let (_, b) = identity::find_by_username(&app.store, "bob")
        .await
        .expect("查")
        .expect("有");
    assert_ne!(
        a.pwd_hash, b.pwd_hash,
        "同密码不同用户必须得到不同哈希，否则 salt 没起作用"
    );
}

// ---------------------------------------------------------------------------
// 登录
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 登录成功只返回有效90天的jwt() {
    let app = common::spawn_app().await;
    app.register("alice", "password123").await;

    let r = app.login("alice", "password123").await;
    assert_eq!(r.status(), 200);

    let body: serde_json::Value = r.json().await.expect("JSON");
    assert!(!body["access_token"].as_str().expect("有").is_empty());
    assert!(body.get("refresh_token").is_none());
    assert_eq!(body["expires_in"], 90 * 24 * 60 * 60, "JWT 有效期 90 天");
    assert_eq!(body["role"], "user");
}

#[tokio::test]
async fn 不存在的用户与错密码响应完全一致() {
    let app = common::spawn_app().await;
    app.register("alice", "password123").await;

    // ==== 这是一条安全属性，不是功能 ====
    // 区分两者会让登录接口变成用户名枚举工具：攻击者只要看响应就能确认
    // 哪些用户名已注册。所以状态码与响应体必须逐字节相同。
    let wrong_pw = app.login("alice", "wrong-password").await;
    let no_user = app.login("nosuchuser", "wrong-password").await;

    assert_eq!(wrong_pw.status(), 401);
    assert_eq!(no_user.status(), 401);

    let a = wrong_pw.text().await.expect("文本");
    let b = no_user.text().await.expect("文本");
    assert_eq!(a, b, "两种失败的响应体必须完全一致");
}

#[tokio::test]
async fn 被禁用的账户无法登录且原因不外泄() {
    let app = common::spawn_app().await;
    app.register("alice", "password123").await;

    // 直接改存储把账户禁用。
    let (uid, mut user) = identity::find_by_username(&app.store, "alice")
        .await
        .expect("查")
        .expect("有");
    user.disabled = true;
    identity::save(&app.store, uid, &user).await.expect("保存");

    let disabled = app.login("alice", "password123").await;
    assert_eq!(disabled.status(), 401);

    // 「被禁用」也不能与「密码错误」区分——否则又成了账户状态探测接口。
    let wrong = app.login("alice2", "password123").await;
    assert_eq!(
        disabled.text().await.expect("文本"),
        wrong.text().await.expect("文本"),
        "禁用与密码错误的响应必须一致"
    );
}

// ---------------------------------------------------------------------------
// /api/me 与鉴权
// ---------------------------------------------------------------------------

#[tokio::test]
async fn me需要凭证() {
    let app = common::spawn_app().await;
    assert_eq!(app.get("/api/me").await.status(), 401, "缺 Authorization");

    for bad in ["", "not-a-jwt", "a.b.c"] {
        assert_eq!(
            app.get_auth("/api/me", bad).await.status(),
            401,
            "非法 token {bad:?} 应被拒"
        );
    }
}

#[tokio::test]
async fn me返回当前用户() {
    let app = common::spawn_app().await;
    let access = app.signup("alice", "password123").await;

    let r = app.get_auth("/api/me", &access).await;
    assert_eq!(r.status(), 200);

    let body: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(body["username"], "alice");
    assert_eq!(body["role"], "user");
    assert_eq!(body["bytes_used"], 0);
    assert_eq!(
        body["quota_bytes"], 5_000_000_000_u64,
        "新用户为 Basic 5 GB"
    );
}

#[tokio::test]
async fn 禁用账户的已签发token立即失效() {
    let app = common::spawn_app().await;
    let access = app.signup("alice", "password123").await;
    assert_eq!(app.get_auth("/api/me", &access).await.status(), 200);

    // ==== 「权限实时查 KV」的意义 ====
    // 如果鉴权只看 JWT 里的 claims，禁用账户要等 token 过期（90 天）才生效。
    // 这里禁用之后同一个 token 必须立刻失效。
    let (uid, mut user) = identity::find_by_username(&app.store, "alice")
        .await
        .expect("查")
        .expect("有");
    user.disabled = true;
    identity::save(&app.store, uid, &user).await.expect("保存");

    assert_eq!(
        app.get_auth("/api/me", &access).await.status(),
        401,
        "禁用后同一个 access token 必须立即失效"
    );
}

#[tokio::test]
async fn 角色变更立即体现() {
    let app = common::spawn_app().await;
    let access = app.signup("alice", "password123").await;

    let (uid, mut user) = identity::find_by_username(&app.store, "alice")
        .await
        .expect("查")
        .expect("有");
    user.role = Role::Admin;
    identity::save(&app.store, uid, &user).await.expect("保存");

    // token 里写的还是 user，但实时查 KV 拿到的是 admin。
    let body: serde_json::Value = app
        .get_auth("/api/me", &access)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(body["role"], "admin", "角色变更应立即体现，不等 token 过期");
}

#[tokio::test]
async fn agent前缀的token在s3阶段被拒() {
    let app = common::spawn_app().await;

    // agent token 的校验在 S7 实现。在那之前必须拒绝，
    // 而不是「暂时放行」——放行一个未实现的鉴权分支是最危险的占位方式。
    let r = app
        .get_auth("/api/me", "at_abcdefghijklmnopqrstuvwxyz012345")
        .await;
    assert_eq!(r.status(), 401, "agent token 现在还不该被接受");
}

// ---------------------------------------------------------------------------
// 管理账户初始化
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 管理账户首次创建且不重复创建() {
    let ts = common::temp_store();
    let now = 1_000_000u64;

    let first = identity::ensure_admin(&ts.store, "admin", now)
        .await
        .expect("创建");
    let password = first.expect("首次应创建并返回初始密码");
    assert!(password.len() >= 16, "初始密码要够长: {}", password.len());

    // ==== 重启不该重置管理员密码 ====
    // 否则每次重启都会在日志里留一个可用的管理员凭证，
    // 而日志通常比密码库更容易被看到。
    let second = identity::ensure_admin(&ts.store, "admin", now + 100)
        .await
        .expect("再次调用");
    assert!(second.is_none(), "已存在时不该返回新密码");

    // 原密码仍然可用。
    let (_, user) = identity::find_by_username(&ts.store, "admin")
        .await
        .expect("查")
        .expect("有");
    assert!(user.role.is_admin());
    assert!(
        identity::verify_password(&password, &user.pwd_hash).expect("校验"),
        "重启后原初始密码仍应可用"
    );
}

#[tokio::test]
async fn 管理员登录后角色为admin() {
    let app = common::spawn_app().await;

    // spawn_app 不跑 bootstrap，所以这里手工建一个管理员。
    let now = 1_000_000u64;
    let password = identity::ensure_admin(&app.store, "admin", now)
        .await
        .expect("创建")
        .expect("应返回密码");

    let r = app.login("admin", &password).await;
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.expect("JSON");
    assert_eq!(body["role"], "admin");

    let access = body["access_token"].as_str().expect("有");
    let me: serde_json::Value = app
        .get_auth("/api/me", access)
        .await
        .json()
        .await
        .expect("JSON");
    assert_eq!(me["role"], "admin");
}

// ---------------------------------------------------------------------------
// 文档
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 认证接口都在openapi文档里() {
    let app = common::spawn_app_with(|c| c.docs.enabled = true).await;
    let doc: serde_json::Value = app
        .get("/api-docs/openapi.json")
        .await
        .json()
        .await
        .expect("JSON");

    for (path, method) in [
        ("/api/auth/register", "post"),
        ("/api/auth/login", "post"),
        ("/api/me", "get"),
    ] {
        assert!(
            doc["paths"][path][method].is_object(),
            "{method} {path} 应出现在文档里（注解与实现同源）"
        );
    }

    assert!(doc["paths"].get("/api/auth/refresh").is_none());
    assert!(doc["paths"].get("/api/auth/logout").is_none());
    let response = &doc["components"]["schemas"]["TokenResponse"];
    assert!(response["properties"].get("refresh_token").is_none());
    assert!(
        response["properties"]["expires_in"]["type"]
            .as_array()
            .expect("可空整数类型")
            .contains(&serde_json::json!("null"))
    );

    // 两种安全方案都要声明；读接口不标安全方案，正好体现「公开可读」。
    let schemes = &doc["components"]["securitySchemes"];
    assert!(schemes["bearer_jwt"].is_object(), "应声明 bearer_jwt");
    assert!(schemes["agent_token"].is_object(), "应声明 agent_token");

    // /api/me 要标上 bearer_jwt，否则 Swagger UI 上没法点 Authorize 调试。
    assert!(
        doc["paths"]["/api/me"]["get"]["security"].is_array(),
        "/api/me 应标注安全方案"
    );
}

// ---------------------------------------------------------------------------
// 用户名索引一致性
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 用户记录与用户名索引同批提交() {
    let app = common::spawn_app().await;
    app.register("alice", "password123").await;

    // 只有索引没有记录 → 登录查到一个不存在的 uid；
    // 只有记录没有索引 → 用户名可被再次注册。两者都不能出现。
    let idx = app
        .store
        .get(Cf::Idx, keys::username_key("alice").expect("键"))
        .await
        .expect("get")
        .expect("索引应存在");
    let uid = keys::Uid::parse(std::str::from_utf8(&idx).expect("UTF-8")).expect("解析");

    let raw = app
        .store
        .get(Cf::Meta, keys::user_key(uid))
        .await
        .expect("get")
        .expect("记录应存在");
    let user: User = records::decode(&raw).expect("解码");
    assert_eq!(user.username, "alice");
}
