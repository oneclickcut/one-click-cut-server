//! 画布**内**的目录：列一层、建、删。
//!
//! # 先分清两种「文件夹」
//!
//! | 维度 | 画布外：文件夹 `Folder` | 画布内：目录 `CanvasDir` |
//! |------|------------------------|--------------------------|
//! | 身份标识 | `FolderId`（ULID），改名不变 | **路径字符串**，改名就是换身份 |
//! | 存储 | `d\|{uid}\|{folder_id}` + `ch\|` 子索引 | `fd\|{canvas_id}\|{dir_path}`，无子索引 |
//! | 树的表达 | 父指针 + 子节点索引 | **由 path 里的斜杠隐含** |
//! | 是否影响 URL | 不影响 | **影响**：path 是 URL 的一部分 |
//!
//! 代码与接口里一律区分用词：画布外叫「文件夹 / folder」，画布内叫「目录 / dir」。
//!
//! # 目录树是派生的，目录记录只解决「空目录」
//!
//! 文件的 `path` 仍然是唯一真相，一层的内容靠两次前缀扫描并起来：
//! 文件键 `f|{canvas_id}|{prefix}/` 派生出子目录名，目录键
//! `fd|{canvas_id}|{prefix}/` 补上那些还没有任何文件的空目录。
//!
//! 这个选择的直接好处是**写入路径完全不用改**：`put_file` 不需要创建或校验祖先
//! 目录，也就不存在「目录记录与文件实际分布漂移」的对账问题。老数据零迁移——
//! 没有任何 `fd|` 记录的画布，派生出来的目录树跟前端自己按斜杠分组的结果一样。
//!
//! # 不维护任何目录级计数器
//!
//! 子目录的 `file_count` / `bytes` 每次列表都从前缀扫描现算。单画布文件数上限
//! 5000，一次扫描扛得住；而计数器一定会漂移（覆盖写、批量删、崩溃各是一条
//! 漂移路径）。

use anyhow::Result;
use std::collections::BTreeMap;

use crate::domain::canvas::{self, FileRow, PathConflict};
use crate::storage::keys::{self, CanvasId, Uid};
use crate::storage::records::{self, CanvasDir, FileEntry};
use crate::storage::{Batch, Cf, Store};

/// 单画布目录记录数上限。
///
/// 记录本身只有十几字节，但没有上限就等于对外开放一个「无限写小 key」的接口。
/// 注意这个上限只算**显式记录**：派生出来的目录不占额度，因为它们不落盘。
pub const MAX_DIRS_PER_CANVAS: usize = 500;

/// 一层里的一个子目录。
#[derive(Debug, Clone)]
pub struct DirRow {
    /// 画布内完整路径，不带前后斜杠，例如 `scene01/raw`。
    pub path: String,
    /// 本层显示名（`path` 的最后一段）。
    pub name: String,
    /// **整棵子树**的文件数，不是直接子项。
    pub file_count: u64,
    /// 整棵子树的字节数。
    pub bytes: u64,
    /// 有显式目录记录时才有值；纯派生出来的目录没有创建时间可言。
    pub created_at: Option<u64>,
    pub updated_at: Option<u64>,
}

/// 一层的内容。
#[derive(Debug, Clone, Default)]
pub struct EntryList {
    /// 子目录，按名字字典序。
    pub dirs: Vec<DirRow>,
    /// 本层文件（不含子目录里的），按名字字典序。
    pub files: Vec<FileRow>,
}

/// 删一个目录的影响规模。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeleteDirReport {
    pub deleted_files: u64,
    pub deleted_dirs: u64,
    pub freed_bytes: u64,
}

/// 建目录的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateOutcome {
    /// false 表示这个目录本来就在（幂等命中），没有新写入任何记录。
    pub created: bool,
    /// 实际写入的记录数，含自动补齐的中间层。
    pub dirs_written: u64,
}

/// 目录操作的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirError {
    /// 画布不存在或不属于调用者。
    CanvasNotFound,
    /// 目录不存在（既没有显式记录，也没有任何文件落在它下面）。
    DirNotFound,
    /// 目录非空且没带 `recursive`。
    NotEmpty,
    /// 超出单画布目录数上限。
    TooManyDirs,
    /// 违反不变量 1：同一个路径不能同时是文件和目录。
    PathConflict(PathConflict),
}

pub type DirResult<T> = std::result::Result<T, DirError>;

// ---------------------------------------------------------------------------
// 存在性判断
// ---------------------------------------------------------------------------

