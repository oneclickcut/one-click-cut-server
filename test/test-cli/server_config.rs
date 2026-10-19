//! 使用真实服务 CLI 验证配置检查、环境覆盖及错误路径；不启动数据库。
use std::process::Command;

fn command(dir: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_one-click-cut-server"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("OCC_") {
            command.env_remove(key);
        }
    }
    command.current_dir(dir);
    command
}

fn config(dir: &std::path::Path) -> std::path::PathBuf {
    let file = dir.join("server.toml");
    std::fs::write(&file, "[jwt]\nsecret='must-never-appear-in-check-output'\n[gallery]\npublisher_usernames=['worker1']\n[task_queue]\nreader_usernames=['worker1','reader']\n").unwrap();
    file
}

#[test]
fn 配置检查返回生效名单但不暴露密钥或创建数据库() {
    let dir = tempfile::tempdir().unwrap();
    let file = config(dir.path());
    let output = command(dir.path())
        .arg(&file)
        .arg("--check-config")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["config_path"], file.to_str().unwrap());
    assert_eq!(body["publisher_usernames"], serde_json::json!(["worker1"]));
    assert_eq!(
        body["reader_usernames"],
        serde_json::json!(["worker1", "reader"])
    );
    assert_eq!(body["publisher_source"], "config_or_default");
    assert_eq!(body["generation_completions_enabled"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("must-never-appear"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("must-never-appear"));
    assert!(!dir.path().join("data").exists());
}

#[test]
fn 环境发布名单覆盖文件且空数组确实禁用发布() {
    let dir = tempfile::tempdir().unwrap();
    let file = config(dir.path());
    for (value, expected, enabled) in [
        (r#"["worker2"]"#, serde_json::json!(["worker2"]), true),
        ("[]", serde_json::json!([]), false),
    ] {
        let output = command(dir.path())
            .arg("--check-config")
            .env("OCC_CONFIG", &file)
            .env("OCC_GALLERY_PUBLISHER_USERNAMES", value)
            .output()
            .unwrap();
        assert!(output.status.success());
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["publisher_usernames"], expected);
        assert_eq!(body["publisher_source"], "OCC_GALLERY_PUBLISHER_USERNAMES");
        assert_eq!(body["generation_completions_enabled"], enabled);
    }
}

#[test]
fn 显式配置缺失和未知参数不能悄悄启动默认服务() {
    let dir = tempfile::tempdir().unwrap();
    for args in [vec!["missing.toml"], vec!["--check-config"], vec!["--typo"]] {
        let output = command(dir.path()).args(args).output().unwrap();
        assert!(!output.status.success());
    }
    let output = command(dir.path())
        .env("OCC_CONFIG", "missing.toml")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!dir.path().join("data").exists());
}

#[test]
fn 命令行路径优先于环境路径且非法发布名单拒绝启动() {
    let dir = tempfile::tempdir().unwrap();
    let file = config(dir.path());
    let output = command(dir.path())
        .arg("--check-config")
        .arg(&file)
        .env("OCC_CONFIG", "missing.toml")
        .output()
        .unwrap();
    assert!(output.status.success());
    for value in ["invalid-json", r#"["invalid name"]"#] {
        let output = command(dir.path())
            .arg("--check-config")
            .arg(&file)
            .env("OCC_GALLERY_PUBLISHER_USERNAMES", value)
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert!(!dir.path().join("data").exists());
}
