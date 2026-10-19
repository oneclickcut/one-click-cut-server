//! 树结构操作、改名、可见性、路径解析。
//!
//! # 树的表达方式：父指针 + 子索引
//!
//! 不用「全路径当 key」，而是每个节点记 `parent_id`，另加一份
//! `ch|{uid}|{parent_id}|{kind}|{child_id}` 的子节点索引。直接收益是
//! **改名 O(1)**：只改 `meta` 里的 `name`，不动索引（索引用 id 不用名字），
//! 所以改一个上层文件夹的名字不会引发整棵子树重写。
//!
//! # 项目是普通文件夹树的叶子
//!
//! 普通文件夹可包含项目；制作画布和公共区属于项目，不占普通文件夹树的节点。
//!
//! 「叶子」指的是**普通文件夹树的**叶子。画布内部另有一层目录（
//! [`crate::domain::canvas_dir`]），但那是完全不同的东西：它的身份是路径字符串
//! 而不是 ULID，树形由 path 里的斜杠隐含，也不是权限或配额的单位。
//! 用词上一律区分——画布外叫「文件夹 / folder」，画布内叫「目录 / dir」。
//!
//! # 可见性不继承
//!
//! **画布自身的 `visibility` 是唯一判据。** 文件夹的只作为新建子项的默认值。
//! 继承会让判定需要向上遍历到根，而且改上层文件夹会意外翻转一批画布的状态。

use anyhow::{Context, Result};

use crate::storage::keys::{self, CanvasId, ChildKind, FolderId, ParentId, ProjectId, Uid};
use crate::storage::records::{
    self, Canvas, CanvasRole, Folder, Project, ProjectStatus, Visibility,
};
use crate::storage::{Batch, Cf, Store};
use crate::util;

/// slug 生成的最大重试次数。
///
/// 12 字符 base62 约 71 bit 熵，碰撞概率可忽略，但**不能不处理**：
/// 不重试的话碰撞会静默覆盖掉别人的画布索引。
const SLUG_MAX_RETRY: usize = 8;

/// 树上的一个节点。
#[derive(Debug, Clone)]
pub enum Node {
    Folder { id: FolderId, folder: Folder },
    Project { id: ProjectId, project: Project },
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Self::Folder { folder, .. } => &folder.name,
            Self::Project { project, .. } => &project.name,
        }
    }

    pub fn parent_id(&self) -> Option<FolderId> {
        match self {
            Self::Folder { folder, .. } => folder.parent_id,
            Self::Project { project, .. } => project.parent_id,
        }
    }
}

/// 操作树时可能出现的、需要区分对待的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceError {
    Inactive,
    TooManyCanvases,
    /// 节点不存在，或不属于这个用户。**两者不区分**——区分会泄露
    /// 「这个 id 确实存在，只是不属于你」。
    NotFound,
    /// 目标父节点是画布。画布是叶子，不能放子节点。
    ParentIsCanvas,
    /// 移动会形成环（把一个文件夹移进自己的子孙里）。
    WouldCycle,
    /// 文件夹非空且没带 `recursive`。
    NotEmpty,
}

pub type SpaceResult<T> = std::result::Result<T, SpaceError>;

// ---------------------------------------------------------------------------
// 读
// ---------------------------------------------------------------------------

/// 读一个文件夹。
pub async fn get_folder(store: &Store, uid: Uid, id: FolderId) -> Result<Option<Folder>> {
    let Some(raw) = store.get(Cf::Meta, keys::folder_key(uid, id)).await? else {
        return Ok(None);
    };
    Ok(Some(records::decode(&raw)?))
}

/// 读一个画布。
pub async fn get_canvas(store: &Store, uid: Uid, id: CanvasId) -> Result<Option<Canvas>> {
    let Some(raw) = store.get(Cf::Meta, keys::canvas_key(uid, id)).await? else {
        return Ok(None);
    };
    Ok(Some(records::decode(&raw)?))
}

