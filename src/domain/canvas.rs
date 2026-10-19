//! 画布内文件读写、manifest 生成。
//!
//! # `path` 仍然是文件的唯一真相
//!
//! 画布内的目录树**从文件 path 的斜杠派生**，不是靠目录记录表达的。前缀扫描
//! `f|{canvas_id}|` 天然按路径字典序返回，正好是 manifest 需要的顺序。
//!
//! 画布内另有一层目录实体（[`crate::domain::canvas_dir`]），但它只为「空目录也能
//! 存在」这一件事服务：写文件时**不需要**先创建祖先目录，也不会去校验它们存在。
//! 这里唯一与目录相关的约束是不变量 1（同一个路径不能同时是文件和目录），
//! 见 [`put_file`]。
//!
//! # 写入的原子性
//!
//! 内容先入库（[`crate::storage::blob::put_bytes`]），然后 FileEntry、
//! 新旧 blob 的引用计数、画布统计在**同一个 WriteBatch** 里提交。所以不存在
//! 「元数据指向不存在的内容」的中间态；崩在提交之前只会留下没人引用的
//! blob 或 chunk，由 GC 的孤儿扫描清理。

use anyhow::Result;

use crate::storage::keys::{self, CanvasId, ContentHash, Uid};
use crate::storage::records::{self, Canvas, FileEntry};
use crate::storage::{Batch, Cf, Store, blob};

/// 单个画布的文件数上限。
///
/// 一个典型画布约 350 个文件，5000 给了很大余量。设上限是因为无上限的画布会让
/// manifest 变成一个几 MB 的 JSON，而 manifest 是播放器的入口、走短缓存。
pub const MAX_FILES_PER_CANVAS: u64 = 5000;

/// 一条文件条目连同它的 path。
#[derive(Debug, Clone)]
pub struct FileRow {
    pub path: String,
    pub entry: FileEntry,
}

/// 写入一个文件的结果。
#[derive(Debug, Clone)]
pub struct PutOutcome {
    pub hash: ContentHash,
    pub size: u64,
    /// 是否覆盖了同路径的旧文件。
    pub replaced: bool,
    /// 内容命中去重，没有写入新字节。
    pub deduplicated: bool,
}

/// 文件路径与目录撞车的两种形态。
///
/// 分开两种是为了给出有用的提示：前者用户该改文件名，后者该先把那个文件挪走。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathConflict {
    /// 这个路径已经是一个目录（有显式目录记录，或已有文件落在它下面）。
    TargetIsDir,
    /// 路径的某个祖先段已经是一个文件，例如已有文件 `a` 时要写 `a/b`。
    AncestorIsFile,
}

/// 画布内操作的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasError {
    /// 画布不存在或不属于调用者。
    NotFound,
    /// 文件不存在。
    FileNotFound,
    /// 超出单画布文件数上限。
    TooManyFiles,
    /// 违反不变量 1：同一个路径不能同时是文件和目录。
    PathConflict(PathConflict),
    /// 超出用户配额。带上数字，让调用方给出有用的提示。
    QuotaExceeded { used: u64, limit: u64 },
}

pub type CanvasResult<T> = std::result::Result<T, CanvasError>;

/// 列出画布内全部文件条目，按路径字典序。
pub async fn list_files(store: &Store, canvas_id: CanvasId, limit: usize) -> Result<Vec<FileRow>> {
    let rows = store
        .scan_prefix_limit(Cf::Meta, keys::file_prefix(canvas_id), limit)
        .await?;

    let mut out = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let (_, path) = keys::parse_file_key(&key)?;
        out.push(FileRow {
            path,
            entry: records::decode(&value)?,
        });
    }
    Ok(out)
}

/// 读一条文件条目。
pub async fn get_file(store: &Store, canvas_id: CanvasId, path: &str) -> Result<Option<FileEntry>> {
    if keys::validate_path(path).is_err() {
        // 非法 path 不可能存在，直接当查不到。
        return Ok(None);
    }
    let Some(raw) = store
        .get(Cf::Meta, keys::file_key(canvas_id, path)?)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(records::decode(&raw)?))
}

/// 写入或覆盖一个文件。
///
/// `mime` 只用来决定内容落进 `blob_txt` 还是 `blob_bin`，不落盘——
/// 对外的 `Content-Type` 由扩展名实时推导，这样改了推导规则不用回写历史数据。
pub async fn put_file(
    store: &Store,
    uid: Uid,
    canvas_id: CanvasId,
    path: &str,
    mime: &str,
    data: &[u8],
    now: u64,
) -> Result<CanvasResult<PutOutcome>> {
    let mut batch = Batch::new();
    let result = stage_put_file(store, &mut batch, uid, canvas_id, path, mime, data, now).await?;
    if result.is_ok() {
        store.write(batch).await?;
    }
    Ok(result)
}