/// 这个路径上有没有一个目录（显式记录，或已有文件落在它下面）。
///
/// # 为什么不用再扫一遍 `fd|{path}/`
///
/// [`create_dir`] **总是把缺失的中间层一并写成显式记录**，所以「有 `fd|a/b`
/// 但没有 `fd|a`」这种状态不会出现。少一次前缀扫描，而这个函数在每次
/// 文件写入时都会被调用。
async fn dir_exists(store: &Store, canvas_id: CanvasId, path: &str) -> Result<bool> {
    if explicit_dir_exists(store, canvas_id, path).await? {
        return Ok(true);
    }
    let under = keys::file_prefix_under(canvas_id, path)?;
    Ok(!store.scan_prefix_keys(Cf::Meta, under, 1).await?.is_empty())
}

/// 这个路径上有没有一条显式目录记录。
async fn explicit_dir_exists(store: &Store, canvas_id: CanvasId, path: &str) -> Result<bool> {
    Ok(store
        .get(Cf::Meta, keys::canvas_dir_key(canvas_id, path)?)
        .await?
        .is_some())
}

/// 这个路径上有没有一个文件条目。
async fn file_exists(store: &Store, canvas_id: CanvasId, path: &str) -> Result<bool> {
    Ok(store
        .get(Cf::Meta, keys::file_key(canvas_id, path)?)
        .await?
        .is_some())
}

/// `a/b/c` → `["a", "a/b"]`，不含自己。
fn ancestors(path: &str) -> Vec<&str> {
    path.char_indices()
        .filter(|(_, c)| *c == '/')
        .map(|(i, _)| &path[..i])
        .collect()
}