/// 按 slug 定位画布。静态分发路径用。
///
/// slug 全局唯一，所以一次 get 就够——uid 与 canvas_id 都在 value 里。
pub async fn resolve_slug(store: &Store, slug: &str) -> Result<Option<(Uid, CanvasId)>> {
    if keys::validate_slug(slug).is_err() {
        // 非法输入不可能存在，直接当查不到。
        return Ok(None);
    }
    let Some(raw) = store.get(Cf::Idx, keys::canvas_slug_key(slug)?).await? else {
        return Ok(None);
    };
    let target: records::SlugTarget = records::decode(&raw).context("slug 索引解码失败")?;
    Ok(Some((target.uid, target.canvas_id)))
}

/// 列出某一层的子节点。
///
/// 一次前缀扫描拿到全部子节点 id（索引的 value 是空的，信息全在 key 里），
/// 然后逐个回查 `meta` 拿名字与属性。
pub async fn list_children(
    store: &Store,
    uid: Uid,
    parent: ParentId,
    limit: usize,
) -> Result<Vec<Node>> {
    let index_keys = store
        .scan_prefix_keys(Cf::Idx, keys::child_prefix(uid, parent), limit)
        .await?;

    let mut out = Vec::with_capacity(index_keys.len());
    for key in index_keys {
        let (kind, id_text) = keys::parse_child_key(&key)?;
        match kind {
            ChildKind::Folder => {
                let id = FolderId::parse(&id_text)?;
                // 索引存在但记录不存在只可能来自 bug（两者同批提交）。
                // 记 warn 并跳过，不让一条坏索引把整个列表打挂。
                match get_folder(store, uid, id).await? {
                    Some(folder) => out.push(Node::Folder { id, folder }),
                    None => tracing::warn!(%uid, %id, "子节点索引指向不存在的文件夹"),
                }
            }
            ChildKind::Project => {
                let id = ProjectId::parse(&id_text)?;
                match get_project(store, uid, id).await? {
                    Some(project) => out.push(Node::Project { id, project }),
                    None => tracing::warn!(%uid, %id, "子节点索引指向不存在的项目"),
                }
            }
        }
    }
    Ok(out)
}

/// 校验一个父节点可以接收子节点。
async fn check_parent(store: &Store, uid: Uid, parent: ParentId) -> Result<SpaceResult<()>> {
    match parent {
        ParentId::Root => Ok(Ok(())),
        ParentId::Folder(id) => {
            if get_folder(store, uid, id).await?.is_none() {
                // 有可能调用方传的其实是一个画布 id：那要报「画布是叶子」
                // 而不是「不存在」，否则前端会以为 id 错了。
                if get_canvas(store, uid, CanvasId::parse(&id.to_text())?)
                    .await?
                    .is_some()
                {
                    return Ok(Err(SpaceError::ParentIsCanvas));
                }
                return Ok(Err(SpaceError::NotFound));
            }
            Ok(Ok(()))
        }
    }
}

// ---------------------------------------------------------------------------
// 建
// ---------------------------------------------------------------------------

/// 建文件夹。
pub async fn create_folder(
    store: &Store,
    uid: Uid,
    parent: ParentId,
    name: &str,
    now: u64,
) -> Result<SpaceResult<(FolderId, Folder)>> {
    keys::validate_display_name(name)?;
    if let Err(e) = check_parent(store, uid, parent).await? {
        return Ok(Err(e));
    }

    // 新建子项的可见性默认跟随父文件夹——这是文件夹 visibility 唯一的作用。
    let visibility = default_visibility(store, uid, parent).await?;

    let id = FolderId::new();
    let folder = Folder {
        v: 1,
        parent_id: match parent {
            ParentId::Root => None,
            ParentId::Folder(p) => Some(p),
        },
        name: name.to_owned(),
        visibility,
        created_at: now,
        updated_at: now,
    };

    let mut batch = Batch::new();
    batch
        .put(
            Cf::Meta,
            keys::folder_key(uid, id),
            records::encode(&folder)?,
        )
        .put(
            Cf::Idx,
            keys::child_key(uid, parent, ChildKind::Folder, &id.to_text())?,
            Vec::new(),
        );
    store.write(batch).await?;

    Ok(Ok((id, folder)))
}

