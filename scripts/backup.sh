#!/usr/bin/env bash
# RocksDB checkpoint 备份。
#
# 单机单盘是这套架构最大的风险，而全部内容入 KV 把风险集中到了一个目录上——
# **备份不再是可选项，它是唯一防线。**
#
# checkpoint 用硬链接，几乎不占额外空间也不阻塞写入，而且快照是原子的：
# 不会拍到一个「元数据新、内容旧」的中间状态。这是 RocksDB 比裸文件树更适合
# 做一致性备份的地方。
#
# 用法：
#   scripts/backup.sh                        # 用默认路径
#   OCC_DATA_DIR=/data scripts/backup.sh     # 指定数据目录
#   OCC_BACKUP_KEEP=4 scripts/backup.sh      # 保留几份
#
# 建议的 systemd timer 频率：每 6 小时。
set -euo pipefail

DATA_DIR="${OCC_DATA_DIR:-/data}"
DB_DIR="$DATA_DIR/rocksdb"
BACKUP_ROOT="$DATA_DIR/backup"
KEEP="${OCC_BACKUP_KEEP:-4}"

log() { printf '[backup] %s\n' "$*"; }
die() { printf '[backup] 错误: %s\n' "$*" >&2; exit 1; }

[[ -d "$DB_DIR" ]] || die "数据库目录不存在: $DB_DIR"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TARGET="$BACKUP_ROOT/$STAMP"

mkdir -p "$BACKUP_ROOT"
[[ -e "$TARGET" ]] && die "目标已存在: $TARGET"

# ==== 为什么用 cp -al 而不是 rsync ====
#
# RocksDB 的 SST 与 blob 文件是**只增不改**的（它从不原地修改已写入的文件），
# 所以硬链接快照是安全的：即使之后有新的写入，已链接的那些文件内容不会变。
#
# 真正的 `create_checkpoint` API 还会处理 MANIFEST 与 WAL 的一致性，
# 这个脚本用 `/admin/checkpoint` 接口触发才最稳；纯 shell 版本作为
# 「服务没起来时也能备份」的兜底。
if [[ -n "${OCC_ADMIN_TOKEN:-}" && -n "${OCC_BASE_URL:-}" ]]; then
    log "通过服务接口触发一致性 checkpoint..."
    curl -fsS -X POST "$OCC_BASE_URL/api/admin/checkpoint" \
        -H "authorization: Bearer $OCC_ADMIN_TOKEN" \
        -H 'content-type: application/json' \
        -d "{\"target\":\"$TARGET\"}" \
        || die "接口触发失败；服务未运行时请停机后再跑本脚本"
else
    log "服务接口不可用，退回硬链接快照（要求此刻没有写入，或接受 WAL 未刷新的风险）"
    log "  设置 OCC_BASE_URL 与 OCC_ADMIN_TOKEN 可走一致性 checkpoint"
    cp -al "$DB_DIR" "$TARGET"
fi

SIZE="$(du -sh "$TARGET" 2>/dev/null | cut -f1)"
COUNT="$(find "$TARGET" -type f | wc -l)"
log "快照完成: $TARGET（$COUNT 个文件，表观 $SIZE，硬链接实际占用远小于此）"

# 只保留最近 KEEP 份。
mapfile -t OLD < <(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d | sort -r | tail -n "+$((KEEP + 1))")
for d in "${OLD[@]:-}"; do
    [[ -n "$d" ]] || continue
    log "删除旧快照: $d"
    rm -rf "$d"
done

log "当前保留 $(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d | wc -l) 份快照"

# 异地同步：这一步才是真正的防线。
# 本地快照挡不住盘坏，而盘坏是单机单盘方案最可能的失败方式。
if [[ -n "${OCC_BACKUP_REMOTE:-}" ]]; then
    log "同步到异地: $OCC_BACKUP_REMOTE"
    # SST 与 blob 文件只增不改，所以增量效率很高。
    rsync -a --delete "$BACKUP_ROOT/" "$OCC_BACKUP_REMOTE/"
    log "异地同步完成"
else
    log "警告: 未设置 OCC_BACKUP_REMOTE，只有本地快照——盘坏即全丢"
fi