/// 写一个文件之前检查不变量 1，返回撞车的形态。
///
/// 两条：这个路径不能已经是目录，它的任何祖先段也不能已经是文件。
/// **三条写入通道（PUT / tar 批量 / 分块 complete）都要过这道检查**，
/// 所以它挂在 domain 层而不是某个 handler 里。
pub async fn conflict_for_file(
    store: &Store,
    canvas_id: CanvasId,
    path: &str,
) -> Result<Option<PathConflict>> {
    // 段数超过目录上限的 path 不可能是目录，省掉这次查询。
    if path.split('/').count() <= keys::MAX_DIR_SEGMENTS
        && dir_exists(store, canvas_id, path).await?
    {
        return Ok(Some(PathConflict::TargetIsDir));
    }
    for anc in ancestors(path) {
        if file_exists(store, canvas_id, anc).await? {
            return Ok(Some(PathConflict::AncestorIsFile));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// 列一层
// ---------------------------------------------------------------------------

/// 列出一层的子目录与文件。
///
/// `prefix` 为 `None` 即画布根层；否则是不带前后斜杠的目录路径。
/// 指定的目录不存在时返回 [`DirError::DirNotFound`]——静默返回空列表会让前端
/// 以为那是个空目录，与 `/api/tree` 对不存在父节点的处理一致。
pub async fn list_entries(
    store: &Store,
    canvas_id: CanvasId,
    prefix: Option<&str>,
) -> Result<DirResult<EntryList>> {
    let (file_scan, dir_scan) = match prefix {
        None => (
            keys::file_prefix(canvas_id),
            keys::canvas_dir_prefix(canvas_id),
        ),
        Some(p) => (
            keys::file_prefix_under(canvas_id, p)?,
            keys::canvas_dir_prefix_under(canvas_id, p)?,
        ),
    };

    // 文件侧一次拿到整棵子树：子目录的合计数顺手就算出来了，成本是零。
    let file_rows = store
        .scan_prefix_limit(Cf::Meta, file_scan.clone(), usize::MAX)
        .await?;
    let dir_rows = store
        .scan_prefix_limit(Cf::Meta, dir_scan.clone(), usize::MAX)
        .await?;

    // 存在性：显式记录、子树里的文件、子树里的目录记录，三者有一个就算存在。
    if let Some(p) = prefix
        && file_rows.is_empty()
        && dir_rows.is_empty()
        && !explicit_dir_exists(store, canvas_id, p).await?
    {
        return Ok(Err(DirError::DirNotFound));
    }

    // BTreeMap 的迭代顺序就是名字的字典序，不用再排一次。
    let mut dirs: BTreeMap<String, DirRow> = BTreeMap::new();
    let mut files: Vec<FileRow> = Vec::new();
    let full = |name: &str| match prefix {
        None => name.to_owned(),
        Some(p) => format!("{p}/{name}"),
    };

    for (key, value) in file_rows {
        let Some(rel) = relative(&key, file_scan.len()) else {
            tracing::warn!("文件键不是 UTF-8，跳过");
            continue;
        };
        let entry: FileEntry = records::decode(&value)?;
        match rel.split_once('/') {
            // 有斜杠：这条文件在某个子目录里，只贡献合计数。
            Some((name, _)) => {
                let row = dirs.entry(name.to_owned()).or_insert_with(|| DirRow {
                    path: full(name),
                    name: name.to_owned(),
                    file_count: 0,
                    bytes: 0,
                    created_at: None,
                    updated_at: None,
                });
                row.file_count += 1;
                row.bytes = row.bytes.saturating_add(entry.size);
            }
            // 无斜杠：本层的文件。扫描本来就按字节序，所以这里天然有序。
            None => files.push(FileRow {
                path: full(rel),
                entry,
            }),
        }
    }

    for (key, value) in dir_rows {
        let Some(rel) = relative(&key, dir_scan.len()) else {
            tracing::warn!("目录键不是 UTF-8，跳过");
            continue;
        };
        let (name, is_direct) = match rel.split_once('/') {
            Some((name, _)) => (name, false),
            None => (rel, true),
        };
        let row = dirs.entry(name.to_owned()).or_insert_with(|| DirRow {
            path: full(name),
            name: name.to_owned(),
            file_count: 0,
            bytes: 0,
            created_at: None,
            updated_at: None,
        });
        // 只有直接子目录那一条记录的时间戳属于它自己。
        if is_direct {
            let rec: CanvasDir = records::decode(&value)?;
            row.created_at = Some(rec.created_at);
            row.updated_at = Some(rec.updated_at);
        }
    }

    // 同名既是文件又是目录：只可能来自不变量 1 上线前的老数据。
    // 目录优先并记 warn，不报错——列表接口不该因为一条脏数据整个失败。
    let before = files.len();
    files.retain(|f| {
        let name = f.path.rsplit('/').next().unwrap_or(&f.path);
        !dirs.contains_key(name)
    });
    if files.len() != before {
        tracing::warn!(
            canvas = %canvas_id,
            prefix = prefix.unwrap_or(""),
            dropped = before - files.len(),
            "同一层里有同名的文件与目录，列表按目录优先"
        );
    }

    Ok(Ok(EntryList {
        dirs: dirs.into_values().collect(),
        files,
    }))
}

/// 从一条 key 里取出扫描前缀之后的那一段。
fn relative(key: &[u8], prefix_len: usize) -> Option<&str> {
    key.get(prefix_len..)
        .and_then(|b| std::str::from_utf8(b).ok())
}

// ---------------------------------------------------------------------------
// 建
// ---------------------------------------------------------------------------

/// 建一个目录。
///
/// - 中间层不存在时**自动补齐**（建 `a/b/c` 会连带写 `a`、`a/b`）。目录本来就是
///   派生的，缺中间记录不会导致功能不正常，但补齐让列表结果更符合直觉，
///   也维持了 [`dir_exists`] 那条「显式目录的祖先也是显式的」假设。
/// - 已存在就**幂等返回**，不报错。目录的身份是路径，重复创建没有歧义——
///   这跟 `POST /api/canvases` 不幂等是有意的区别。
pub async fn create_dir(
    store: &Store,
    uid: Uid,
    canvas_id: CanvasId,
    path: &str,
    now: u64,
) -> Result<DirResult<CreateOutcome>> {
    keys::validate_dir_path(path)?;

    if crate::domain::space::get_canvas(store, uid, canvas_id)
        .await?
        .is_none()
    {
        return Ok(Err(DirError::CanvasNotFound));
    }

    // 不变量 1：自己和每个祖先段都不能已经是文件。
    for p in ancestors(path).into_iter().chain([path]) {
        if file_exists(store, canvas_id, p).await? {
            return Ok(Err(DirError::PathConflict(PathConflict::AncestorIsFile)));
        }
    }

    if dir_exists(store, canvas_id, path).await? {
        return Ok(Ok(CreateOutcome {
            created: false,
            dirs_written: 0,
        }));
    }

    // 要写哪几条。已有显式记录的跳过，这样重复调用不会刷掉别人的 created_at。
    let mut todo = Vec::new();
    for p in ancestors(path).into_iter().chain([path]) {
        if !explicit_dir_exists(store, canvas_id, p).await? {
            todo.push(p);
        }
    }

    // 目录数上限。只扫到上限 +1 就够，不必数清一个超限画布到底有多少条。
    let existing = store
        .scan_prefix_keys(
            Cf::Meta,
            keys::canvas_dir_prefix(canvas_id),
            MAX_DIRS_PER_CANVAS + 1,
        )
        .await?
        .len();
    if existing + todo.len() > MAX_DIRS_PER_CANVAS {
        return Ok(Err(DirError::TooManyDirs));
    }

    let mut batch = Batch::new();
    for p in &todo {
        batch.put(
            Cf::Meta,
            keys::canvas_dir_key(canvas_id, p)?,
            records::encode(&CanvasDir::new(now))?,
        );
    }
    store.write(batch).await?;

    Ok(Ok(CreateOutcome {
        created: true,
        dirs_written: todo.len() as u64,
    }))
}

// ---------------------------------------------------------------------------
// 删
// ---------------------------------------------------------------------------

/// 删一个目录。
///
/// 非空且没带 `recursive` → [`DirError::NotEmpty`]，手感与删非空文件夹对齐。
///
/// 级联删除**复用 [`canvas::stage_delete_files`]**：逐个文件减引用、归零的进 GC
/// 队列、同批更新画布统计与用户 `bytes_used`。不另写一套减引用逻辑——那是最容易
/// 漏掉引用计数的地方。该前缀下的全部 `fd|` 记录在同一批里删掉。
pub async fn delete_dir(
    store: &Store,
    uid: Uid,
    canvas_id: CanvasId,
    path: &str,
    recursive: bool,
    now: u64,
) -> Result<DirResult<DeleteDirReport>> {
    keys::validate_dir_path(path)?;

    if crate::domain::space::get_canvas(store, uid, canvas_id)
        .await?
        .is_none()
    {
        return Ok(Err(DirError::CanvasNotFound));
    }

    let self_key = keys::canvas_dir_key(canvas_id, path)?;
    let explicit = store.get(Cf::Meta, self_key.clone()).await?.is_some();
    let file_keys = store
        .scan_prefix_keys(
            Cf::Meta,
            keys::file_prefix_under(canvas_id, path)?,
            usize::MAX,
        )
        .await?;
    let sub_dir_keys = store
        .scan_prefix_keys(
            Cf::Meta,
            keys::canvas_dir_prefix_under(canvas_id, path)?,
            usize::MAX,
        )
        .await?;

    if !explicit && file_keys.is_empty() && sub_dir_keys.is_empty() {
        return Ok(Err(DirError::DirNotFound));
    }
    if !recursive && (!file_keys.is_empty() || !sub_dir_keys.is_empty()) {
        return Ok(Err(DirError::NotEmpty));
    }

    let mut paths = Vec::with_capacity(file_keys.len());
    for key in &file_keys {
        let (_, p) = keys::parse_file_key(key)?;
        paths.push(p);
    }

    let mut batch = Batch::new();
    let outcome =
        match canvas::stage_delete_files(store, &mut batch, uid, canvas_id, &paths, now).await? {
            Ok(o) => o,
            // stage_delete_files 只会因为画布不存在而失败，而那一步在上面已经查过。
            Err(_) => return Ok(Err(DirError::CanvasNotFound)),
        };

    let mut deleted_dirs = 0u64;
    for key in sub_dir_keys {
        batch.delete(Cf::Meta, key);
        deleted_dirs += 1;
    }
    if explicit {
        batch.delete(Cf::Meta, self_key);
        deleted_dirs += 1;
    }

    store.write(batch).await?;

    tracing::info!(
        canvas = %canvas_id,
        dir = %path,
        files = outcome.deleted,
        dirs = deleted_dirs,
        "画布内目录已删除"
    );

    Ok(Ok(DeleteDirReport {
        deleted_files: outcome.deleted,
        deleted_dirs,
        freed_bytes: outcome.freed,
    }))
}

/// 把删掉一个画布全部目录记录的写操作追加进批次。
///
/// 删画布时必须调它，否则这些记录会在画布消失后永久残留，而且**没有任何人会
/// 发现**——它们不参与统计，接口也扫不到。
pub async fn stage_delete_all(
    store: &Store,
    batch: &mut Batch,
    canvas_id: CanvasId,
) -> Result<u64> {
    let keys_ = store
        .scan_prefix_keys(Cf::Meta, keys::canvas_dir_prefix(canvas_id), usize::MAX)
        .await?;
    let n = keys_.len() as u64;
    for key in keys_ {
        batch.delete(Cf::Meta, key);
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 祖先段逐层展开() {
        assert_eq!(ancestors("a"), Vec::<&str>::new(), "根层文件没有祖先目录");
        assert_eq!(ancestors("a/b"), vec!["a"]);
        assert_eq!(ancestors("a/b/c.jpg"), vec!["a", "a/b"]);
    }

    #[test]
    fn 目录段数上限给文件名留了一段() {
        // 15 段目录 + 1 段文件名 = 16 段 path，正好是上限。否则能建出一个
        // 永远放不进文件的目录。
        assert_eq!(keys::MAX_DIR_SEGMENTS + 1, keys::MAX_PATH_SEGMENTS);
    }

    #[test]
    fn 相对段按前缀长度切出() {
        let key = b"fd\0AAA\0scene01/raw";
        assert_eq!(relative(key, 7), Some("scene01/raw"));
        // 非 UTF-8 不该 panic，只该被跳过。
        assert_eq!(relative(&[0xff, 0xfe], 0), None);
    }
}
