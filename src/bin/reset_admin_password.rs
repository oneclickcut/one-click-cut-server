//! 命令行工具：直接改 RocksDB，重置管理员密码。
//!
//! 不走 HTTP 接口——用在忘记密码、又没有可用 token 的场景，跳过登录鉴权直接
//! 改存储层。复用 `domain::identity` 里跟服务端登录校验同一套 argon2 哈希逻辑，
//! 保证改出来的密码日后能正常登录。
//!
//! 用法：
//!   cargo run --bin reset_admin_password -- <用户名> <新密码> [配置文件路径]
//!
//! 运行前必须确认目标 RocksDB 目录没有被服务进程占用，否则打不开库。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use one_click_cut_server::{Config, Store, domain::identity};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let username = args.next().context("缺少参数：用户名")?;
    let new_password = args.next().context("缺少参数：新密码")?;
    let config_path = args
        .next()
        .or_else(|| std::env::var("OCC_CONFIG").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    if args.next().is_some() {
        bail!("用法: reset_admin_password <用户名> <新密码> [配置文件路径]");
    }

    identity::validate_password(&new_password)?;

    let config = Config::load(Some(&config_path))
        .with_context(|| format!("读取配置文件失败: {}", config_path.display()))?;
    let db_dir = config.rocksdb_dir();
    // 服务启动允许创建新库，密码恢复只能操作已有库，避免路径错误时误建空库。
    if !db_dir.join("CURRENT").is_file() {
        bail!(
            "数据库不存在: {}；请使用与服务相同的配置文件、工作目录和 OCC_DATA_DIR",
            db_dir.display()
        );
    }

    println!("[reset_admin_password] 打开数据库: {}", db_dir.display());
    let store = Store::open(&db_dir)
        .with_context(|| format!(
            "打开 RocksDB 失败: {}；若包含 LOCK 请先停止服务；若包含 Column families not opened，请运行 scripts/build-release.sh 同步更新服务与重置工具",
            db_dir.display()
        ))?;

    let Some((uid, mut user)) = identity::find_by_username(&store, &username).await? else {
        bail!("用户不存在: {username}");
    };
    if !user.role.is_admin() {
        bail!("用户 {username}（uid={uid}）不是管理员，本工具只用于重置管理员密码");
    }

    user.pwd_hash = identity::hash_password(&new_password)?;
    identity::save(&store, uid, &user).await?;
    store.flush().await?;

    println!("[reset_admin_password] 已重置密码: username={username} uid={uid}");
    if user.disabled {
        println!(
            "[reset_admin_password] 注意：该账户已被禁用，仅重置密码不会解除禁用，仍无法登录。"
        );
    }
    println!(
        "[reset_admin_password] 本操作仅修改密码，已签发的 JWT 不会因此失效；需要立即阻断时请禁用账号。"
    );
    Ok(())
}