/// Stage a single file mutation together with its caller's receipt. Caller holds operation_lock.
pub async fn stage_put_file(
    store: &Store,
    batch: &mut Batch,
    uid: Uid,
    canvas_id: CanvasId,
    path: &str,
    mime: &str,
    data: &[u8],
    now: u64,
) -> Result<CanvasResult<PutOutcome>> {
    keys::validate_path(path)?;

    let Some(mut canvas) = crate::domain::space::get_canvas(store, uid, canvas_id).await? else {
        return Ok(Err(CanvasError::NotFound));
    };

    // 不变量 1：这个路径不能已经是一个目录，祖先段也不能已经是一个文件。
    //
    // 这几次额外的 KV 查询是写入路径上唯一为目录付出的代价。放在这里而不是
    // HTTP 层，是因为三条写入通道（PUT / tar 批量 / 分块 complete）都要守住它，
    // 而 domain 层是它们唯一的共同入口。
    if let Some(conflict) =
        crate::domain::canvas_dir::conflict_for_file(store, canvas_id, path).await?
    {
        return Ok(Err(CanvasError::PathConflict(conflict)));
    }

    let file_key = keys::file_key(canvas_id, path)?;
    let old: Option<FileEntry> = match store.get(Cf::Meta, file_key.clone()).await? {
        Some(raw) => Some(records::decode(&raw)?),
        None => None,
    };

    // 文件数上限只对新增生效：覆盖不增加条目数，不该被上限挡住。
    if old.is_none() && canvas.file_count >= MAX_FILES_PER_CANVAS {
        return Ok(Err(CanvasError::TooManyFiles));
    }

    // 配额检查放在写入内容**之前**：先写完再发现超限，就得回滚一次已落盘的
    // 内容，而回滚在内容寻址下并不安全（那份内容可能已被别人引用）。
    //
    // `delta` 是这次会增加的字节数。覆盖写时是差值，可能为 0 甚至为负——
    // 负的情况直接放行。
    let delta = (data.len() as u64).saturating_sub(old.as_ref().map_or(0, |o| o.size));
    if delta > 0 {
        let check = crate::domain::quota::check(store, uid, delta).await?;
        if let crate::domain::quota::QuotaCheck::Exceeded { used, limit, .. } = check {
            return Ok(Err(CanvasError::QuotaExceeded { used, limit }));
        }
    }

    // 内容先入库。这一步失败不会留下任何元数据，重试即可。
    let written = blob::put_bytes(store, mime, data).await?;

    // 新内容加引用。
    blob::stage_reference(store, batch, written.hash).await?;

    // 旧内容减引用。同哈希覆盖（内容没变）时加减抵消，refcount 不变——
    // 这也是为什么不能因为「哈希相同」就跳过整个流程：updated_at 还是要更新的。
    if let Some(old) = &old {
        blob::stage_dereference(store, batch, old.hash, now).await?;
    }

    let entry = FileEntry {
        v: 1,
        hash: written.hash,
        size: written.size,
        updated_at: now,
    };
    batch.put(Cf::Meta, file_key, records::encode(&entry)?);

    // 画布统计增量维护，不靠扫描。
    match &old {
        Some(o) => {
            canvas.bytes = canvas.bytes + written.size - o.size;
        }
        None => {
            canvas.file_count += 1;
            canvas.bytes += written.size;
        }
    }
    canvas.updated_at = now;
    batch.put(
        Cf::Meta,
        keys::canvas_key(uid, canvas_id),
        records::encode(&canvas)?,
    );

    // 用户的 `bytes_used` 也在同一批次里更新：配额是靠它执行的，
    // 与画布统计分开提交会让两者漂移。
    if let Some(mut user) = crate::domain::identity::find_by_uid(store, uid).await? {
        match &old {
            Some(o) => {
                user.bytes_used = user.bytes_used + written.size - o.size;
            }
            None => user.bytes_used = user.bytes_used.saturating_add(written.size),
        }
        batch.put(Cf::Meta, keys::user_key(uid), records::encode(&user)?);
    }

    Ok(Ok(PutOutcome {
        hash: written.hash,
        size: written.size,
        replaced: old.is_some(),
        deduplicated: written.deduplicated,
    }))
}

