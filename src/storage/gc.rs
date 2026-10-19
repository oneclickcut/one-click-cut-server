//! 引用归零回收与孤儿对账。
//!
//! # 两件不同的事
//!
//! | 机制 | 处理什么 | 触发 |
//! |------|----------|------|
//! | 待删队列 | 引用正常归零的内容 | 每分钟扫一次到期项 |
//! | 孤儿对账 | 崩在 WriteBatch 提交前留下的、有值无计数的块 | 每日一次 |
//!
//! 两者不能合并：队列里的项是「明确要删」，孤儿是「谁都不知道它存在」。
//!
//! # 为什么删除前要二次确认
//!
//! 排队与执行之间有一小时窗口，这期间 agent 完全可能重传相同内容，让计数
//! 从 0 复活。不确认就删会删掉正在被引用的内容——而且因为是内容寻址，
//! 症状会出现在一个看起来毫不相关的画布上。

use anyhow::Result;

use super::keys::{self, ContentHash, GcKind};
use super::kv::{Batch, Cf, Store};
use super::records::{self, BlobLayout, BlobMeta};

/// 一轮 GC 的结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    /// 实际删除的 blob 数。
    pub blobs_deleted: u64,
    /// 实际删除的 chunk 数。
    pub chunks_deleted: u64,
    /// 到期但因计数复活而跳过的项数。
    pub revived: u64,
    /// 因为级联而新排队的 chunk 数。
    pub chunks_enqueued: u64,
}

impl GcReport {
    pub fn is_empty(&self) -> bool {
        self.blobs_deleted == 0 && self.chunks_deleted == 0 && self.revived == 0
    }
}

/// 跑一轮回收：处理 `now` 之前到期的全部待删项。
///
/// `max_items` 限制单轮处理量，避免一次删除几十万项把后台任务卡住太久。
pub async fn run_once(store: &Store, now: u64, max_items: usize) -> Result<GcReport> {
    let mut report = GcReport::default();

    // 先处理 blob：删 blob 可能级联出新的待删 chunk，放在 chunk 之前能让
    // 同一轮就把级联出来的块也排上队（但不会同一轮删掉，它们有自己的延迟）。
    process_queue(store, GcKind::Blob, now, max_items, &mut report).await?;
    process_queue(store, GcKind::Chunk, now, max_items, &mut report).await?;

    if !report.is_empty() {
        tracing::info!(
            blobs = report.blobs_deleted,
            chunks = report.chunks_deleted,
            revived = report.revived,
            "GC 完成一轮"
        );
    }
    Ok(report)
}

async fn process_queue(
    store: &Store,
    kind: GcKind,
    now: u64,
    max_items: usize,
    report: &mut GcReport,
) -> Result<()> {
    // 队列 key 是 `{tag}|{到期时间大端}|{hash}`，所以字典序等于到期时间序：
    // 从头扫到第一个未到期的项就能停，不用遍历整个队列。
    let rows = store
        .scan_prefix_keys(Cf::Gc, keys::gc_prefix(kind), max_items)
        .await?;

    for key in rows {
        let (_, delete_after, hash) = keys::parse_gc_key(&key)?;
        if delete_after > now {
            // 后面的项到期更晚，直接收工。
            break;
        }

        let (ref_cf, ref_key) = match kind {
            GcKind::Blob => (Cf::BlobMeta, keys::blob_ref_key(hash)),
            GcKind::Chunk => (Cf::ChunkMeta, keys::chunk_ref_key(hash)),
        };

        // ==== 二次确认 ====
        // 这一小时里内容可能因为 agent 重传而复活。
        if store.counter(ref_cf, ref_key.clone()).await? > 0 {
            store.delete(Cf::Gc, key).await?;
            report.revived += 1;
            tracing::debug!(hash = %hash.to_hex8(), "待删内容已被重新引用，跳过");
            continue;
        }

        match kind {
            GcKind::Blob => delete_blob(store, hash, key, now, report).await?,
            GcKind::Chunk => delete_chunk(store, hash, key, report).await?,
        }
    }
    Ok(())
}

/// 删一个 blob：分块的先给每个块减引用，归零的块排进 chunk 队列。
async fn delete_blob(
    store: &Store,
    hash: ContentHash,
    queue_key: Vec<u8>,
    now: u64,
    report: &mut GcReport,
) -> Result<()> {
    let meta_key = keys::blob_meta_key(hash);
    let meta: Option<BlobMeta> = match store.get(Cf::BlobMeta, meta_key.clone()).await? {
        Some(raw) => Some(records::decode(&raw)?),
        None => None,
    };

    let mut batch = Batch::new();

    match &meta {
        Some(m) => match &m.layout {
            BlobLayout::Single { class } => {
                let cf = match class {
                    records::ContentClass::Text => Cf::BlobTxt,
                    records::ContentClass::Binary => Cf::BlobBin,
                };
                batch.delete(cf, keys::content_key(hash));
            }
            BlobLayout::Chunked { chunks } => {
                for ch in chunks {
                    batch.add(Cf::ChunkMeta, keys::chunk_ref_key(*ch), -1);
                    // 读到的是本批次提交前的值，所以「减完归零」等价于「当前 <= 1」。
                    if store
                        .counter(Cf::ChunkMeta, keys::chunk_ref_key(*ch))
                        .await?
                        <= 1
                    {
                        batch.put(
                            Cf::Gc,
                            keys::gc_key(GcKind::Chunk, now + super::blob::GC_DELAY_SECS, *ch),
                            Vec::new(),
                        );
                        report.chunks_enqueued += 1;
                    }
                }
            }
        },
        None => {
            // 元数据已经没了但队列里还有项：可能是重复入队，或上一轮删到一半崩了。
            // 继续往下清理队列项即可，不是错误。
            tracing::debug!(hash = %hash.to_hex8(), "待删 blob 的元数据已不存在");
        }
    }

    batch.delete(Cf::BlobMeta, meta_key);
    batch.delete(Cf::BlobMeta, keys::blob_ref_key(hash));
    batch.delete(Cf::Gc, queue_key);
    store.write(batch).await?;

    report.blobs_deleted += 1;
    Ok(())
}