/// 创建项目、公共资料区和默认制作画布，三条记录同批提交。
pub async fn create_project(
    store: &Store,
    uid: Uid,
    parent: ParentId,
    name: &str,
    request_key: Option<&str>,
    now: u64,
) -> Result<SpaceResult<(ProjectId, Project, CanvasId, Canvas)>> {
    keys::validate_display_name(name)?;
    if let Err(e) = check_parent(store, uid, parent).await? {
        return Ok(Err(e));
    }
    let visibility = default_visibility(store, uid, parent).await?;
    let project_id = ProjectId::new();
    let resources_id = CanvasId::new();
    let default_id = CanvasId::new();
    let resources_slug = allocate_slug(store).await?;
    let mut default_slug = allocate_slug(store).await?;
    while default_slug == resources_slug {
        default_slug = allocate_slug(store).await?;
    }
    let project = Project {
        v: 1,
        name: name.to_owned(),
        parent_id: match parent {
            ParentId::Root => None,
            ParentId::Folder(id) => Some(id),
        },
        status: ProjectStatus::Active,
        resources_canvas_id: resources_id,
        created_at: now,
        updated_at: now,
    };
    let resources = Canvas {
        v: 1,
        project_id,
        role: CanvasRole::Resources,
        sort_order: 0,
        name: "约束文档".to_owned(),
        slug: resources_slug,
        visibility,
        file_count: 0,
        bytes: 0,
        created_at: now,
        updated_at: now,
        entry: None,
    };
    let canvas = Canvas {
        v: 1,
        project_id,
        role: CanvasRole::Production,
        sort_order: 0,
        name: "第01集".to_owned(),
        slug: default_slug,
        visibility,
        file_count: 0,
        bytes: 0,
        created_at: now,
        updated_at: now,
        entry: None,
    };
    let mut batch = Batch::new();
    if let Some(request_key) = request_key {
        batch.put(
            Cf::Idx,
            keys::project_request_key(uid, request_key),
            project_id.to_text().into_bytes(),
        );
    }
    batch
        .put(
            Cf::Meta,
            keys::project_key(uid, project_id),
            records::encode(&project)?,
        )
        .put(
            Cf::Idx,
            keys::child_key(uid, parent, ChildKind::Project, &project_id.to_text())?,
            Vec::new(),
        );
    for (id, c) in [(resources_id, &resources), (default_id, &canvas)] {
        batch
            .put(Cf::Meta, keys::canvas_key(uid, id), records::encode(c)?)
            .put(
                Cf::Idx,
                keys::project_member_key(project_id, id),
                Vec::new(),
            )
            .put(
                Cf::Idx,
                keys::canvas_slug_key(&c.slug)?,
                records::encode(&records::SlugTarget::new(uid, id))?,
            );
    }
    store.write(batch).await?;
    Ok(Ok((project_id, project, default_id, canvas)))
}

/// 项目内创建制作画布。公共区只能由项目初始化创建。
pub async fn create_canvas(
    store: &Store,
    uid: Uid,
    project_id: ProjectId,
    name: &str,
    request_key: Option<&str>,
    now: u64,
) -> Result<SpaceResult<(CanvasId, Canvas)>> {
    keys::validate_display_name(name)?;
    let Some(project) = get_project(store, uid, project_id).await? else {
        return Ok(Err(SpaceError::NotFound));
    };
    if project.status != ProjectStatus::Active {
        return Ok(Err(SpaceError::Inactive));
    }
    if list_project_canvases(store, uid, project_id)
        .await?
        .into_iter()
        .filter(|(_, c)| c.role == CanvasRole::Production)
        .count()
        >= 100
    {
        return Ok(Err(SpaceError::TooManyCanvases));
    }
    let id = CanvasId::new();
    let canvas = Canvas {
        v: 1,
        project_id,
        role: CanvasRole::Production,
        sort_order: now as i64,
        name: name.to_owned(),
        slug: allocate_slug(store).await?,
        visibility: default_visibility(store, uid, ParentId::from(project.parent_id)).await?,
        file_count: 0,
        bytes: 0,
        created_at: now,
        updated_at: now,
        entry: None,
    };
    let mut batch = Batch::new();
    if let Some(request_key) = request_key {
        batch.put(
            Cf::Idx,
            keys::canvas_request_key(project_id, request_key),
            id.to_text().into_bytes(),
        );
    }
    batch
        .put(
            Cf::Meta,
            keys::canvas_key(uid, id),
            records::encode(&canvas)?,
        )
        .put(
            Cf::Idx,
            keys::project_member_key(project_id, id),
            Vec::new(),
        )
        .put(
            Cf::Idx,
            keys::canvas_slug_key(&canvas.slug)?,
            records::encode(&records::SlugTarget::new(uid, id))?,
        );
    store.write(batch).await?;
    Ok(Ok((id, canvas)))
}