/// 删除一批文件。
///
/// 返回实际删除的条数。不存在的 path 被跳过而不是报错：批量删除的调用方
/// 通常在做「清理这些路径」，其中一部分已经不在了不该让整批失败。
pub async fn delete_files(
    store: &Store,
    uid: Uid,
    canvas_id: CanvasId,
    paths: &[String],
    now: u64,
) -> Result<CanvasResult<u64>> {
    let mut batch = Batch::new();
    let outcome = match stage_delete_files(store, &mut batch, uid, canvas_id, paths, now).await? {
        Ok(o) => o,
        Err(e) => return Ok(Err(e)),
    };
    store.write(batch).await?;
    Ok(Ok(outcome.deleted))
}

/// 删一批文件的结果。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeleteFilesOutcome {
    /// 实际删除的条目数。
    pub deleted: u64,
    /// 释放的逻辑字节数。
    pub freed: u64,
}

/// 把删一批文件所需的全部写操作追加进 `batch`，**不提交**。
///
/// 抽出来是因为删目录要在同一个批次里再删一批 `fd|` 记录，而**引用计数的减法
/// 只允许有一份实现**——那是最容易漏掉、且漏了以后只能靠对账发现的地方。
/// 画布统计与用户 `bytes_used` 的更新也一并追加进来，三者必须同批。
pub async fn stage_delete_files(
    store: &Store,
    batch: &mut Batch,
    uid: Uid,
    canvas_id: CanvasId,
    paths: &[String],
    now: u64,
) -> Result<CanvasResult<DeleteFilesOutcome>> {
    let Some(mut canvas) = crate::domain::space::get_canvas(store, uid, canvas_id).await? else {
        return Ok(Err(CanvasError::NotFound));
    };

    let mut deleted = 0u64;
    let mut freed = 0u64;

    for path in paths {
        if keys::validate_path(path).is_err() {
            continue;
        }
        let key = keys::file_key(canvas_id, path)?;
        let Some(raw) = store.get(Cf::Meta, key.clone()).await? else {
            continue;
        };
        let entry: FileEntry = records::decode(&raw)?;

        blob::stage_dereference(store, batch, entry.hash, now).await?;
        batch.delete(Cf::Meta, key);
        deleted += 1;
        freed += entry.size;
    }

    if deleted > 0 {
        canvas.file_count = canvas.file_count.saturating_sub(deleted);
        canvas.bytes = canvas.bytes.saturating_sub(freed);
        canvas.updated_at = now;
        batch.put(
            Cf::Meta,
            keys::canvas_key(uid, canvas_id),
            records::encode(&canvas)?,
        );

        // 同批更新用户已用字节，见 put_file 里的同一处说明。
        if let Some(mut user) = crate::domain::identity::find_by_uid(store, uid).await? {
            user.bytes_used = user.bytes_used.saturating_sub(freed);
            batch.put(Cf::Meta, keys::user_key(uid), records::encode(&user)?);
        }
    }

    Ok(Ok(DeleteFilesOutcome { deleted, freed }))
}

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

/// manifest 里的一个文件条目。
#[derive(Debug, Clone)]
pub struct ManifestFile {
    pub path: String,
    /// 相对 `base_url` 的 URL，带 `?v=` 内容版本。
    pub url: String,
    pub size: u64,
    pub mime: String,
    pub hash: String,
}

/// 画布的公开基址。**全库只有这里拼 `/f/` 路径。**
///
/// 路径里只有 slug，不带所有者信息：URL 会被随手转发出去，放进去的任何
/// 身份标识都等于公开它。slug 的 71 bit 熵本来就是唯一的保护，用户名那一段
/// 从来没提供过安全性。
pub fn base_url(slug: &str) -> String {
    format!("/f/{slug}/")
}

/// 播放器契约。
#[derive(Debug, Clone)]
pub struct Manifest {
    pub canvas_id: String,
    pub name: String,
    pub visibility: &'static str,
    pub base_url: String,
    pub entry: Option<String>,
    pub updated_at: u64,
    pub files: Vec<ManifestFile>,
}

/// 生成 manifest。
///
/// # 为什么每个 URL 都要带 `?v=<hash8>`
///
/// 缓存失效完全靠 URL 版本化，不用 CDN 的 purge API：不保留历史版本，
/// 覆盖后 hash 变化 → `?v=` 变化 → 缓存键变化 → CDN 自然取新内容。
/// 这条设计的前提是**播放器必须从 manifest 拿 URL，不能自己拼路径**。
pub fn build_manifest(canvas_id: CanvasId, canvas: &Canvas, files: &[FileRow]) -> Manifest {
    Manifest {
        canvas_id: canvas_id.to_text(),
        name: canvas.name.clone(),
        visibility: match canvas.visibility {
            records::Visibility::Public => "public",
            records::Visibility::Private => "private",
        },
        base_url: base_url(&canvas.slug),
        entry: canvas.entry.clone(),
        updated_at: canvas.updated_at,
        files: files
            .iter()
            .map(|row| ManifestFile {
                url: format!("{}?v={}", row.path, row.entry.hash.to_hex8()),
                path: row.path.clone(),
                size: row.entry.size,
                mime: mime_for_path(&row.path),
                hash: row.entry.hash.to_hex(),
            })
            .collect(),
    }
}

