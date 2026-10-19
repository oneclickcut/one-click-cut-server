//! 分块上传：初始化、收分片、finalize、放弃。
//!
//! # 为什么 mp4 必须走这条通道
//!
//! 两个硬理由，缺一个都不行：
//!
//! 1. **Cloudflare 免费版单请求体上限 100MB**，一个成片可能更大；
//! 2. 家宽上行慢，一次几十 MB 的 PUT 断了就得全部重传。
//!
//! 断点续传只是顺带的好处。
//!
//! # part 与 chunk 不是一回事
//!
//! | 概念 | 大小 | 属于 | 作用 |
//! |------|------|------|------|
//! | part | 16MB（可配） | HTTP 层 | 一次请求传多少 |
//! | chunk | 4MB（固定） | 存储层 | Range 读取的粒度 |
//!
//! 一个 16MB 的 part 会被切成 4 个 chunk。两者解耦的意义：改 part 大小
//! （比如 Tunnel 限制变了）不影响已落盘的内容布局。
//!
//! # 为什么 part 必须按 4MB 对齐
//!
//! 每个 part 独立切块并立刻落盘，所以 part 的大小必须是 4MB 的整数倍——
//! 否则第一个 part 的末尾会产生一个短块，而它在文件中间，破坏了
//! 「`offset / 4MB` 直接索引到块」这个 Range 读取的基础。
//! **末尾那个 part 例外**，它本来就该短。

use anyhow::{Result, bail};

use crate::storage::chunk::CHUNK_SIZE;
use crate::storage::keys::{self, CanvasId, ContentHash, Uid, UploadId};
use crate::storage::records::{self, BlobMeta, FileEntry, StageInfo};
use crate::storage::{Batch, Cf, Store, blob};

/// 默认 part 大小：16MB。
///
/// 是 4MB 的 4 倍（满足对齐要求），同时远低于 Cloudflare 免费版的 100MB
/// 单请求体上限，留足了头部与网络抖动的余量。
pub const DEFAULT_PART_SIZE: u32 = 16 << 20;

/// part 大小上限：50MB。
///
/// 接口文档第 7 节的要求：每个 part 控制在 50MB 以内，给 Tunnel 留余量。
pub const MAX_PART_SIZE: u32 = 50 << 20;

/// 上传会话超时：24 小时未 finalize 就清理。
pub const STAGE_TTL_SECS: u64 = 24 * 3600;

/// 单个上传会话的分片数上限。
///
/// 16MB × 4096 = 64GB，远超任何合理的成片。设上限是防止一个坏掉的客户端
/// 无限往 `stage` 列族里堆东西。
pub const MAX_PARTS: u32 = 4096;

/// 一次上传会话的初始化结果。
#[derive(Debug, Clone)]
pub struct InitiatedUpload {
    pub upload_id: UploadId,
    /// 客户端应当按这个大小切分（末尾那个可以短）。
    pub part_size: u32,
    pub expires_at: u64,
}

/// 收下一个分片后的状态。
#[derive(Debug, Clone)]
pub struct PartAccepted {
    pub part_no: u32,
    pub size: u64,
    /// 该 part 被切成了几个 chunk。
    pub chunks: usize,
    /// 这个分片之前已经传过（幂等重传）。
    pub duplicate: bool,
}

/// finalize 的结果。
#[derive(Debug, Clone)]
pub struct CompletedUpload {
    pub path: String,
    pub hash: ContentHash,
    pub size: u64,
    pub replaced: bool,
}

/// 分块上传可能的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadError {
    QuotaExceeded,
    /// 画布不存在或不属于调用者。
    CanvasNotFound,
    /// 上传会话不存在、已 finalize 或已过期。
    SessionNotFound,
    /// 分片号超出上限。
    PartOutOfRange,
    /// 分片大小不符合约定（非末尾分片必须正好 `part_size`）。
    BadPartSize(String),
    /// finalize 时分片不连续（缺号）。
    MissingParts(Vec<u32>),
    /// 一个分片都没传就 finalize。
    NoParts,
    /// 目标路径与一个目录撞车（不变量 1）。
    ///
    /// 在 `initiate` 与 `complete` 两处都要查：前者让客户端在传 90MB 之前就失败，
    /// 后者兜住「上传窗口期内有人把那个路径建成了目录」这个竞态。
    PathConflict(crate::domain::canvas::PathConflict),
}