pub async fn get_project(store: &Store, uid: Uid, id: ProjectId) -> Result<Option<Project>> {
    match store.get(Cf::Meta, keys::project_key(uid, id)).await? {
        Some(raw) => Ok(Some(records::decode(&raw)?)),
        None => Ok(None),
    }
}

pub async fn list_project_canvases(
    store: &Store,
    uid: Uid,
    project_id: ProjectId,
) -> Result<Vec<(CanvasId, Canvas)>> {
    let mut out = Vec::new();
    for key in store
        .scan_prefix_keys(Cf::Idx, keys::project_member_prefix(project_id), usize::MAX)
        .await?
    {
        let id = keys::parse_project_member_key(&key)?;
        if let Some(canvas) = get_canvas(store, uid, id).await? {
            if canvas.project_id != project_id {
                anyhow::bail!("项目成员索引与画布归属不一致");
            }
            out.push((id, canvas));
        }
    }
    out.sort_by_key(|(id, c)| (c.sort_order, *id));
    Ok(out)
}

/// 找一个没被占用的 slug。
///
/// slug 是**全局**唯一的（键里不含所有者），所以这里的碰撞检查是跨用户的。
/// 碰撞概率可忽略，但不检查会让碰撞静默覆盖掉别人的画布索引——
/// 那个画布的 URL 会突然指向另一个画布，而没有任何报错。
async fn allocate_slug(store: &Store) -> Result<String> {
    for attempt in 0..SLUG_MAX_RETRY {
        let slug = util::random_base62(keys::SLUG_LEN);
        if store
            .get(Cf::Idx, keys::canvas_slug_key(&slug)?)
            .await?
            .is_none()
        {
            return Ok(slug);
        }
        tracing::warn!(attempt, "slug 碰撞，重试");
    }
    anyhow::bail!("连续 {SLUG_MAX_RETRY} 次 slug 碰撞，随机源可能有问题");
}

/// 新建子项的默认可见性：跟随父文件夹，根层默认 private。
async fn default_visibility(store: &Store, uid: Uid, parent: ParentId) -> Result<Visibility> {
    match parent {
        ParentId::Root => Ok(Visibility::Private),
        ParentId::Folder(id) => Ok(get_folder(store, uid, id)
            .await?
            .map(|f| f.visibility)
            .unwrap_or(Visibility::Private)),
    }
}

// ---------------------------------------------------------------------------
// 改
// ---------------------------------------------------------------------------

/// 改文件夹的名字与可见性。
///
/// 不级联时只修改本文件夹；级联 private 还会按共享策略在同批下架作品。
pub async fn update_folder(
    store: &Store,
    actor: Uid,
    uid: Uid,
    id: FolderId,
    name: Option<&str>,
    visibility: Option<Visibility>,
    apply_to_children: bool,
    now: u64,
) -> Result<SpaceResult<Folder>> {
    let Some(mut folder) = get_folder(store, uid, id).await? else {
        return Ok(Err(SpaceError::NotFound));
    };

    if let Some(n) = name {
        keys::validate_display_name(n)?;
        folder.name = n.to_owned();
    }
    if let Some(v) = visibility {
        folder.visibility = v;
    }
    folder.updated_at = now;

    let mut batch = Batch::new();
    batch.put(
        Cf::Meta,
        keys::folder_key(uid, id),
        records::encode(&folder)?,
    );

    // 可见性不继承，所以要下推给子项必须显式说。这是一次真实的批量写，
    // 不是判定时的向上遍历——判定永远只看画布自己那一个字段。
    if apply_to_children && let Some(v) = visibility {
        push_visibility(store, &mut batch, actor, uid, ParentId::Folder(id), v, now).await?;
    }

    store.write(batch).await?;
    Ok(Ok(folder))
}

