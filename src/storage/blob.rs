//! 内容寻址读写：BLAKE3 标识、按大小分流、去重、引用计数。
//!
//! # 写入路径
//!
//! 边读流边算整文件哈希，同时按 4MB 切块缓冲：
//!
//! - **不超过 4MB**：内容作为单个 value 写进 `blob_txt` 或 `blob_bin`；
//! - **超过 4MB**：每满一块算块哈希、写进 `chunk` 列族，块哈希追加到有序列表。
//!
//! 最后一个 WriteBatch 原子提交新 blob 的 refcount、chunk 的 refcount、
//! FileEntry、以及旧 blob 的 refcount-1。因为内容与元数据都在 RocksDB 里，
//! **不存在「元数据指向不存在的内容」的中间态**——崩在提交之前只会留下没人引用的
//! blob 或 chunk，由 GC 的孤儿扫描清理。
//!
//! # 读取路径
//!
//! 按区间换算出块列表（[`super::chunk::slices_for_range`]），**逐块一次
//! `spawn_blocking`**。不要在一次调用里读完整个文件，否则大文件 Range 请求会
//! 长时间占住一个阻塞线程；峰值内存也会从「并发数 × 4MB」变成「并发数 × 文件大小」。

use anyhow::{Context, Result, bail};

use super::chunk::{self, ByteRange, CHUNK_SIZE};
use super::keys::{self, ContentHash};
use super::kv::{Batch, Cf, Store};
use super::records::{self, BlobLayout, BlobMeta, ContentClass};

/// GC 延迟：引用归零后等这么久才真正删除。
///
/// 避开正在传输中的请求：URL 已发出、字节还没读完时内容被删会导致播放中断。
pub const GC_DELAY_SECS: u64 = 3600;

/// 一次写入的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    pub hash: ContentHash,
    pub size: u64,
    /// 内容已存在、本次没有写入任何字节。agent 重传相同素材时是常态。
    pub deduplicated: bool,
}

/// 按内容类别决定落到哪个列族。
pub fn class_for_mime(mime: &str) -> ContentClass {
    // 文本收益很高（zstd 常有 3~10 倍），已压缩的图片视频再压是纯 CPU 浪费。
    let m = mime.trim().to_ascii_lowercase();
    if m.starts_with("text/") {
        return ContentClass::Text;
    }
    match m.split(';').next().unwrap_or("").trim() {
        "application/json"
        | "application/xml"
        | "application/javascript"
        | "application/x-subrip"
        | "image/svg+xml" => ContentClass::Text,
        _ => ContentClass::Binary,
    }
}

fn cf_for_class(class: ContentClass) -> Cf {
    match class {
        ContentClass::Text => Cf::BlobTxt,
        ContentClass::Binary => Cf::BlobBin,
    }
}

/// 把一段完整内容写进存储，返回内容哈希。
///
/// 只负责**内容**：不写 FileEntry、不碰画布统计、不加 refcount。调用方拿到
/// [`WriteOutcome`] 后自己组装业务批次（见 [`stage_reference`]）。这样切分是因为
/// 「内容入库」与「谁引用了它」是两件独立的事——去重的前提就是同一份内容可以被
/// 多个 FileEntry 引用。
///
/// 写入是幂等的：同一份内容写多少次，磁盘上只有一份。
pub async fn put_bytes(store: &Store, mime: &str, data: &[u8]) -> Result<WriteOutcome> {
    let size = data.len() as u64;
    let hash = ContentHash::from_bytes(*blake3::hash(data).as_bytes());

    // 去重：元数据已存在就说明内容也在（两者同批提交，不会只有一半）。
    if store
        .get(Cf::BlobMeta, keys::blob_meta_key(hash))
        .await?
        .is_some()
    {
        return Ok(WriteOutcome {
            hash,
            size,
            deduplicated: true,
        });
    }

    let mut batch = Batch::new();

    if chunk::needs_chunking(size) {
        // 分块：每块按自身哈希寻址，已存在的块跳过不写。
        let mut chunks = Vec::with_capacity(chunk::chunk_count(size) as usize);
        for piece in data.chunks(CHUNK_SIZE as usize) {
            let ch = ContentHash::from_bytes(*blake3::hash(piece).as_bytes());
            chunks.push(ch);
            if store.get(Cf::Chunk, keys::content_key(ch)).await?.is_none() {
                batch.put(Cf::Chunk, keys::content_key(ch), piece.to_vec());
            }
        }
        batch.put(
            Cf::BlobMeta,
            keys::blob_meta_key(hash),
            records::encode(&BlobMeta::chunked(size, chunks))?,
        );
    } else {
        let class = class_for_mime(mime);
        batch.put(cf_for_class(class), keys::content_key(hash), data.to_vec());
        batch.put(
            Cf::BlobMeta,
            keys::blob_meta_key(hash),
            records::encode(&BlobMeta::single(size, class))?,
        );
    }

    store.write(batch).await?;

    Ok(WriteOutcome {
        hash,
        size,
        deduplicated: false,
    })
}

/// 读元数据。
pub async fn meta(store: &Store, hash: ContentHash) -> Result<Option<BlobMeta>> {
    let Some(raw) = store.get(Cf::BlobMeta, keys::blob_meta_key(hash)).await? else {
        return Ok(None);
    };
    Ok(Some(records::decode(&raw)?))
}