/// 删一个 chunk：内容值与计数记录一起删。
async fn delete_chunk(
    store: &Store,
    hash: ContentHash,
    queue_key: Vec<u8>,
    report: &mut GcReport,
) -> Result<()> {
    let mut batch = Batch::new();
    batch.delete(Cf::Chunk, keys::content_key(hash));
    batch.delete(Cf::ChunkMeta, keys::chunk_ref_key(hash));
    batch.delete(Cf::Gc, queue_key);
    store.write(batch).await?;

    report.chunks_deleted += 1;
    Ok(())
}

/// 对账结果。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuditReport {
    /// 有值但没有计数记录的 chunk：崩在 WriteBatch 提交之前的残留。
    pub orphan_chunks: Vec<ContentHash>,
    /// blobmeta 指向的内容不存在：**这只应在数据损坏时出现，需要告警。**
    pub dangling_blobs: Vec<ContentHash>,
}

impl AuditReport {
    pub fn is_clean(&self) -> bool {
        self.orphan_chunks.is_empty() && self.dangling_blobs.is_empty()
    }
}

/// 对账：扫 `chunk` 与 `chunkmeta` 的差集，以及 blobmeta 里的悬挂指针。
///
/// 两类问题的严重程度完全不同：
///
/// - **孤儿 chunk 是正常的**，崩溃恢复的预期残留，删掉即可；
/// - **悬挂 blob 是数据损坏**，说明元数据在、内容没了，只能靠备份恢复，必须告警。
pub async fn audit(store: &Store, max_items: usize) -> Result<AuditReport> {
    let mut report = AuditReport::default();

    // 孤儿 chunk：chunk 列族有值，chunkmeta 没有对应计数。
    let chunk_keys = store
        .scan_prefix_keys(Cf::Chunk, Vec::new(), max_items)
        .await?;
    for raw_key in chunk_keys {
        let Ok(hash) = keys::parse_content_key(&raw_key) else {
            tracing::warn!(len = raw_key.len(), "chunk 列族里有长度异常的键");
            continue;
        };
        if store
            .get(Cf::ChunkMeta, keys::chunk_ref_key(hash))
            .await?
            .is_none()
        {
            report.orphan_chunks.push(hash);
        }
    }

    // 悬挂 blobmeta：元数据在，内容值不在。
    let meta_rows = store
        .scan_prefix_limit(Cf::BlobMeta, keys::blob_meta_prefix(), max_items)
        .await?;
    for (key, value) in meta_rows {
        let hash = keys::parse_hash_key(&key)?;
        let meta: BlobMeta = records::decode(&value)?;
        let present = match &meta.layout {
            BlobLayout::Single { class } => {
                let cf = match class {
                    records::ContentClass::Text => Cf::BlobTxt,
                    records::ContentClass::Binary => Cf::BlobBin,
                };
                store.get(cf, keys::content_key(hash)).await?.is_some()
            }
            BlobLayout::Chunked { chunks } => {
                let mut all = true;
                for ch in chunks {
                    if store
                        .get(Cf::Chunk, keys::content_key(*ch))
                        .await?
                        .is_none()
                    {
                        all = false;
                        break;
                    }
                }
                all
            }
        };
        if !present {
            report.dangling_blobs.push(hash);
        }
    }

    if !report.dangling_blobs.is_empty() {
        // 这不是「清理一下就好」的问题，是真丢数据了。
        tracing::error!(
            count = report.dangling_blobs.len(),
            "发现元数据指向不存在的内容，数据可能已损坏，需人工介入并考虑从备份恢复"
        );
    }
    if !report.orphan_chunks.is_empty() {
        tracing::info!(
            count = report.orphan_chunks.len(),
            "发现孤儿 chunk（崩溃恢复的预期残留）"
        );
    }

    Ok(report)
}

/// 删掉对账发现的孤儿 chunk。
pub async fn purge_orphans(store: &Store, orphans: &[ContentHash]) -> Result<u64> {
    if orphans.is_empty() {
        return Ok(0);
    }
    let mut batch = Batch::new();
    for h in orphans {
        batch.delete(Cf::Chunk, keys::content_key(*h));
    }
    store.write(batch).await?;
    tracing::info!(count = orphans.len(), "已清理孤儿 chunk");
    Ok(orphans.len() as u64)
}
