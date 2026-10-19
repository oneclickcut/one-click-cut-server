//! 后台维护任务与统计。
//!
//! # 三个不同节奏的任务
//!
//! | 任务 | 频率 | 做什么 |
//! |------|------|--------|
//! | GC 一轮 | 每分钟 | 处理待删队列里到期的项 |
//! | 过期凭证清理 | 每小时 | agent token、超时的上传会话 |
//! | 对账 | 每天 | 孤儿 chunk 差集、blobmeta 悬挂指针（后者告警） |
//!
//! 分开是因为它们的代价差几个数量级：GC 一轮只扫队列头，对账要扫全部内容键。

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

use crate::storage::{Cf, Store, gc};
use crate::util;

/// GC 每轮最多处理多少项。
///
/// 一次删几十万项会让后台任务卡住太久，期间别的写入都要排在它后面。
const GC_BATCH: usize = 1000;

/// 对账每轮最多扫多少键。
const AUDIT_BATCH: usize = 100_000;

/// 各任务的间隔。
const GC_INTERVAL: Duration = Duration::from_secs(60);
const CREDENTIAL_INTERVAL: Duration = Duration::from_secs(3600);
const AUDIT_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// 起后台维护任务。
///
/// 返回的 handle 在进程退出时被丢弃即可——任务里没有需要 flush 的状态，
/// 每一轮本身是原子的。
pub fn spawn(store: Store) -> Vec<tokio::task::JoinHandle<()>> {
    let store = Arc::new(store);

    let gc_store = Arc::clone(&store);
    let gc_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(GC_INTERVAL);
        // 跳过第一次立即触发：启动瞬间正忙着开库与建索引。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = gc::run_once(&gc_store, util::now_secs(), GC_BATCH).await {
                // 一轮失败不该让任务退出——下一分钟再试。
                tracing::error!(error = ?e, "GC 一轮失败");
            }
        }
    });

    let cred_store = Arc::clone(&store);
    let cred_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CREDENTIAL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = purge_credentials(&cred_store, util::now_secs()).await {
                tracing::error!(error = ?e, "清理过期凭证失败");
            }
        }
    });

    let audit_store = Arc::clone(&store);
    let audit_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(AUDIT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            match gc::audit(&audit_store, AUDIT_BATCH).await {
                Ok(report) => {
                    // 孤儿是崩溃恢复的预期残留，直接清掉。
                    if !report.orphan_chunks.is_empty()
                        && let Err(e) = gc::purge_orphans(&audit_store, &report.orphan_chunks).await
                    {
                        tracing::error!(error = ?e, "清理孤儿块失败");
                    }
                    // 悬挂 blobmeta 是数据损坏，gc::audit 内部已经 error 告警，
                    // 这里不自动「清理」——那等于承认数据丢了。
                }
                Err(e) => tracing::error!(error = ?e, "对账失败"),
            }
        }
    });

    vec![gc_task, cred_task, audit_task]
}

