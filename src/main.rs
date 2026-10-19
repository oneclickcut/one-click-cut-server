//! 启动入口。只做三件事：读配置、起日志、交给 [`one_click_cut_server::serve`]。
//!
//! 业务逻辑一律在 lib 里，这样集成测试才能复用同一套启动路径。

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use one_click_cut_server::{Config, init_tracing, serve};

#[tokio::main]
async fn main() -> Result<()> {
    // 检查模式只读配置，不打开数据库、不监听端口、不执行后台维护。
    let mut check_config = false;
    let mut argument_path = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--check-config" if !check_config => check_config = true,
            "--help" | "-h" => {
                println!(
                    "用法：one-click-cut-server [配置文件] [--check-config]\n配置优先级：命令行路径 > OCC_CONFIG > ./config.toml；OCC_* 环境变量覆盖文件值。\n--check-config 仅输出非敏感配置与最终发布名单，不启动服务。"
                );
                return Ok(());
            }
            _ if arg.starts_with('-') || argument_path.is_some() => bail!("无效命令行参数"),
            _ => argument_path = Some(arg),
        }
    }
    let selected_path = argument_path.or_else(|| std::env::var("OCC_CONFIG").ok());
    let explicit_path = selected_path.is_some();
    let config_path = selected_path
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));

    // 显式路径错误不能悄悄启动为默认配置，避免名单丢失或打开错误数据目录。
    if explicit_path || check_config {
        let metadata = std::fs::metadata(&config_path)
            .with_context(|| format!("配置文件不可读取: {}", config_path.display()))?;
        ensure!(
            metadata.is_file(),
            "配置路径不是文件: {}",
            config_path.display()
        );
    }

    let config = Config::load(Some(&config_path))?;
    let resolved_path = std::env::current_dir()?.join(&config_path);
    let publisher_source = if std::env::var_os("OCC_GALLERY_PUBLISHER_USERNAMES").is_some() {
        "OCC_GALLERY_PUBLISHER_USERNAMES"
    } else {
        "config_or_default"
    };
    if check_config {
        // 显式选择字段，绝不能序列化整个 Config（其中包含 JWT 密钥）。
        println!(
            "{}",
            serde_json::json!({
                "config_path": resolved_path,
                "listen": config.listen,
                "publisher_source": publisher_source,
                "publisher_usernames": config.gallery.publisher_usernames,
                "reader_usernames": config.task_queue.reader_usernames,
                "generation_completions_enabled": !config.gallery.publisher_usernames.is_empty(),
            })
        );
        return Ok(());
    }
    init_tracing(&config.log)?;

    if config_path.exists() {
        tracing::info!(path = %resolved_path.display(), "已加载配置文件");
    } else {
        tracing::warn!(path = %config_path.display(), "配置文件不存在，使用默认值");
    }

    tracing::info!(source=publisher_source, publishers=?config.gallery.publisher_usernames,
        "生成完成通知的生效发布名单");
    if config.gallery.publisher_usernames.is_empty() {
        tracing::warn!(
            "发布名单为空，所有 Worker 的生成完成通知都会返回 403；队列读取权限不授予发布权限"
        );
    }

    serve(config).await
}
