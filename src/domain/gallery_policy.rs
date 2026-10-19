//! 自动共享策略。所有调用者必须持有 operation_lock；写入组合成同一批次。
use super::{canvas, gallery, identity, plans, space};
use crate::storage::{
    Batch, Cf, Store,
    keys::{self, CanvasId, GenerationId, ProjectId, Uid},
    records::{
        self, Canvas, CanvasRole, Project, ProjectStatus, ShareConfig, ShareCover, User, Visibility,
    },
};
use anyhow::Result;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("基础版必须自动共享作品；升级 Pro 或 Max 可修改共享设置")]
    RequiredByPlan,
    #[error("无权限操作共享设置")]
    Forbidden,
    #[error("资源不存在或不可用")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Preference {
    pub v: u16,
    pub auto_share_gallery: bool,
    pub updated_at: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Suppression {
    None,
    Owner,
    Admin,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Control {
    pub v: u16,
    pub suppression: Suppression,
    pub last_share_config: Option<ShareConfig>,
    pub updated_at: u64,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            v: 1,
            suppression: Suppression::None,
            last_share_config: None,
            updated_at: 0,
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Ready {
    pub v: u16,
    pub generation_id: GenerationId,
    pub canvas_id: CanvasId,
    pub entry: String,
    pub entry_hash: String,
    pub cover_path: Option<String>,
    pub completed_at: u64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub v: u16,
    pub canvas_id: CanvasId,
    pub request_digest: String,
    pub processed_at: u64,
    pub auto_share_applied: bool,
}
#[derive(Debug, Serialize)]
pub struct Completion {
    pub generation_id: GenerationId,
    pub task_user_id: Uid,
    pub project_id: ProjectId,
    pub canvas_id: CanvasId,
    pub entry: String,
    pub entry_hash: String,
    pub cover_path: Option<String>,
}

pub async fn read_json<T: DeserializeOwned>(store: &Store, key: Vec<u8>) -> Result<Option<T>> {
    let Some(raw) = store.get(Cf::Meta, key).await? else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_slice(&raw)?;
    anyhow::ensure!(
        value.get("v").and_then(|v| v.as_u64()) == Some(1),
        "画廊状态版本不受支持"
    );
    Ok(Some(serde_json::from_value(value)?))
}
pub fn stage_json(batch: &mut Batch, key: Vec<u8>, value: &impl Serialize) -> Result<()> {
    batch.put(Cf::Meta, key, serde_json::to_vec(value)?);
    Ok(())
}
pub async fn control(store: &Store, pid: ProjectId) -> Result<Control> {
    Ok(read_json(store, keys::gallery_control_key(pid))
        .await?
        .unwrap_or_default())
}
pub async fn user(store: &Store, uid: Uid) -> Result<User> {
    identity::find_by_uid(store, uid)
        .await?
        .filter(|u| !u.disabled)
        .ok_or_else(|| PolicyError::NotFound.into())
}
pub async fn can_customize(store: &Store, uid: Uid) -> Result<bool> {
    let u = user(store, uid).await?;
    Ok(u.role.is_admin() || plans::get(store, uid, u.created_at).await?.plan != plans::Plan::Basic)
}
/// 返回（当前有效自动共享值，允许关闭）。即使 Basic 也检查存在的偏好记录是否损坏。
pub async fn preferences(store: &Store, uid: Uid) -> Result<(bool, bool)> {
    let p: Option<Preference> = read_json(store, keys::gallery_preference_key(uid)).await?;
    let can = can_customize(store, uid).await?;
    Ok((!can || p.is_none_or(|p| p.auto_share_gallery), can))
}
pub async fn set_preference(store: &Store, uid: Uid, enabled: bool, now: u64) -> Result<()> {
    let (_, can) = preferences(store, uid).await?;
    if !enabled && !can {
        return Err(PolicyError::RequiredByPlan.into());
    }
    let mut batch = Batch::new();
    stage_json(
        &mut batch,
        keys::gallery_preference_key(uid),
        &Preference {
            v: 1,
            auto_share_gallery: enabled,
            updated_at: now,
        },
    )?;
    store.write(batch).await
}
/// 检查操作者自身权限；管理员可操作任意所有者，普通用户不得清除管理员标记。
pub async fn authorize_change(store: &Store, actor: Uid, owner: Uid, pid: ProjectId) -> Result<()> {
    let u = user(store, actor).await?;
    if u.role.is_admin() {
        return Ok(());
    }
    if actor != owner {
        return Err(PolicyError::NotFound.into());
    }
    if !can_customize(store, actor).await? {
        return Err(PolicyError::RequiredByPlan.into());
    }
    if control(store, pid).await?.suppression == Suppression::Admin {
        return Err(PolicyError::Forbidden.into());
    }
    Ok(())
}
pub async fn stage_clear(store: &Store, batch: &mut Batch, pid: ProjectId, now: u64) -> Result<()> {
    let mut c = control(store, pid).await?;
    c.suppression = Suppression::None;
    c.updated_at = now;
    stage_json(batch, keys::gallery_control_key(pid), &c)
}
pub async fn stage_close(
    store: &Store,
    batch: &mut Batch,
    actor: Uid,
    owner: Uid,
    pid: ProjectId,
    now: u64,
) -> Result<()> {
    authorize_change(store, actor, owner, pid).await?;
    let mut c = control(store, pid).await?;
    if let Some(share) = gallery::get(store, pid).await? {
        c.last_share_config = Some(share.config)
    }
    c.suppression = if actor != owner {
        Suppression::Admin
    } else {
        Suppression::Owner
    };
    c.updated_at = now;
    stage_json(batch, keys::gallery_control_key(pid), &c)?;
    gallery::stage_remove(store, batch, pid).await
}
/// 可见性修改与下架同批提交；文件夹级联失败时整批丢弃。
pub async fn stage_private(
    store: &Store,
    batch: &mut Batch,
    actor: Uid,
    owner: Uid,
    cid: CanvasId,
    c: &Canvas,
    now: u64,
) -> Result<()> {
    if let Some(share) = gallery::get(store, c.project_id).await?
        && share.config.canvas_ids.contains(&cid)
    {
        stage_close(store, batch, actor, owner, c.project_id, now).await?;
    }
    Ok(())
}
fn defaults(project: &Project) -> ShareConfig {
    let title: String = project
        .name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(100)
        .collect();
    ShareConfig {
        title: if title.is_empty() {
            "未命名作品".into()
        } else {
            title
        },
        description: String::new(),
        author_name: "创作者".into(),
        canvas_ids: vec![],
        cover: None,
    }
}
async fn valid_cover(
    store: &Store,
    cover: Option<ShareCover>,
    ids: &[CanvasId],
) -> Result<Option<ShareCover>> {
    let Some(c) = cover else { return Ok(None) };
    let ext = c.path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if !ids.contains(&c.canvas_id)
        || keys::validate_path(&c.path).is_err()
        || !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp")
    {
        return Ok(None);
    }
    Ok(canvas::get_file(store, c.canvas_id, &c.path)
        .await?
        .map(|_| c))
}
async fn active_project(store: &Store, owner: Uid, pid: ProjectId) -> Result<Project> {
    user(store, owner).await?;
    space::get_project(store, owner, pid)
        .await?
        .filter(|p| p.status == ProjectStatus::Active)
        .ok_or_else(|| PolicyError::NotFound.into())
}
fn stage_canvas(batch: &mut Batch, owner: Uid, cid: CanvasId, c: &Canvas) -> Result<()> {
    batch.put(Cf::Meta, keys::canvas_key(owner, cid), records::encode(c)?);
    Ok(())
}

pub async fn toggle(
    store: &Store,
    actor: Uid,
    owner: Uid,
    pid: ProjectId,
    enabled: bool,
    now: u64,
) -> Result<()> {
    authorize_change(store, actor, owner, pid).await?;
    let project = space::get_project(store, owner, pid)
        .await?
        .ok_or(PolicyError::NotFound)?;
    if enabled && project.status != ProjectStatus::Active {
        return Err(PolicyError::Conflict("项目正在删除".into()).into());
    }
    let mut batch = Batch::new();
    if !enabled {
        stage_close(store, &mut batch, actor, owner, pid, now).await?;
        return store.write(batch).await;
    }
    // 关闭操作允许管理员管理被禁用所有者，但重新开启要求所有者有效。
    user(store, owner).await?;
    let old = gallery::get(store, pid).await?;
    if let Some(old) = &old
        && !gallery::inspect(store, old).await?.canvases.is_empty()
    {
        stage_clear(store, &mut batch, pid, now).await?;
        return store.write(batch).await;
    }
    let c = control(store, pid).await?;
    let mut config = old
        .map(|s| s.config)
        .or(c.last_share_config)
        .unwrap_or_else(|| defaults(&project));
    let saved = config.canvas_ids.clone();
    let mut ready = HashMap::new();
    // 只遍历项目最多 100 个制作画布，避免对完成回执做全量扫描。
    for (cid, mut canvas) in space::list_project_canvases(store, owner, pid).await? {
        if canvas.role != CanvasRole::Production {
            continue;
        }
        let record: Option<Ready> = read_json(store, keys::gallery_ready_key(pid, cid)).await?;
        if let Some(r) = record {
            if r.canvas_id != cid {
                anyhow::bail!("完成记录画布不一致")
            }
            if canvas.entry.as_deref() != Some(&r.entry) {
                continue;
            }
            if canvas::get_file(store, cid, &r.entry)
                .await?
                .is_none_or(|f| f.hash.to_hex() != r.entry_hash)
            {
                continue;
            }
        } else if !saved.contains(&cid) {
            continue;
        }
        let Some(entry) = &canvas.entry else { continue };
        if canvas::get_file(store, cid, entry).await?.is_none() {
            continue;
        }
        canvas.visibility = Visibility::Public;
        canvas.updated_at = now;
        stage_canvas(&mut batch, owner, cid, &canvas)?;
        ready.insert(cid, canvas);
    }
    if ready.is_empty() {
        return Err(PolicyError::Conflict("没有可恢复的已完成制作画布".into()).into());
    }
    config.canvas_ids = ready.keys().copied().collect();
    config.canvas_ids.sort();
    config.cover = valid_cover(store, config.cover, &config.canvas_ids).await?;
    gallery::stage_put(store, &mut batch, owner, pid, config, now, &ready).await?;
    stage_clear(store, &mut batch, pid, now).await?;
    store.write(batch).await
}

/// 返回（是否重复通知，当时是否执行了自动共享）。身份及 Worker 名单由 HTTP 层在锁内检查。
pub async fn complete(store: &Store, r: Completion, now: u64) -> Result<(bool, bool)> {
    let owner = r.task_user_id;
    let pid = r.project_id;
    let project = active_project(store, owner, pid).await?;
    let mut c = space::get_canvas(store, owner, r.canvas_id)
        .await?
        .filter(|c| c.project_id == pid && c.role == CanvasRole::Production)
        .ok_or(PolicyError::NotFound)?;
    let digest = blake3::hash(&serde_json::to_vec(&r)?).to_hex().to_string();
    if let Some(done) =
        read_json::<Receipt>(store, keys::gallery_done_key(pid, r.generation_id)).await?
    {
        if done.request_digest != digest {
            return Err(PolicyError::Conflict("生成 ID 已用于不同的完成通知".into()).into());
        }
        return Ok((true, done.auto_share_applied));
    }
    let file = canvas::get_file(store, r.canvas_id, &r.entry)
        .await?
        .ok_or_else(|| PolicyError::Conflict("入口文件不存在".into()))?;
    if file.hash.to_hex() != r.entry_hash {
        return Err(PolicyError::Conflict("入口文件内容版本已变化".into()).into());
    }
    let (auto, can) = preferences(store, owner).await?;
    let ctrl = control(store, pid).await?;
    let publish = ctrl.suppression != Suppression::Admin
        && auto
        && (!can || ctrl.suppression == Suppression::None);
    let cover = valid_cover(
        store,
        r.cover_path.clone().map(|path| ShareCover {
            canvas_id: r.canvas_id,
            path,
        }),
        &[r.canvas_id],
    )
    .await?;
    if r.cover_path.is_some() && cover.is_none() {
        tracing::warn!(project_id=%pid, canvas_id=%r.canvas_id, "完成通知封面无效，降级为空");
    }
    let mut batch = Batch::new();
    c.entry = Some(r.entry.clone());
    c.updated_at = now;
    if publish {
        c.visibility = Visibility::Public;
    }
    stage_canvas(&mut batch, owner, r.canvas_id, &c)?;
    if publish {
        let old = gallery::get(store, pid).await?;
        let mut config = match old {
            Some(s) => {
                let visible = gallery::inspect(store, &s).await?;
                let mut cfg = s.config;
                cfg.canvas_ids = visible.canvases.into_iter().map(|(id, _)| id).collect();
                cfg
            }
            None => defaults(&project),
        };
        if !config.canvas_ids.contains(&r.canvas_id) {
            config.canvas_ids.push(r.canvas_id)
        }
        config.cover = valid_cover(store, config.cover, &config.canvas_ids)
            .await?
            .or(cover.clone());
        let overrides = HashMap::from([(r.canvas_id, c)]);
        gallery::stage_put(store, &mut batch, owner, pid, config, now, &overrides).await?;
        if ctrl.suppression == Suppression::Owner {
            stage_clear(store, &mut batch, pid, now).await?;
        }
    }
    stage_json(
        &mut batch,
        keys::gallery_ready_key(pid, r.canvas_id),
        &Ready {
            v: 1,
            generation_id: r.generation_id,
            canvas_id: r.canvas_id,
            entry: r.entry,
            entry_hash: r.entry_hash,
            cover_path: cover.map(|c| c.path),
            completed_at: now,
        },
    )?;
    stage_json(
        &mut batch,
        keys::gallery_done_key(pid, r.generation_id),
        &Receipt {
            v: 1,
            canvas_id: r.canvas_id,
            request_digest: digest,
            processed_at: now,
            auto_share_applied: publish,
        },
    )?;
    store.write(batch).await?;
    Ok((false, publish))
}

/// deleting 已持久化后分块清理；项目删除重试可以继续。
pub async fn cleanup_project(store: &Store, pid: ProjectId) -> Result<()> {
    for prefix in [
        keys::gallery_ready_prefix(pid),
        keys::gallery_done_prefix(pid),
    ] {
        loop {
            let rows = store
                .scan_prefix_keys(Cf::Meta, prefix.clone(), 256)
                .await?;
            if rows.is_empty() {
                break;
            }
            let mut batch = Batch::new();
            for key in rows {
                batch.delete(Cf::Meta, key);
            }
            store.write(batch).await?;
        }
    }
    let mut batch = Batch::new();
    batch.delete(Cf::Meta, keys::gallery_control_key(pid));
    store.write(batch).await
}