/// 读完整内容。
///
/// **只用于小文件与测试。** 大文件走 [`read_range`]，否则内存峰值会随文件大小走。
pub async fn read_all(store: &Store, hash: ContentHash) -> Result<Option<Vec<u8>>> {
    let Some(m) = meta(store, hash).await? else {
        return Ok(None);
    };
    if m.size == 0 {
        return Ok(Some(Vec::new()));
    }
    let range = ByteRange::full(m.size)?;
    read_range(store, hash, range).await.map(Some)
}

/// 按字节区间读内容。
///
/// 区间起点越界返回错误，调用方映射成 416；末尾越界按截断处理。
pub async fn read_range(store: &Store, hash: ContentHash, range: ByteRange) -> Result<Vec<u8>> {
    let m = meta(store, hash)
        .await?
        .with_context(|| format!("内容不存在: {}", hash.to_hex8()))?;

    match &m.layout {
        BlobLayout::Single { class } => {
            let raw = store
                .get(cf_for_class(*class), keys::content_key(hash))
                .await?
                .with_context(|| format!("元数据存在但内容缺失（数据损坏）: {}", hash.to_hex8()))?;
            if range.start >= raw.len() as u64 {
                bail!("区间起点 {} 超出文件大小 {}", range.start, raw.len());
            }
            let end = (range.end as usize).min(raw.len() - 1);
            Ok(raw[range.start as usize..=end].to_vec())
        }
        BlobLayout::Chunked { chunks } => {
            let slices = chunk::slices_for_range(m.size, range)?;
            let mut out = Vec::with_capacity(range.len().min(m.size) as usize);
            // 逐块读取，每块一次 spawn_blocking（Store::get 内部已包）。
            // 不要一次把全部块读进来——那就退化成了「整文件载入内存」。
            for s in slices {
                let ch = *chunks
                    .get(s.index as usize)
                    .with_context(|| format!("块列表缺少第 {} 块", s.index))?;
                let raw = store
                    .get(Cf::Chunk, keys::content_key(ch))
                    .await?
                    .with_context(|| format!("块内容缺失（数据损坏）: {}", ch.to_hex8()))?;
                let from = s.offset as usize;
                let to = from + s.len as usize;
                if to > raw.len() {
                    bail!(
                        "块 {} 实际 {} 字节，不足以提供 {}..{}",
                        s.index,
                        raw.len(),
                        from,
                        to
                    );
                }
                out.extend_from_slice(&raw[from..to]);
            }
            Ok(out)
        }
    }
}

/// 往批次里追加「引用 +1」：blob 计数加一，分块的每个块计数也加一。
///
/// 与 FileEntry 的写入放同一个批次，所以 refcount 与引用它的记录**要么都可见、
/// 要么都不可见**。这是不会出现悬挂引用的根据。
pub async fn stage_reference(store: &Store, batch: &mut Batch, hash: ContentHash) -> Result<()> {
    batch.add(Cf::BlobMeta, keys::blob_ref_key(hash), 1);

    // 分块内容的每个块也要计数，否则 GC 会把仍在使用的块当成孤儿删掉。
    if let Some(m) = meta(store, hash).await?
        && let BlobLayout::Chunked { chunks } = &m.layout
    {
        for ch in chunks {
            batch.add(Cf::ChunkMeta, keys::chunk_ref_key(*ch), 1);
        }
    }
    Ok(())
}

/// 往批次里追加「引用 -1」，并在归零时把内容排进 GC 待删队列。
///
/// 归零不立即删：延迟 [`GC_DELAY_SECS`] 避开正在传输中的响应。真正的删除由
/// [`super::gc`] 的后台任务执行，并在删除前二次确认计数仍为 0。
///
/// 返回是否已排入队列（调用方一般只用于日志与测试断言）。
pub async fn stage_dereference(
    store: &Store,
    batch: &mut Batch,
    hash: ContentHash,
    now: u64,
) -> Result<bool> {
    batch.add(Cf::BlobMeta, keys::blob_ref_key(hash), -1);

    let current = store
        .counter(Cf::BlobMeta, keys::blob_ref_key(hash))
        .await?;
    // 读到的是批次提交前的值，所以「减完会归零」等价于「当前 <= 1」。
    if current > 1 {
        return Ok(false);
    }

    let delete_after = now + GC_DELAY_SECS;
    batch.put(
        Cf::Gc,
        keys::gc_key(keys::GcKind::Blob, delete_after, hash),
        Vec::new(),
    );
    Ok(true)
}

/// 当前 Unix 秒。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime分流() {
        // 文本类进 blob_txt（zstd），其余进 blob_bin（不压缩）。
        for m in [
            "text/plain",
            "text/html; charset=utf-8",
            "application/json",
            "application/x-subrip",
            "image/svg+xml",
        ] {
            assert_eq!(class_for_mime(m), ContentClass::Text, "{m} 应判为文本");
        }
        for m in [
            "image/jpeg",
            "image/png",
            "video/mp4",
            "application/octet-stream",
        ] {
            assert_eq!(class_for_mime(m), ContentClass::Binary, "{m} 应判为二进制");
        }
        // 大小写与空白不该影响判断。
        assert_eq!(class_for_mime("  TEXT/Plain  "), ContentClass::Text);
    }
}