pub type UploadResult<T> = std::result::Result<T, UploadError>;

/// 初始化一次分块上传。
pub async fn initiate(
    store: &Store,
    uid: Uid,
    canvas_id: CanvasId,
    path: &str,
    part_size: Option<u32>,
    now: u64,
) -> Result<UploadResult<InitiatedUpload>> {
    keys::validate_path(path)?;

    if crate::domain::space::get_canvas(store, uid, canvas_id)
        .await?
        .is_none()
    {
        return Ok(Err(UploadError::CanvasNotFound));
    }

    // 不变量 1 尽早查：让客户端在传 90MB 之前就知道这个路径不能用。
    if let Some(c) = crate::domain::canvas_dir::conflict_for_file(store, canvas_id, path).await? {
        return Ok(Err(UploadError::PathConflict(c)));
    }

    let part_size = part_size.unwrap_or(DEFAULT_PART_SIZE);
    if part_size == 0 || part_size > MAX_PART_SIZE {
        bail!(
            "part_size 必须在 1..={} 之间，实际 {part_size}",
            MAX_PART_SIZE
        );
    }
    // 对齐要求见模块说明：不对齐会在文件中间产生短块，破坏 Range 的块索引。
    if !(part_size as u64).is_multiple_of(CHUNK_SIZE) {
        bail!(
            "part_size 必须是 {} 字节（4MB）的整数倍，实际 {part_size}",
            CHUNK_SIZE
        );
    }

    let upload_id = UploadId::new();
    let info = StageInfo {
        v: 1,
        canvas_id,
        path: path.to_owned(),
        created_at: now,
        part_size,
    };
    store
        .put(
            Cf::Stage,
            keys::stage_info_key(upload_id),
            records::encode(&info)?,
        )
        .await?;

    Ok(Ok(InitiatedUpload {
        upload_id,
        part_size,
        expires_at: now + STAGE_TTL_SECS,
    }))
}

/// 读会话信息。
async fn load_session(store: &Store, upload_id: UploadId, now: u64) -> Result<Option<StageInfo>> {
    let Some(raw) = store
        .get(Cf::Stage, keys::stage_info_key(upload_id))
        .await?
    else {
        return Ok(None);
    };
    let info: StageInfo = records::decode(&raw)?;
    if now.saturating_sub(info.created_at) > STAGE_TTL_SECS {
        // 过期会话留给后台任务清理，这里只是不让它继续用。
        return Ok(None);
    }
    Ok(Some(info))
}

/// 收下一个分片。
///
/// 每个 part 到达就立刻切块落盘，**不在内存里攒整个文件**。`stage` 列族里只记
/// 「第 N 个 part 对应哪些块」，finalize 时按序号顺序拼成完整的块列表。
///
/// 幂等：同一个 part 重复上传不会产生额外的块（内容寻址天然去重），
/// 也不会让块列表出现重复。
pub async fn put_part(
    store: &Store,
    upload_id: UploadId,
    part_no: u32,
    data: &[u8],
    now: u64,
) -> Result<UploadResult<PartAccepted>> {
    let Some(info) = load_session(store, upload_id, now).await? else {
        return Ok(Err(UploadError::SessionNotFound));
    };
    if part_no >= MAX_PARTS {
        return Ok(Err(UploadError::PartOutOfRange));
    }

    // 非末尾分片必须正好 part_size，否则拼出来的文件会在中间缺字节，
    // 而这种损坏只有在播放到那一段时才暴露。
    //
    // 「是不是末尾分片」这件事在收 part 时无法判断（客户端还没说总共几个），
    // 所以规则是：**小于 part_size 的分片只能是最后一个**，由 finalize 校验；
    // 这里只挡住「大于 part_size」。
    let len = data.len() as u64;
    if len > info.part_size as u64 {
        return Ok(Err(UploadError::BadPartSize(format!(
            "分片 {part_no} 有 {len} 字节，超过约定的 {} 字节",
            info.part_size
        ))));
    }
    if len == 0 {
        return Ok(Err(UploadError::BadPartSize(format!(
            "分片 {part_no} 是空的"
        ))));
    }

    let part_key = keys::stage_part_key(upload_id, part_no);
    let duplicate = store.get(Cf::Stage, part_key.clone()).await?.is_some();

    // 切块并写入。已存在的块跳过——同一个 part 重传时全部命中。
    let mut batch = Batch::new();
    let mut hashes = Vec::with_capacity(data.len().div_ceil(CHUNK_SIZE as usize));
    for piece in data.chunks(CHUNK_SIZE as usize) {
        let h = ContentHash::from_bytes(*blake3::hash(piece).as_bytes());
        hashes.push(h);
        if store.get(Cf::Chunk, keys::content_key(h)).await?.is_none() {
            batch.put(Cf::Chunk, keys::content_key(h), piece.to_vec());
        }
    }

    // 分片记录存「块哈希列表 + 这个 part 的字节数」。
    let record = PartRecord {
        v: 1,
        size: len,
        chunks: hashes.clone(),
    };
    batch.put(Cf::Stage, part_key, records::encode(&record)?);
    store.write(batch).await?;

    Ok(Ok(PartAccepted {
        part_no,
        size: len,
        chunks: hashes.len(),
        duplicate,
    }))
}