/// 清理过期的凭证与上传会话。
pub async fn purge_credentials(store: &Store, now: u64) -> Result<PurgeReport> {
    let agent = crate::domain::agent::purge_expired(store, now, 10_000).await?;
    let uploads = crate::domain::upload::purge_expired(store, now, 1_000).await?;

    let report = PurgeReport {
        agent_tokens: agent,
        upload_sessions: uploads,
    };
    if !report.is_empty() {
        tracing::info!(
            agent = report.agent_tokens,
            uploads = report.upload_sessions,
            "已清理过期凭证与会话"
        );
    }
    Ok(report)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PurgeReport {
    pub agent_tokens: u64,
    pub upload_sessions: u64,
}

impl PurgeReport {
    pub fn is_empty(&self) -> bool {
        self.agent_tokens == 0 && self.upload_sessions == 0
    }
}

/// 全局统计，给 admin 接口用。
#[derive(Debug, Clone)]
pub struct Stats {
    pub gallery_completion_receipts: u64,
    pub users: u64,
    pub canvases: u64,
    pub files: u64,
    /// 画布内目录记录数（`fd|` 前缀）。
    ///
    /// 它不参与配额也不参与容量统计，纳进来纯粹是为了能观察规模——
    /// 这是个「无限写小 key」风险点，虽然有单画布 500 的上限兜底。
    pub canvas_dirs: u64,
    /// 全部画布的 `bytes` 之和：用户视角的逻辑总量。
    pub logical_bytes: u64,
    /// blob 条数（去重后的内容份数）。
    pub blobs: u64,
    /// chunk 条数。
    pub chunks: u64,
    /// 数据目录实际占用。
    pub disk_bytes: u64,
    /// 待删队列深度。
    pub gc_pending: u64,
    /// 未 finalize 的上传会话数。
    pub upload_sessions: u64,
}

impl Stats {
    /// 去重率：逻辑总量与物理占用的比值。
    ///
    /// 大于 1 说明去重在起作用。这是「内容寻址」这条设计唯一能被直接观测到的
    /// 收益指标。
    pub fn dedup_ratio(&self) -> f64 {
        if self.disk_bytes == 0 {
            return 1.0;
        }
        self.logical_bytes as f64 / self.disk_bytes as f64
    }
}

/// 收集全局统计。
///
/// 走的是全表扫描，所以**只给 admin 接口用**，不要放进任何高频路径。
pub async fn collect_stats(store: &Store) -> Result<Stats> {
    use crate::storage::keys;
    use crate::storage::records;

    let users = store
        .scan_prefix_keys(Cf::Meta, keys::user_prefix(), usize::MAX)
        .await?
        .len() as u64;

    let canvas_rows = store
        .scan_prefix_limit(Cf::Meta, keys::canvas_prefix(), usize::MAX)
        .await?;
    let mut logical_bytes = 0u64;
    let mut files = 0u64;
    for (_, value) in &canvas_rows {
        let canvas: records::Canvas = records::decode(value)?;
        logical_bytes = logical_bytes.saturating_add(canvas.bytes);
        files = files.saturating_add(canvas.file_count);
    }
    // 目录记录单独扫一遍。`fd|` 与 `f|` 的第二个字节不同（`d` vs `0x00`），
    // 所以这一趟扫不到文件条目，文件那边也扫不到目录记录。
    let canvas_dirs = store
        .scan_prefix_keys(Cf::Meta, keys::canvas_dir_prefix_all(), usize::MAX)
        .await?
        .len() as u64;

    let blobs = store
        .scan_prefix_keys(Cf::BlobMeta, keys::blob_meta_prefix(), usize::MAX)
        .await?
        .len() as u64;
    let chunks = store
        .scan_prefix_keys(Cf::Chunk, Vec::new(), usize::MAX)
        .await?
        .len() as u64;

    let gc_pending = (store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Blob), usize::MAX)
        .await?
        .len()
        + store
            .scan_prefix_keys(Cf::Gc, keys::gc_prefix(keys::GcKind::Chunk), usize::MAX)
            .await?
            .len()) as u64;

    let upload_sessions = store
        .scan_prefix_keys(Cf::Stage, keys::stage_info_prefix(), usize::MAX)
        .await?
        .len() as u64;

    let mut gallery_completion_receipts = 0;
    let mut after = None;
    loop {
        let rows = store
            .scan_prefix_page(Cf::Meta, keys::gallery_done_prefix_all(), after, false, 256)
            .await?;
        if rows.is_empty() {
            break;
        }
        gallery_completion_receipts += rows.len() as u64;
        after = rows.last().map(|(key, _)| key.clone());
    }
    Ok(Stats {
        gallery_completion_receipts,
        users,
        canvases: canvas_rows.len() as u64,
        files,
        canvas_dirs,
        logical_bytes,
        blobs,
        chunks,
        disk_bytes: dir_size(store.path()),
        gc_pending,
        upload_sessions,
    })
}

/// 递归统计目录占用。
fn dir_size(path: &std::path::Path) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(logical: u64, disk: u64) -> Stats {
        Stats {
            gallery_completion_receipts: 0,
            users: 1,
            canvases: 1,
            files: 1,
            canvas_dirs: 0,
            logical_bytes: logical,
            blobs: 1,
            chunks: 0,
            disk_bytes: disk,
            gc_pending: 0,
            upload_sessions: 0,
        }
    }

    #[test]
    fn 去重率反映实际收益() {
        // 逻辑 300MB、物理 100MB → 去重率 3。
        assert!((stats(300, 100).dedup_ratio() - 3.0).abs() < 1e-9);
        // 没有去重时约等于 1。
        assert!((stats(100, 100).dedup_ratio() - 1.0).abs() < 1e-9);
        // 空库不该除以零。
        assert!((stats(0, 0).dedup_ratio() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn 清理报告的空判断() {
        assert!(PurgeReport::default().is_empty());
        assert!(
            !PurgeReport {
                agent_tokens: 1,
                ..Default::default()
            }
            .is_empty()
        );
    }
}