/// 按扩展名推导 `Content-Type`。
///
/// 实时推导而不是落盘：改了推导规则（比如以后要给 `.srt` 换一个 MIME）
/// 不用回写历史数据。
pub fn mime_for_path(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        // 手工列出项目实际用到的类型，顺序与总览文档第 1 节的清单一致。
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        // srt 的标准类型是 application/x-subrip，但播放器普遍按文本处理。
        "srt" => "text/plain; charset=utf-8",
        "vtt" => "text/vtt; charset=utf-8",
        // 认不出来就交给调用方自己嗅探，不要猜。
        _ => "application/octet-stream",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime推导覆盖项目用到的类型() {
        assert_eq!(mime_for_path("a.jpg"), "image/jpeg");
        assert_eq!(mime_for_path("a.JPEG"), "image/jpeg", "扩展名大小写不敏感");
        assert_eq!(mime_for_path("final.mp4"), "video/mp4");
        assert_eq!(mime_for_path("timeline.json"), "application/json");
        assert_eq!(mime_for_path("sub.srt"), "text/plain; charset=utf-8");
        assert_eq!(mime_for_path("scene01/f001.png"), "image/png");
        // 文本类要带 charset，否则浏览器会按 latin-1 解中文。
        assert!(mime_for_path("a.md").contains("charset=utf-8"));
    }

    #[test]
    fn 认不出的扩展名不瞎猜() {
        for p in ["noext", "a.xyz", "a.", ".hidden"] {
            assert_eq!(
                mime_for_path(p),
                "application/octet-stream",
                "{p} 应回落到 octet-stream"
            );
        }
    }

    #[test]
    fn manifest的url带内容版本() {
        let canvas_id = CanvasId::new();
        let canvas = Canvas {
            v: 1,
            project_id: crate::storage::keys::ProjectId::new(),
            role: crate::storage::records::CanvasRole::Production,
            sort_order: 0,
            name: "夏日海边".into(),
            slug: "k7Qm3xB9pLdR".into(),
            visibility: records::Visibility::Private,
            file_count: 1,
            bytes: 8412,
            created_at: 0,
            updated_at: 100,
            entry: Some("timeline.json".into()),
        };
        let mut hash_bytes = [0u8; 32];
        hash_bytes[..4].copy_from_slice(&[0x1a, 0x2b, 0x3c, 0x4d]);
        let rows = vec![FileRow {
            path: "timeline.json".into(),
            entry: FileEntry {
                v: 1,
                hash: ContentHash::from_bytes(hash_bytes),
                size: 8412,
                updated_at: 100,
            },
        }];

        let m = build_manifest(canvas_id, &canvas, &rows);
        // 基址里只有 slug：没有用户名，也没有 uid。
        assert_eq!(m.base_url, "/f/k7Qm3xB9pLdR/");
        assert_eq!(m.entry.as_deref(), Some("timeline.json"));
        assert_eq!(m.files.len(), 1);
        // `?v=` 是缓存正确性的唯一保证：内容变了 URL 就变，CDN 自然取新的。
        assert_eq!(m.files[0].url, "timeline.json?v=1a2b3c4d");
        assert_eq!(m.files[0].mime, "application/json");
        assert!(m.files[0].hash.starts_with("1a2b3c4d"));
        assert_eq!(m.files[0].hash.len(), 64);
    }

    #[test]
    fn manifest的base_url用slug不用名字() {
        let canvas = Canvas {
            v: 1,
            project_id: crate::storage::keys::ProjectId::new(),
            role: crate::storage::records::CanvasRole::Production,
            sort_order: 0,
            name: "可读的名字".into(),
            slug: "k7Qm3xB9pLdR".into(),
            visibility: records::Visibility::Public,
            file_count: 0,
            bytes: 0,
            created_at: 0,
            updated_at: 0,
            entry: None,
        };
        let m = build_manifest(CanvasId::new(), &canvas, &[]);
        assert!(
            !m.base_url.contains("可读的名字"),
            "URL 里出现名字会让改名导致缓存全失效: {}",
            m.base_url
        );
        assert_eq!(m.visibility, "public");
        assert!(m.files.is_empty());
    }
}