/// 递归把可见性写进子树。
async fn push_visibility(
    store: &Store,
    batch: &mut Batch,
    actor: Uid,
    uid: Uid,
    parent: ParentId,
    visibility: Visibility,
    now: u64,
) -> Result<()> {
    // 用显式栈而不是递归 async fn：后者需要 Box::pin，读起来更绕。
    let mut stack = vec![parent];
    while let Some(p) = stack.pop() {
        for node in list_children(store, uid, p, usize::MAX).await? {
            match node {
                Node::Folder { id, mut folder } => {
                    folder.visibility = visibility;
                    folder.updated_at = now;
                    batch.put(
                        Cf::Meta,
                        keys::folder_key(uid, id),
                        records::encode(&folder)?,
                    );
                    stack.push(ParentId::Folder(id));
                }
                Node::Project { id, .. } => {
                    for (canvas_id, mut canvas) in list_project_canvases(store, uid, id).await? {
                        if visibility == Visibility::Private {
                            super::gallery_policy::stage_private(
                                store, batch, actor, uid, canvas_id, &canvas, now,
                            )
                            .await?;
                        }
                        canvas.visibility = visibility;
                        canvas.updated_at = now;
                        batch.put(
                            Cf::Meta,
                            keys::canvas_key(uid, canvas_id),
                            records::encode(&canvas)?,
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// 改画布的名字、可见性与 entry。
///
/// **改名不影响 URL**：静态路径用的是 slug，这里碰不到它。
pub async fn update_canvas(
    store: &Store,
    actor: Uid,
    uid: Uid,
    id: CanvasId,
    name: Option<&str>,
    visibility: Option<Visibility>,
    entry: Option<&str>,
    sort_order: Option<i64>,
    now: u64,
) -> Result<SpaceResult<Canvas>> {
    let Some(mut canvas) = get_canvas(store, uid, id).await? else {
        return Ok(Err(SpaceError::NotFound));
    };
    if canvas.role != CanvasRole::Production {
        return Ok(Err(SpaceError::NotFound));
    }
    if get_project(store, uid, canvas.project_id)
        .await?
        .map(|p| p.status)
        != Some(ProjectStatus::Active)
    {
        return Ok(Err(SpaceError::Inactive));
    }

    let mut batch = Batch::new();
    if let Some(n) = name {
        keys::validate_display_name(n)?;
        canvas.name = n.to_owned();
    }
    if let Some(v) = visibility {
        if v == Visibility::Private {
            super::gallery_policy::stage_private(store, &mut batch, actor, uid, id, &canvas, now)
                .await?;
        }
        canvas.visibility = v;
    }
    if let Some(e) = entry {
        keys::validate_path(e)?;
        canvas.entry = Some(e.to_owned());
    }
    if let Some(order) = sort_order {
        canvas.sort_order = order;
    }
    canvas.updated_at = now;

    batch.put(
        Cf::Meta,
        keys::canvas_key(uid, id),
        records::encode(&canvas)?,
    );
    store.write(batch).await?;
    Ok(Ok(canvas))
}

// ---------------------------------------------------------------------------
// 移动
// ---------------------------------------------------------------------------

/// 移动文件夹。
pub async fn move_folder(
    store: &Store,
    uid: Uid,
    id: FolderId,
    new_parent: ParentId,
    now: u64,
) -> Result<SpaceResult<()>> {
    let Some(mut folder) = get_folder(store, uid, id).await? else {
        return Ok(Err(SpaceError::NotFound));
    };
    if let Err(e) = check_parent(store, uid, new_parent).await? {
        return Ok(Err(e));
    }

    // 不能移进自己，也不能移进自己的子孙——那样整棵子树会从树上脱落，
    // 变成一圈互相指向的孤岛，列目录永远看不到它们。
    if new_parent == ParentId::Folder(id) {
        return Ok(Err(SpaceError::WouldCycle));
    }
    if let ParentId::Folder(target) = new_parent
        && is_descendant(store, uid, target, id).await?
    {
        return Ok(Err(SpaceError::WouldCycle));
    }

    let old_parent = ParentId::from(folder.parent_id);
    folder.parent_id = match new_parent {
        ParentId::Root => None,
        ParentId::Folder(p) => Some(p),
    };
    folder.updated_at = now;

    // 删旧索引、写新索引、更新记录——必须同批，否则中间态会让节点
    // 同时出现在两层，或者一层都不出现。
    let mut batch = Batch::new();
    batch
        .delete(
            Cf::Idx,
            keys::child_key(uid, old_parent, ChildKind::Folder, &id.to_text())?,
        )
        .put(
            Cf::Idx,
            keys::child_key(uid, new_parent, ChildKind::Folder, &id.to_text())?,
            Vec::new(),
        )
        .put(
            Cf::Meta,
            keys::folder_key(uid, id),
            records::encode(&folder)?,
        );
    store.write(batch).await?;

    Ok(Ok(()))
}

// ---------------------------------------------------------------------------
// 删
// ---------------------------------------------------------------------------

/// 一次删除影响到的规模，用于日志与响应。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeleteReport {
    pub folders: u64,
    pub canvases: u64,
    pub files: u64,
    /// 连带清理掉的画布内目录记录数。
    pub dirs: u64,
    /// 因为引用归零而排进 GC 待删队列的 blob 数。
    pub blobs_queued: u64,
    pub deleted_bytes: u64,
}

/// 删画布及其全部文件条目。
///
/// 每个文件条目引用的 blob 都要减一次引用；归零的排进 GC 队列（延迟一小时，
/// 避开正在传输中的响应）。
pub async fn delete_canvas(
    store: &Store,
    uid: Uid,
    id: CanvasId,
    now: u64,
) -> Result<SpaceResult<DeleteReport>> {
    let Some(canvas) = get_canvas(store, uid, id).await? else {
        return Ok(Err(SpaceError::NotFound));
    };

    if canvas.role != CanvasRole::Production {
        return Ok(Err(SpaceError::NotFound));
    }
    if get_project(store, uid, canvas.project_id)
        .await?
        .map(|p| p.status)
        != Some(ProjectStatus::Active)
    {
        return Ok(Err(SpaceError::Inactive));
    }
    let mut batch = Batch::new();
    let mut report = DeleteReport {
        canvases: 1,
        ..Default::default()
    };
    stage_canvas_delete(store, &mut batch, uid, id, &canvas, now, &mut report).await?;
    stage_user_usage(store, &mut batch, uid, report.deleted_bytes).await?;
    store.write(batch).await?;

    tracing::info!(%uid, canvas = %id, files = report.files, "画布已删除");
    Ok(Ok(report))
}

/// 把删一个画布所需的全部写操作追加进批次。
///
/// 抽出来是因为级联删文件夹要对每个画布做同样的事，而**整个级联必须是一个批次**——
/// 删一半的树比不删更糟。
async fn stage_canvas_delete(
    store: &Store,
    batch: &mut Batch,
    uid: Uid,
    id: CanvasId,
    canvas: &Canvas,
    now: u64,
    report: &mut DeleteReport,
) -> Result<()> {
    batch.delete(Cf::Meta, keys::gallery_ready_key(canvas.project_id, id));
    // 文件条目：逐条减引用。
    let rows = store
        .scan_prefix_limit(Cf::Meta, keys::file_prefix(id), usize::MAX)
        .await?;
    for (key, value) in rows {
        let entry: records::FileEntry = records::decode(&value)?;
        if crate::storage::blob::stage_dereference(store, batch, entry.hash, now).await? {
            report.blobs_queued += 1;
        }
        batch.delete(Cf::Meta, key);
        report.files += 1;
    }

    // 画布内的目录记录。**必须删**：它们不参与任何统计、也没有接口能扫到，
    // 漏删就会在画布消失后永久残留，而且没有人会发现。
    report.dirs += crate::domain::canvas_dir::stage_delete_all(store, batch, id).await?;
    report.deleted_bytes = report.deleted_bytes.saturating_add(canvas.bytes);

    for (key, value) in store
        .scan_prefix(Cf::Stage, keys::stage_info_prefix())
        .await?
    {
        let info: records::StageInfo = records::decode(&value)?;
        if info.canvas_id != id {
            continue;
        }
        let upload_id = keys::parse_stage_info_key(&key)?;
        batch.delete(Cf::Stage, key);
        for part in store
            .scan_prefix_keys(Cf::Stage, keys::stage_part_prefix(upload_id), usize::MAX)
            .await?
        {
            batch.delete(Cf::Stage, part);
        }
    }

    batch
        .delete(Cf::Meta, keys::canvas_key(uid, id))
        .delete(Cf::Idx, keys::canvas_slug_key(&canvas.slug)?)
        .delete(Cf::Idx, keys::project_member_key(canvas.project_id, id));
    Ok(())
}

pub async fn delete_project(
    store: &Store,
    uid: Uid,
    id: ProjectId,
    now: u64,
) -> Result<SpaceResult<DeleteReport>> {
    let Some(mut project) = get_project(store, uid, id).await? else {
        return Ok(Err(SpaceError::NotFound));
    };
    // 先持久化不可写状态并吊销凭证。若后续清理失败，下一次请求从剩余索引继续。
    let mut batch = Batch::new();
    crate::domain::gallery::stage_remove(store, &mut batch, id).await?;
    if project.status != ProjectStatus::Deleting {
        project.status = ProjectStatus::Deleting;
        project.updated_at = now;
        batch.put(
            Cf::Meta,
            keys::project_key(uid, id),
            records::encode(&project)?,
        );
        for key in store
            .scan_prefix_keys(
                Cf::Auth,
                keys::agent_token_by_project_prefix(id),
                usize::MAX,
            )
            .await?
        {
            if let Ok(hash) = keys::parse_agent_token_index_key(&key) {
                batch.delete(Cf::Auth, keys::agent_token_key(hash));
            }
            batch.delete(Cf::Auth, key);
        }
    }
    store.write(batch).await?;
    super::gallery_policy::cleanup_project(store, id).await?;
    let mut report = DeleteReport::default();
    for (canvas_id, canvas) in list_project_canvases(store, uid, id).await? {
        let mut batch = Batch::new();
        let mut one = DeleteReport::default();
        stage_canvas_delete(store, &mut batch, uid, canvas_id, &canvas, now, &mut one).await?;
        stage_user_usage(store, &mut batch, uid, one.deleted_bytes).await?;
        store.write(batch).await?;
        report.canvases += 1;
        report.files += one.files;
        report.dirs += one.dirs;
        report.blobs_queued += one.blobs_queued;
        report.deleted_bytes += one.deleted_bytes;
    }
    let mut batch = Batch::new();
    for key in store
        .scan_prefix_keys(Cf::Idx, keys::canvas_request_prefix(id), usize::MAX)
        .await?
    {
        batch.delete(Cf::Idx, key);
    }
    for (key, value) in store
        .scan_prefix(Cf::Idx, keys::project_request_prefix(uid))
        .await?
    {
        if value == id.to_text().as_bytes() {
            batch.delete(Cf::Idx, key);
        }
    }
    batch.delete(Cf::Meta, keys::project_key(uid, id)).delete(
        Cf::Idx,
        keys::child_key(
            uid,
            ParentId::from(project.parent_id),
            ChildKind::Project,
            &id.to_text(),
        )?,
    );
    store.write(batch).await?;
    Ok(Ok(report))
}

/// 删文件夹。
///
/// 非空时必须带 `recursive`，否则返回 [`SpaceError::NotEmpty`]——
/// 让调用方映射成 409。这是为了不让一次误点删掉一整棵子树。
pub async fn delete_folder(
    store: &Store,
    uid: Uid,
    id: FolderId,
    recursive: bool,
    now: u64,
) -> Result<SpaceResult<DeleteReport>> {
    let Some(folder) = get_folder(store, uid, id).await? else {
        return Ok(Err(SpaceError::NotFound));
    };

    let children = list_children(store, uid, ParentId::Folder(id), 1).await?;
    if !children.is_empty() && !recursive {
        return Ok(Err(SpaceError::NotEmpty));
    }

    // 先逐项目清理；各项目有持久化 deleting 状态和独立的批次。
    // 中途失败时文件夹仍在，重试会继续处理剩余项目。
    let mut report = DeleteReport::default();
    if recursive {
        let mut pending = vec![id];
        while let Some(folder_id) = pending.pop() {
            for node in list_children(store, uid, ParentId::Folder(folder_id), usize::MAX).await? {
                match node {
                    Node::Folder { id, .. } => pending.push(id),
                    Node::Project { id, .. } => {
                        if let Ok(one) = delete_project(store, uid, id, now).await? {
                            report.canvases += one.canvases;
                            report.files += one.files;
                            report.dirs += one.dirs;
                            report.blobs_queued += one.blobs_queued;
                            report.deleted_bytes += one.deleted_bytes;
                        }
                    }
                }
            }
        }
    }
    let mut batch = Batch::new();
    // 显式栈做深度优先，整棵子树进同一个批次。
    let mut stack = vec![(id, folder)];
    while let Some((folder_id, folder)) = stack.pop() {
        for node in list_children(store, uid, ParentId::Folder(folder_id), usize::MAX).await? {
            match node {
                Node::Folder { id, folder } => stack.push((id, folder)),
                Node::Project { .. } => return Ok(Err(SpaceError::NotEmpty)),
            }
        }
        batch
            .delete(Cf::Meta, keys::folder_key(uid, folder_id))
            .delete(
                Cf::Idx,
                keys::child_key(
                    uid,
                    ParentId::from(folder.parent_id),
                    ChildKind::Folder,
                    &folder_id.to_text(),
                )?,
            );
        report.folders += 1;
    }

    store.write(batch).await?;
    tracing::info!(
        %uid,
        folder = %id,
        folders = report.folders,
        canvases = report.canvases,
        files = report.files,
        "文件夹已删除"
    );
    Ok(Ok(report))
}

async fn stage_user_usage(
    store: &Store,
    batch: &mut Batch,
    uid: Uid,
    deleted_bytes: u64,
) -> Result<()> {
    if let Some(mut user) = crate::domain::identity::find_by_uid(store, uid).await? {
        user.bytes_used = user.bytes_used.saturating_sub(deleted_bytes);
        batch.put(Cf::Meta, keys::user_key(uid), records::encode(&user)?);
    }
    Ok(())
}

/// `candidate` 是否是 `ancestor` 的子孙（含自己）。
///
/// 沿父指针往上走，而不是从 `ancestor` 往下遍历整棵子树：树深通常只有几层，
/// 往上走是 O(深度) 而往下是 O(子树规模)。
async fn is_descendant(
    store: &Store,
    uid: Uid,
    candidate: FolderId,
    ancestor: FolderId,
) -> Result<bool> {
    let mut cur = Some(candidate);
    // 设一个上限：如果数据已经因为别的 bug 形成了环，这里不能死循环。
    for _ in 0..64 {
        let Some(id) = cur else { return Ok(false) };
        if id == ancestor {
            return Ok(true);
        }
        cur = get_folder(store, uid, id).await?.and_then(|f| f.parent_id);
    }
    tracing::error!(%uid, %candidate, "沿父指针上溯超过 64 层，树结构可能已有环");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 失败原因彼此可区分() {
        // 这四种失败对应四个不同的 HTTP 状态码，混了会让前端无法给出
        // 有意义的提示。
        assert_ne!(SpaceError::NotFound, SpaceError::ParentIsCanvas);
        assert_ne!(SpaceError::WouldCycle, SpaceError::NotEmpty);
    }

    #[test]
    fn 根层父节点转换() {
        assert_eq!(ParentId::from(None), ParentId::Root);
        let id = FolderId::new();
        assert_eq!(ParentId::from(Some(id)), ParentId::Folder(id));
    }
}