/// `stage` 列族里一个分片的记录。
///
/// 放在这里而不是 `storage/records.rs`：它是分块上传的实现细节，
/// 上传通道之外没人需要它。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PartRecord {
    #[serde(default = "one")]
    v: u16,
    size: u64,
    chunks: Vec<ContentHash>,
}

fn one() -> u16 {
    1
}

/// 已上传分片的概览，用于断点续传时告诉客户端「还差哪些」。
#[derive(Debug, Clone)]
pub struct UploadStatus {
    pub path: String,
    pub part_size: u32,
    /// 已收到的分片号，升序。
    pub parts: Vec<u32>,
    pub bytes_received: u64,
    pub expires_at: u64,
}

/// 查一次上传会话的进度。
pub async fn status(
    store: &Store,
    upload_id: UploadId,
    now: u64,
) -> Result<UploadResult<UploadStatus>> {
    let Some(info) = load_session(store, upload_id, now).await? else {
        return Ok(Err(UploadError::SessionNotFound));
    };
    let parts = load_parts(store, upload_id).await?;

    Ok(Ok(UploadStatus {
        path: info.path,
        part_size: info.part_size,
        bytes_received: parts.iter().map(|(_, r)| r.size).sum(),
        parts: parts.iter().map(|(n, _)| *n).collect(),
        expires_at: info.created_at + STAGE_TTL_SECS,
    }))
}

/// 按分片号升序读出全部分片记录。
///
/// 分片 key 里的序号是 4 字节大端，所以前缀扫描天然按序号排序——
/// 不需要在内存里再排一次。
async fn load_parts(store: &Store, upload_id: UploadId) -> Result<Vec<(u32, PartRecord)>> {
    let rows = store
        .scan_prefix_limit(Cf::Stage, keys::stage_part_prefix(upload_id), usize::MAX)
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let part_no = keys::parse_stage_part_key(&key)?;
        out.push((part_no, records::decode(&value)?));
    }
    Ok(out)
}

