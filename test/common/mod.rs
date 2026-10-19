//! 集成测试公共 fixture。
//!
//! 每个测试文件用路径属性引入（cargo 只认 `tests/`，本项目按 CLAUDE.md 放在
//! `test/`，所以测试目标都在 `Cargo.toml` 里显式登记）：
//!
//! ```ignore
//! #[path = "../common/mod.rs"]
//! mod common;
//! ```
//!
//! 每个测试目标只用到这里的一部分函数，所以整体 allow dead_code。
#![allow(dead_code)]

use std::path::Path;

use one_click_cut_server::storage::StoreOptions;
use one_click_cut_server::{AppState, Config, Store};
use tempfile::TempDir;

/// 一个临时目录里的独立存储。`TempDir` 随结构体一起 drop，自动清理。
pub struct TempStore {
    /// 持有所有权即持有目录：提前 drop 会把数据目录删掉。
    pub dir: TempDir,
    pub store: Store,
}

impl TempStore {
    /// 数据根目录（`store.path()` 是它下面的 `rocksdb/`）。
    pub fn data_dir(&self) -> &Path {
        self.dir.path()
    }

    /// 关掉再按同一路径重开，用来验证数据真的落了盘、且 merge operator 会重新注册。
    pub fn reopen(self) -> Self {
        let dir = self.dir;
        let path = self.store.path().to_path_buf();
        drop(self.store);
        let store = Store::open_with(&path, &StoreOptions::small()).expect("重新打开存储");
        Self { dir, store }
    }

    /// 数据目录当前占用字节数，用来观察写放大与去重效果。
    pub fn disk_usage(&self) -> u64 {
        dir_size(self.dir.path())
    }
}

/// 建一个临时存储。
///
/// 用 [`StoreOptions::small`]：生产默认的 8GB block cache 与 128MB write buffer
/// 在几十个并行测试下会把内存吃光。
pub fn temp_store() -> TempStore {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let store =
        Store::open_with(dir.path().join("rocksdb"), &StoreOptions::small()).expect("打开存储");
    TempStore { dir, store }
}

/// 起在随机端口上的一个服务实例。
pub struct TestApp {
    /// 形如 `http://127.0.0.1:38291`，拼路径直接用。
    pub base_url: String,
    pub client: reqwest::Client,
    /// 同一个存储句柄，测试可以绕过 HTTP 直接检查落盘结果。
    pub store: Store,
    /// 持有临时目录，drop 时清理。
    pub dir: TempDir,
}

