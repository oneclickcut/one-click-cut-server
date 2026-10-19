#!/usr/bin/env bash
# 两个入口共享数据库格式，发布时必须一起构建和更新。
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$project_dir"

# 显式指定输出目录，保证复制的是本次构建产物。
cargo build --release --bins --target-dir "$project_dir/target"
for binary in one-click-cut-server reset_admin_password; do
    staged_binary="$(mktemp "$project_dir/.${binary}.XXXXXX")"
    trap 'rm -f -- "$staged_binary"' EXIT
    install -m 755 "target/release/$binary" "$staged_binary"
    mv -f -- "$staged_binary" "$project_dir/$binary"
done
trap - EXIT
echo '已同步更新 ./one-click-cut-server 和 ./reset_admin_password；运行中的服务需重启。'