/// 拼装并提交。
pub async fn complete(
    store: &Store,
    uid: Uid,
    upload_id: UploadId,
    now: u64,
) -> Result<UploadResult<CompletedUpload>> {
    let Some(info) = load_session(store, upload_id, now).await? else {
        return Ok(Err(UploadError::SessionNotFound));
    };
    let Some(mut canvas) = crate::domain::space::get_canvas(store, uid, info.canvas_id).await?
    else {
        return Ok(Err(UploadError::CanvasNotFound));
    };

    // 不变量 1 再查一次：会话是 24 小时有效的，期间可能有人把这个路径建成了目录。
    // `initiate` 那次检查只是为了早失败，不能替代这一次。
    if let Some(c) =
        crate::domain::canvas_dir::conflict_for_file(store, info.canvas_id, &info.path).await?
    {
        return Ok(Err(UploadError::PathConflict(c)));
    }

    let parts = load_parts(store, upload_id).await?;
    if parts.is_empty() {
        return Ok(Err(UploadError::NoParts));
    }

    // 分片必须从 0 连续到 N-1。缺号就报出来，让客户端只补那几个。
    let missing: Vec<u32> = (0..parts.len() as u32)
        .filter(|n| !parts.iter().any(|(got, _)| got == n))
        .collect();
    if !missing.is_empty() {
        return Ok(Err(UploadError::MissingParts(missing)));
    }

    // 除最后一个，每个分片都必须正好 part_size——否则拼出来的文件会在中间
    // 缺字节，而且块索引会错位。
    for (n, rec) in &parts[..parts.len() - 1] {
        if rec.size != info.part_size as u64 {
            return Ok(Err(UploadError::BadPartSize(format!(
                "分片 {n} 有 {} 字节，只有最后一个分片可以短于约定的 {} 字节",
                rec.size, info.part_size
            ))));
        }
    }

    // 按序号顺序拼块列表。这一步就是分块上传的全部意义：内容早就落盘了，
    // 这里只是把「它们按什么顺序组成一个文件」写下来。
    let mut chunks = Vec::new();
    let mut total = 0u64;
    for (_, rec) in &parts {
        chunks.extend_from_slice(&rec.chunks);
        total += rec.size;
    }

    // 整文件哈希要从落盘的块算，不能信客户端。逐块读回来算一遍 BLAKE3，
    // 顺便验证了每个块都真的在库里。
    let hash = hash_of_chunks(store, &chunks, total).await?;

    let file_key = keys::file_key(info.canvas_id, &info.path)?;
    let old: Option<FileEntry> = match store.get(Cf::Meta, file_key.clone()).await? {
        Some(raw) => Some(records::decode(&raw)?),
        None => None,
    };

    let delta = total.saturating_sub(old.as_ref().map_or(0, |o| o.size));
    if let crate::domain::quota::QuotaCheck::Exceeded { .. } =
        crate::domain::quota::check(store, uid, delta).await?
    {
        return Ok(Err(UploadError::QuotaExceeded));
    }

    let mut batch = Batch::new();

    // blobmeta 可能已存在（同内容之前传过），这时不必重写。
    if store
        .get(Cf::BlobMeta, keys::blob_meta_key(hash))
        .await?
        .is_none()
    {
        batch.put(
            Cf::BlobMeta,
            keys::blob_meta_key(hash),
            records::encode(&BlobMeta::chunked(total, chunks.clone()))?,
        );
    }

    // 引用计数：新内容 +1（含它的每个块），旧内容 -1。
    batch.add(Cf::BlobMeta, keys::blob_ref_key(hash), 1);
    for ch in &chunks {
        batch.add(Cf::ChunkMeta, keys::chunk_ref_key(*ch), 1);
    }
    if let Some(old) = &old {
        blob::stage_dereference(store, &mut batch, old.hash, now).await?;
    }

    let entry = FileEntry {
        v: 1,
        hash,
        size: total,
        updated_at: now,
    };
    batch.put(Cf::Meta, file_key, records::encode(&entry)?);

    // 画布统计。
    match &old {
        Some(o) => canvas.bytes = canvas.bytes + total - o.size,
        None => {
            canvas.file_count += 1;
            canvas.bytes += total;
        }
    }
    canvas.updated_at = now;
    batch.put(
        Cf::Meta,
        keys::canvas_key(uid, info.canvas_id),
        records::encode(&canvas)?,
    );
    if let Some(mut user) = crate::domain::identity::find_by_uid(store, uid).await? {
        user.bytes_used = user
            .bytes_used
            .saturating_sub(old.as_ref().map_or(0, |o| o.size))
            .saturating_add(total);
        batch.put(Cf::Meta, keys::user_key(uid), records::encode(&user)?);
    }

    // 清 stage：会话与全部分片记录。**必须与提交同批**——否则崩在中间会留下
    // 一个已 finalize 但仍可继续 put_part 的会话。
    batch.delete(Cf::Stage, keys::stage_info_key(upload_id));
    for (n, _) in &parts {
        batch.delete(Cf::Stage, keys::stage_part_key(upload_id, *n));
    }

    store.write(batch).await?;

    tracing::info!(
        canvas = %info.canvas_id,
        path = %info.path,
        parts = parts.len(),
        size = total,
        "分块上传已完成"
    );

    Ok(Ok(CompletedUpload {
        path: info.path,
        hash,
        size: total,
        replaced: old.is_some(),
    }))
}