impl TestApp {
    /// 拼出完整 URL。
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .send()
            .await
            .expect("请求失败")
    }

    /// 带 Bearer token 的 GET。
    pub async fn get_auth(&self, path: &str, token: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .bearer_auth(token)
            .send()
            .await
            .expect("请求失败")
    }

    /// POST JSON。
    pub async fn post_json(&self, path: &str, body: serde_json::Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .json(&body)
            .send()
            .await
            .expect("请求失败")
    }

    /// 带 Bearer token 的 POST JSON。
    pub async fn post_json_auth(
        &self,
        path: &str,
        body: serde_json::Value,
        token: &str,
    ) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .expect("请求失败")
    }

    /// 带 Bearer token 的 PATCH JSON。
    pub async fn patch_json_auth(
        &self,
        path: &str,
        body: serde_json::Value,
        token: &str,
    ) -> reqwest::Response {
        self.client
            .patch(self.url(path))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .expect("请求失败")
    }

    /// 带 Bearer token 的 PUT，body 是原始字节。
    pub async fn put_bytes_auth(
        &self,
        path: &str,
        body: Vec<u8>,
        token: &str,
    ) -> reqwest::Response {
        self.client
            .put(self.url(path))
            .bearer_auth(token)
            .body(body)
            .send()
            .await
            .expect("请求失败")
    }

    /// 带 Bearer token 的 POST，body 是原始字节。
    pub async fn post_bytes_auth(
        &self,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
        token: &str,
    ) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .expect("请求失败")
    }

    /// 带 Bearer token 的 DELETE。
    pub async fn delete_auth(&self, path: &str, token: &str) -> reqwest::Response {
        self.client
            .delete(self.url(path))
            .bearer_auth(token)
            .send()
            .await
            .expect("请求失败")
    }

    /// 注册一个用户，返回响应（调用方自己断言状态码）。
    pub async fn register(&self, username: &str, password: &str) -> reqwest::Response {
        self.post_json(
            "/api/auth/register",
            serde_json::json!({ "username": username, "password": password }),
        )
        .await
    }

    /// 登录，返回响应。
    pub async fn login(&self, username: &str, password: &str) -> reqwest::Response {
        self.post_json(
            "/api/auth/login",
            serde_json::json!({ "username": username, "password": password }),
        )
        .await
    }

    /// 注册并登录，返回唯一的 JWT。
    ///
    /// 大多数测试只是需要「一个能用的身份」，不关心注册与登录的细节。
    pub async fn signup(&self, username: &str, password: &str) -> String {
        let r = self.register(username, password).await;
        assert_eq!(
            r.status(),
            201,
            "注册应成功: {}",
            r.text().await.unwrap_or_default()
        );

        let r = self.login(username, password).await;
        assert_eq!(r.status(), 200, "登录应成功");
        let body: serde_json::Value = r.json().await.expect("响应应是 JSON");
        body["access_token"]
            .as_str()
            .expect("应有 access_token")
            .to_owned()
    }
}

/// 起一个默认配置的服务。
pub async fn spawn_app() -> TestApp {
    spawn_app_with(|_| {}).await
}

/// 起一个服务，`mutate` 里可以改配置（例如关掉 docs 开关）。
///
/// 监听 `127.0.0.1:0` 拿随机端口，所以多个测试可以并行跑，互不抢端口。
pub async fn spawn_app_with(mutate: impl FnOnce(&mut Config)) -> TestApp {
    let dir = tempfile::tempdir().expect("创建临时目录");

    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        jwt: one_click_cut_server::config::JwtConfig {
            // 至少 32 字节，否则 AppState 会拒绝构造。
            secret: "test-secret-not-for-production-32+".to_owned(),
        },
        ..Config::default()
    };
    mutate(&mut config);

    let store = Store::open_with(config.rocksdb_dir(), &StoreOptions::small()).expect("打开存储");
    let state = AppState::new(config, store).expect("构造 AppState");
    let store_handle = state.store.clone();
    let app = one_click_cut_server::http::router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定随机端口");
    let addr = listener.local_addr().expect("读取本地地址");

    tokio::spawn(async move {
        // 测试进程退出时这个任务被直接丢弃，不需要优雅关闭。
        let _ = axum::serve(listener, app).await;
    });

    TestApp {
        base_url: format!("http://{addr}"),
        client: reqwest::Client::builder()
            .build()
            .expect("构建 HTTP 客户端"),
        store: store_handle,
        dir,
    }
}

/// 造一段可复现的伪随机内容。
///
/// 不用真随机：测试失败时要能精确复现。也不用全 0 或常量填充——那样压缩率与
/// 真实素材差太远，验证 zstd 与写放大的测试会得出假结论。
///
/// **不同的 `seed` 必须产生不同的内容**，否则去重会把它们合成一份，
/// 让「N 个不同文件」的测试静默变成「1 个文件」。所以先把种子过一遍乘法散列
/// 再置低位，而不是直接 `seed | 1`（那样 `2|1 == 3|1`，相邻种子会撞）。
pub fn pseudo_content(len: usize, seed: u64) -> Vec<u8> {
    let mut x = (seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1) as u32;
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        // xorshift32，够乱且完全可复现。
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        v.push((x & 0xff) as u8);
    }
    v
}

/// 递归统计目录占用。
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}
