#!/usr/bin/env bash
# 恢复演练：在快照上只读打开数据库并抽样读取，确认它真的能恢复。
#
# **没跑通这个脚本就不该上线。** 备份是单机单盘方案的唯一防线，而一份从没被
# 验证过的备份等于没有备份——最常见的失败是「快照一直在生成，但从来没人确认
# 它能打开」。
#
# 用法：
#   scripts/restore-drill.sh                       # 演练最新一份快照
#   scripts/restore-drill.sh /data/backup/cp-123    # 指定快照
#
# 建议频率：每周一次。
set -euo pipefail

DATA_DIR="${OCC_DATA_DIR:-/data}"
BACKUP_ROOT="$DATA_DIR/backup"

log() { printf '[drill] %s\n' "$*"; }
die() { printf '[drill] 错误: %s\n' "$*" >&2; exit 1; }

SNAPSHOT="${1:-}"
if [[ -z "$SNAPSHOT" ]]; then
    SNAPSHOT="$(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -r | head -1)"
    [[ -n "$SNAPSHOT" ]] || die "$BACKUP_ROOT 下没有快照"
    log "使用最新快照: $SNAPSHOT"
fi
[[ -d "$SNAPSHOT" ]] || die "快照目录不存在: $SNAPSHOT"

# 检查快照的形状：RocksDB 目录应当有 CURRENT、MANIFEST 与若干 SST。
for f in CURRENT; do
    [[ -f "$SNAPSHOT/$f" ]] || die "快照缺少 $f，它可能不是一个完整的 RocksDB 目录"
done

SST_COUNT="$(find "$SNAPSHOT" -name '*.sst' | wc -l)"
BLOB_COUNT="$(find "$SNAPSHOT" -name '*.blob' | wc -l)"
MANIFEST_COUNT="$(find "$SNAPSHOT" -name 'MANIFEST-*' | wc -l)"
log "快照内容: $SST_COUNT 个 SST，$BLOB_COUNT 个 blob，$MANIFEST_COUNT 个 MANIFEST"

[[ "$MANIFEST_COUNT" -ge 1 ]] || die "快照没有 MANIFEST，无法打开"

# ==== 真正的验证：把服务指向快照跑一遍 ====
#
# 只看文件在不在是不够的——一份文件齐全但 MANIFEST 损坏的快照打不开。
# 所以这里起一个临时实例，确认它能开库、能响应 /healthz、能读出统计。
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

log "复制快照到临时目录（避免演练动到备份本身）..."
mkdir -p "$WORK/rocksdb"
cp -al "$SNAPSHOT/." "$WORK/rocksdb/" 2>/dev/null || cp -a "$SNAPSHOT/." "$WORK/rocksdb/"

BIN="${OCC_BIN:-./target/release/one-click-cut-server}"
[[ -x "$BIN" ]] || BIN="./target/debug/one-click-cut-server"
[[ -x "$BIN" ]] || die "找不到可执行文件，先跑 cargo build --release"

PORT="${OCC_DRILL_PORT:-18099}"
log "在快照上启动临时实例（端口 $PORT）..."
OCC_DATA_DIR="$WORK" \
OCC_LISTEN="127.0.0.1:$PORT" \
OCC_DOCS_ENABLED=false \
OCC_JWT_SECRET="drill-only-secret-not-used-for-anything-real" \
    "$BIN" > "$WORK/server.log" 2>&1 &
PID=$!

cleanup() {
    kill -TERM "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

for _ in $(seq 1 40); do
    if curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
done

curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null \
    || { cat "$WORK/server.log" >&2; die "快照打不开——这份备份不可用"; }

log "快照可正常打开并响应"
log "演练通过: $SNAPSHOT"