/// 逐块读回来算整文件 BLAKE3。
///
/// 为什么不信客户端给的哈希：那等于让客户端决定内容寻址的 key，
/// 一个恶意（或有 bug 的）客户端可以让两份不同内容映射到同一个哈希，
/// 后写的那份会静默覆盖前一份的元数据。
async fn hash_of_chunks(store: &Store, chunks: &[ContentHash], total: u64) -> Result<ContentHash> {
    let mut hasher = blake3::Hasher::new();
    let mut seen = 0u64;
    for (i, ch) in chunks.iter().enumerate() {
        let raw = store
            .get(Cf::Chunk, keys::content_key(*ch))
            .await?
            .ok_or_else(|| anyhow::anyhow!("第 {i} 块内容缺失: {}", ch.to_hex8()))?;
        hasher.update(&raw);
        seen += raw.len() as u64;
    }
    if seen != total {
        bail!("块内容合计 {seen} 字节，与分片声明的 {total} 字节不一致");
    }
    Ok(ContentHash::from_bytes(*hasher.finalize().as_bytes()))
}

/// 放弃一次上传。
///
/// 只删 stage 记录，**不删已写入的块**：那些块可能被别的文件引用（内容寻址），
/// 而没人引用的块会被 GC 的孤儿扫描回收。这比在这里判断引用关系简单且更安全。
pub async fn abort(store: &Store, upload_id: UploadId, now: u64) -> Result<UploadResult<u32>> {
    // 过期的会话也允许 abort（幂等清理），所以不走 load_session 的过期判断。
    let exists = store
        .get(Cf::Stage, keys::stage_info_key(upload_id))
        .await?
        .is_some();
    if !exists {
        return Ok(Err(UploadError::SessionNotFound));
    }
    let _ = now;

    let parts = load_parts(store, upload_id).await?;
    let mut batch = Batch::new();
    batch.delete(Cf::Stage, keys::stage_info_key(upload_id));
    for (n, _) in &parts {
        batch.delete(Cf::Stage, keys::stage_part_key(upload_id, *n));
    }
    store.write(batch).await?;

    Ok(Ok(parts.len() as u32))
}

/// 清理超时未 finalize 的会话。S8 的后台任务会定期调用。
///
/// 它们产生的块变成孤儿，由 [`crate::storage::gc::audit`] 回收。
pub async fn purge_expired(store: &Store, now: u64, max_items: usize) -> Result<u64> {
    let rows = store
        .scan_prefix_limit(Cf::Stage, keys::stage_info_prefix(), max_items)
        .await?;

    let mut n = 0u64;
    for (key, value) in rows {
        let info: StageInfo = records::decode(&value)?;
        if now.saturating_sub(info.created_at) <= STAGE_TTL_SECS {
            continue;
        }
        let Ok(upload_id) = keys::parse_stage_info_key(&key) else {
            tracing::warn!(len = key.len(), "上传会话键异常，跳过");
            continue;
        };
        // 会话刚被别的调用清掉了也无所谓，这里只是尽力清理。
        let _ = abort(store, upload_id, now).await?;
        n += 1;
    }
    if n > 0 {
        tracing::info!(count = n, "已清理超时的分块上传会话");
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part大小必须是块大小的整数倍() {
        // 不对齐会在文件中间产生短块，破坏「offset / 4MB 索引到块」这个
        // Range 读取的基础。
        assert!((DEFAULT_PART_SIZE as u64).is_multiple_of(CHUNK_SIZE));
        assert_eq!(DEFAULT_PART_SIZE, 16 << 20);
        assert_eq!(DEFAULT_PART_SIZE as u64 / CHUNK_SIZE, 4, "一个 part 四个块");
    }

    #[test]
    fn part大小上限符合tunnel限制() {
        // 接口文档第 7 节：每个 part 控制在 50MB 以内，给 Cloudflare 免费版的
        // 100MB 单请求体上限留余量。
        const { assert!(MAX_PART_SIZE <= 50 << 20) };
        const { assert!(DEFAULT_PART_SIZE < MAX_PART_SIZE) };
    }

    #[test]
    fn 失败原因彼此可区分() {
        assert_ne!(UploadError::SessionNotFound, UploadError::NoParts);
        assert_ne!(
            UploadError::MissingParts(vec![1]),
            UploadError::MissingParts(vec![2]),
            "缺哪几个分片要能区分，客户端只补那几个"
        );
    }
}
