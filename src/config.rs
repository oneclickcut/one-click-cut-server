//! 配置加载：toml 文件为基准，环境变量覆盖。
//!
//! 环境变量命名规则：`OCC_` + 层级路径大写下划线，例如
//! `OCC_LISTEN`、`OCC_DATA_DIR`、`OCC_DOCS_ENABLED`、`OCC_JWT_SECRET`。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 顶层配置。字段都带默认值，所以配置文件缺失也能起来（开发态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 监听地址。生产只监听回环，由 cloudflared 转发。
    pub listen: SocketAddr,
    /// 数据根目录，RocksDB 与备份都在它下面。
    pub data_dir: PathBuf,
    pub log: LogConfig,
    pub docs: DocsConfig,
    pub jwt: JwtConfig,
    pub admin: AdminConfig,
    pub upload: UploadConfig,
    pub task_queue: TaskQueueConfig,
    pub gallery: GalleryConfig,
    pub plans: PlansConfig,
    pub registration: RegistrationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanLimits {
    pub storage_bytes: u64,
    pub monthly_push_limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlansConfig {
    #[serde(default = "basic_limits")]
    pub basic: PlanLimits,
    #[serde(default = "pro_limits")]
    pub pro: PlanLimits,
    #[serde(default = "max_limits")]
    pub max: PlanLimits,
}

fn basic_limits() -> PlanLimits {
    PlanLimits {
        storage_bytes: 5_000_000_000,
        monthly_push_limit: 5,
    }
}
fn pro_limits() -> PlanLimits {
    PlanLimits {
        storage_bytes: 50_000_000_000,
        monthly_push_limit: 50,
    }
}
fn max_limits() -> PlanLimits {
    PlanLimits {
        storage_bytes: 200_000_000_000,
        monthly_push_limit: 300,
    }
}

impl Default for PlansConfig {
    fn default() -> Self {
        Self {
            basic: basic_limits(),
            pro: pro_limits(),
            max: max_limits(),
        }
    }
}

impl PlansConfig {
    pub fn validate(&self) -> Result<()> {
        for (name, limits) in [
            ("basic", &self.basic),
            ("pro", &self.pro),
            ("max", &self.max),
        ] {
            anyhow::ensure!(
                limits.storage_bytes > 0,
                "plans.{name}.storage_bytes 必须大于 0"
            );
            anyhow::ensure!(
                limits.monthly_push_limit > 0,
                "plans.{name}.monthly_push_limit 必须大于 0"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RegistrationConfig {
    pub daily_limit: u64,
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        Self { daily_limit: 100 }
    }
}

/// 独立于队列读取资格的跨用户发布授权。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GalleryConfig {
    pub publisher_usernames: Vec<String>,
}
impl GalleryConfig {
    pub fn validate(&self) -> Result<()> {
        for username in &self.publisher_usernames {
            crate::storage::keys::validate_username(username)
                .context("gallery.publisher_usernames 中有非法用户名")?;
        }
        Ok(())
    }
}

/// 公共任务队列。所有登录用户可提交，只有名单中的用户可领取。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskQueueConfig {
    /// 同时作为永久 JWT 的资格名单；移出并重启后旧永久 JWT 被拒绝。
    pub reader_usernames: Vec<String>,
    pub max_payload_bytes: usize,
}

impl Default for TaskQueueConfig {
    fn default() -> Self {
        Self {
            reader_usernames: Vec::new(),
            max_payload_bytes: 64 << 10,
        }
    }
}

impl TaskQueueConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.max_payload_bytes > 0,
            "task_queue.max_payload_bytes 必须大于 0"
        );
        for username in &self.reader_usernames {
            crate::storage::keys::validate_username(username)
                .context("task_queue.reader_usernames 中有非法用户名")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// `tracing-subscriber` 的 EnvFilter 表达式。
    pub filter: String,
    /// true 输出 JSON 结构化日志，false 输出人读格式。
    pub json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DocsConfig {
    /// 是否挂载 Swagger UI 与 openapi.json。生产默认关。
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct JwtConfig {
    /// HS256 签名密钥，至少 32 字节。生产必须通过环境变量注入，不要写进配置文件。
    ///
    /// 默认空串，启动时会因为太短而直接失败——**宁可起不来，也不要带着弱密钥上线**。
    pub secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadConfig {
    /// 单次 `PUT` 请求体的上限（字节）。
    ///
    /// axum 默认只有 2MB，对这个项目远远不够。设成 32MB 是因为接口文档第 5 节
    /// 规定「32MB 以上必须走分块上传」——分块除了断点续传，另一个硬理由是
    /// Cloudflare 免费版单请求体上限 100MB。
    ///
    /// 超过这个值返回 413，调用方应改走 `/api/canvases/{id}/uploads`。
    pub max_single_bytes: usize,
    /// 单个分片 / 单个 tar 批量请求体的上限（字节）。
    ///
    /// 接口文档第 7 节：每个 part 控制在 50MB 以内，给 Cloudflare 免费版的
    /// 100MB 单请求体上限留余量。
    pub max_part_bytes: usize,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_single_bytes: 32 << 20,
            max_part_bytes: 50 << 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminConfig {
    /// 管理账户的用户名。首次启动时自动创建并打印一次性初始密码。
    pub username: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            username: "admin".to_owned(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:3823".parse().expect("字面量地址可解析"),
            data_dir: PathBuf::from("/data"),
            log: LogConfig::default(),
            docs: DocsConfig::default(),
            jwt: JwtConfig::default(),
            admin: AdminConfig::default(),
            upload: UploadConfig::default(),
            task_queue: TaskQueueConfig::default(),
            gallery: GalleryConfig::default(),
            plans: PlansConfig::default(),
            registration: RegistrationConfig::default(),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: "info,one_click_cut_server=debug".to_owned(),
            json: false,
        }
    }
}

impl Default for DocsConfig {
    fn default() -> Self {
        // 开发期默认开，部署时用配置或环境变量关掉。
        Self { enabled: true }
    }
}

impl Config {
    /// 按「默认值 → toml 文件 → 环境变量」三层叠加加载。
    ///
    /// `path` 不存在不算错误，直接用默认值继续，方便本机 `cargo run`。
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut cfg = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("读取配置文件失败: {}", p.display()))?;
                toml::from_str(&text)
                    .with_context(|| format!("解析配置文件失败: {}", p.display()))?
            }
            _ => Self::default(),
        };
        cfg.apply_env()?;
        cfg.task_queue.validate()?;
        cfg.gallery.validate()?;
        cfg.plans.validate()?;
        Ok(cfg)
    }

    /// 环境变量覆盖。只覆盖显式设置的项。
    fn apply_env(&mut self) -> Result<()> {
        if let Some(v) = env_var("OCC_LISTEN") {
            self.listen = v.parse().context("OCC_LISTEN 不是合法的 host:port")?;
        }
        if let Some(v) = env_var("OCC_DATA_DIR") {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = env_var("OCC_LOG_FILTER") {
            self.log.filter = v;
        }
        if let Some(v) = env_var("OCC_LOG_JSON") {
            self.log.json = parse_bool(&v, "OCC_LOG_JSON")?;
        }
        if let Some(v) = env_var("OCC_DOCS_ENABLED") {
            self.docs.enabled = parse_bool(&v, "OCC_DOCS_ENABLED")?;
        }
        if let Some(v) = env_var("OCC_JWT_SECRET") {
            self.jwt.secret = v;
        }
        if let Some(v) = env_var("OCC_ADMIN_USERNAME") {
            self.admin.username = v;
        }
        if let Some(v) = env_var("OCC_UPLOAD_MAX_SINGLE_MB") {
            let mb: usize = v.parse().context("OCC_UPLOAD_MAX_SINGLE_MB 不是合法整数")?;
            self.upload.max_single_bytes = mb << 20;
        }
        if let Some(v) = env_var("OCC_GALLERY_PUBLISHER_USERNAMES") {
            self.gallery.publisher_usernames = serde_json::from_str(&v)
                .context("OCC_GALLERY_PUBLISHER_USERNAMES 必须是 JSON 字符串数组")?;
        }
        // JSON 数组；用 [] 可以显式清空文件中的领取名单。
        if let Some(v) = env_var("OCC_TASK_QUEUE_READER_USERNAMES") {
            self.task_queue.reader_usernames = serde_json::from_str(&v)
                .context("OCC_TASK_QUEUE_READER_USERNAMES 必须是 JSON 字符串数组")?;
        }
        if let Some(v) = env_var("OCC_TASK_QUEUE_MAX_PAYLOAD_BYTES") {
            self.task_queue.max_payload_bytes = v
                .parse()
                .context("OCC_TASK_QUEUE_MAX_PAYLOAD_BYTES 不是合法整数")?;
        }
        if let Some(v) = env_var("OCC_REGISTRATION_DAILY_LIMIT") {
            self.registration.daily_limit = v
                .parse()
                .context("OCC_REGISTRATION_DAILY_LIMIT 不是合法整数")?;
        }
        for (name, limits) in [
            ("BASIC", &mut self.plans.basic),
            ("PRO", &mut self.plans.pro),
            ("MAX", &mut self.plans.max),
        ] {
            if let Some(v) = env_var(&format!("OCC_PLANS_{name}_STORAGE_BYTES")) {
                limits.storage_bytes = v
                    .parse()
                    .with_context(|| format!("OCC_PLANS_{name}_STORAGE_BYTES 不是合法整数"))?;
            }
            if let Some(v) = env_var(&format!("OCC_PLANS_{name}_MONTHLY_PUSH_LIMIT")) {
                limits.monthly_push_limit = v
                    .parse()
                    .with_context(|| format!("OCC_PLANS_{name}_MONTHLY_PUSH_LIMIT 不是合法整数"))?;
            }
        }
        Ok(())
    }

    /// RocksDB 数据目录。
    pub fn rocksdb_dir(&self) -> PathBuf {
        self.data_dir.join("rocksdb")
    }

    /// checkpoint 备份目录。
    pub fn backup_dir(&self) -> PathBuf {
        self.data_dir.join("backup")
    }
}

/// 读环境变量，空串按未设置处理（避免 `OCC_X=` 意外清空配置）。
fn env_var(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

fn parse_bool(raw: &str, key: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => anyhow::bail!("{key} 不是合法布尔值: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认值可用() {
        let cfg = Config::default();
        assert_eq!(cfg.listen.port(), 3823);
        assert!(cfg.docs.enabled);
        assert_eq!(cfg.rocksdb_dir(), PathBuf::from("/data/rocksdb"));
        assert_eq!(cfg.backup_dir(), PathBuf::from("/data/backup"));
        assert_eq!(
            (
                cfg.plans.basic.storage_bytes,
                cfg.plans.basic.monthly_push_limit
            ),
            (5_000_000_000, 5)
        );
        assert_eq!(
            (
                cfg.plans.pro.storage_bytes,
                cfg.plans.pro.monthly_push_limit
            ),
            (50_000_000_000, 50)
        );
        assert_eq!(
            (
                cfg.plans.max.storage_bytes,
                cfg.plans.max.monthly_push_limit
            ),
            (200_000_000_000, 300)
        );
        assert_eq!(cfg.registration.daily_limit, 100);
    }

    #[test]
    fn 解析toml部分字段其余走默认() {
        let cfg: Config = toml::from_str(
            r#"
            listen = "0.0.0.0:9000"
            data_dir = "/srv/occ"

            [docs]
            enabled = false
            "#,
        )
        .expect("应能解析");
        assert_eq!(cfg.listen.to_string(), "0.0.0.0:9000");
        assert_eq!(cfg.data_dir, PathBuf::from("/srv/occ"));
        assert!(!cfg.docs.enabled);
        // 未出现在 toml 里的段落走默认值
        assert!(!cfg.log.json);
    }

    #[test]
    fn 只配置一个套餐时其他套餐保持各自默认值() {
        let cfg: Config =
            toml::from_str("[plans.pro]\nstorage_bytes = 60000000000\nmonthly_push_limit = 60\n")
                .unwrap();
        assert_eq!(cfg.plans.basic.storage_bytes, 5_000_000_000);
        assert_eq!(cfg.plans.pro.storage_bytes, 60_000_000_000);
        assert_eq!(cfg.plans.max.storage_bytes, 200_000_000_000);
    }

    #[test]
    fn 布尔解析容忍多种写法() {
        for s in ["1", "true", "TRUE", "yes", "on"] {
            assert!(parse_bool(s, "K").expect("合法"));
        }
        for s in ["0", "false", "No", "off"] {
            assert!(!parse_bool(s, "K").expect("合法"));
        }
        assert!(parse_bool("maybe", "K").is_err());
    }

    #[test]
    fn 缺失配置文件不算错误() {
        let cfg = Config::load(Some(Path::new("/nonexistent/occ.toml"))).expect("应回落到默认值");
        assert_eq!(cfg.listen.port(), 3823);
    }
}
