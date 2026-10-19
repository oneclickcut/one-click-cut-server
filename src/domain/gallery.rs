//! 项目共享元数据。调用方持有 operation_lock，文件内容继续由原画布管理。
use std::collections::{HashMap, HashSet};

use anyhow::Result;

use super::{canvas, identity, space};
use crate::storage::{
    Batch, Cf, Store,
    keys::{self, CanvasId, ProjectId, ShareId, Uid},
    records::{
        self, Canvas, CanvasRole, GalleryTarget, ProjectShare, ProjectStatus, ShareConfig,
        Visibility,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub async fn get(store: &Store, id: ProjectId) -> Result<Option<ProjectShare>> {
    let Some(raw) = store.get(Cf::Meta, keys::project_share_key(id)).await? else {
        return Ok(None);
    };
    let share: ProjectShare = records::decode(&raw)?;
    anyhow::ensure!(
        share.v == 1 && share.project_id == id,
        "共享记录版本或项目不一致"
    );
    Ok(Some(share))
}

fn text_field(value: &str, max: usize, name: &str) -> Result<String, ShareError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max || value.chars().any(char::is_control) {
        return Err(ShareError::Invalid(format!(
            "{name} 必须为 1 到 {max} 个字符且不含控制字符"
        )));
    }
    Ok(value.to_owned())
}

async fn unavailable_canvas(
    store: &Store,
    c: &Canvas,
    id: CanvasId,
) -> Result<Option<&'static str>> {
    if c.visibility != Visibility::Public {
        return Ok(Some("canvas_private"));
    }
    let Some(entry) = &c.entry else {
        return Ok(Some("entry_missing"));
    };
    if canvas::get_file(store, id, entry).await?.is_none() {
        return Ok(Some("entry_missing"));
    }
    Ok(None)
}

/// 校验全部配置后才写入；更新时保留本轮共享 ID 与排序。
pub async fn put(
    store: &Store,
    uid: Uid,
    pid: ProjectId,
    config: ShareConfig,
    now: u64,
) -> Result<(ProjectShare, bool), ShareError> {
    let mut batch = Batch::new();
    let result = stage_put(store, &mut batch, uid, pid, config, now, &HashMap::new()).await?;
    store.write(batch).await?;
    Ok(result)
}

/// 与画布修改和完成回执一起提交；overrides 为本批次尚未落盘的画布。
pub async fn stage_put(
    store: &Store,
    batch: &mut Batch,
    uid: Uid,
    pid: ProjectId,
    mut config: ShareConfig,
    now: u64,
    overrides: &HashMap<CanvasId, Canvas>,
) -> Result<(ProjectShare, bool), ShareError> {
    let p = space::get_project(store, uid, pid)
        .await?
        .ok_or_else(|| ShareError::Conflict("项目不存在".into()))?;
    if p.status != ProjectStatus::Active {
        return Err(ShareError::Conflict("项目正在删除".into()));
    }
    config.title = text_field(&config.title, 100, "标题")?;
    config.author_name = text_field(&config.author_name, 40, "作者展示名")?;
    if config.description.chars().count() > 2000 {
        return Err(ShareError::Invalid("简介最多 2000 个字符".into()));
    }
    let unique: HashSet<_> = config.canvas_ids.iter().copied().collect();
    if unique.len() != config.canvas_ids.len() || !(1..=100).contains(&unique.len()) {
        return Err(ShareError::Invalid(
            "请选择 1 到 100 个不重复的制作画布".into(),
        ));
    }
    for id in &config.canvas_ids {
        let c = match overrides.get(id) {
            Some(c) => Some(c.clone()),
            None => space::get_canvas(store, uid, *id).await?,
        }
        .filter(|c| c.project_id == pid && c.role == CanvasRole::Production)
        .ok_or_else(|| ShareError::Invalid("所选画布不属于项目的制作画布".into()))?;
        if let Some(reason) = unavailable_canvas(store, &c, *id).await? {
            return Err(ShareError::Conflict(format!("画布不可展示: {reason}")));
        }
    }
    if let Some(cover) = &config.cover {
        keys::validate_path(&cover.path).map_err(|_| ShareError::Invalid("封面路径非法".into()))?;
        let ext = cover
            .path
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !unique.contains(&cover.canvas_id)
            || !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp")
            || canvas::get_file(store, cover.canvas_id, &cover.path)
                .await?
                .is_none()
        {
            return Err(ShareError::Invalid(
                "封面必须是所选画布中存在的 JPEG、PNG 或 WebP 文件".into(),
            ));
        }
    }
    let old = get(store, pid).await?;
    if old.as_ref().is_some_and(|old| old.owner_uid != uid) {
        return Err(ShareError::Internal(anyhow::anyhow!(
            "共享记录所有者不一致"
        )));
    }
    let created = old.is_none();
    let share = ProjectShare {
        v: 1,
        project_id: pid,
        owner_uid: uid,
        share_id: old.as_ref().map_or_else(ShareId::new, |s| s.share_id),
        shared_at: old.as_ref().map_or(now, |s| s.shared_at),
        updated_at: now,
        config,
    };
    batch.put(
        Cf::Meta,
        keys::project_share_key(pid),
        records::encode(&share)?,
    );
    batch.put(
        Cf::Idx,
        keys::gallery_feed_key(share.share_id),
        records::encode(&GalleryTarget {
            project_id: pid,
            owner_uid: uid,
        })?,
    );
    Ok((share, created))
}

/// 与项目 deleting 状态写入合并，重试删除时也可调用。
pub async fn stage_remove(store: &Store, batch: &mut Batch, pid: ProjectId) -> Result<()> {
    if let Some(share) = get(store, pid).await? {
        batch.delete(Cf::Idx, keys::gallery_feed_key(share.share_id));
    }
    batch.delete(Cf::Meta, keys::project_share_key(pid));
    Ok(())
}

pub struct Presentation {
    pub canvases: Vec<(CanvasId, Canvas)>,
    pub reasons: Vec<String>,
    pub cover_url: Option<String>,
}

/// 列表和详情共用实时可见性判断，索引及共享配置均不能替代权限状态。
pub async fn inspect(store: &Store, share: &ProjectShare) -> Result<Presentation> {
    let mut result = Presentation {
        canvases: Vec::new(),
        reasons: Vec::new(),
        cover_url: None,
    };
    let owner = identity::find_by_uid(store, share.owner_uid).await?;
    if owner.is_none_or(|u| u.disabled) {
        result.reasons.push("owner_unavailable".into());
        return Ok(result);
    }
    let project = space::get_project(store, share.owner_uid, share.project_id).await?;
    let Some(project) = project.filter(|p| p.status == ProjectStatus::Active) else {
        result.reasons.push("project_unavailable".into());
        return Ok(result);
    };
    for id in &share.config.canvas_ids {
        let c = space::get_canvas(store, share.owner_uid, *id)
            .await?
            .filter(|c| c.project_id == share.project_id && c.role == CanvasRole::Production);
        let reason = match &c {
            Some(c) => unavailable_canvas(store, c, *id).await?,
            None => Some("canvas_missing"),
        };
        if let Some(reason) = reason {
            result.reasons.push(format!("{id}:{reason}"));
        } else if let Some(c) = c {
            result.canvases.push((*id, c));
        }
    }
    result.canvases.sort_by_key(|(id, c)| (c.sort_order, *id));
    // 与“我的项目”一致：按文件列表顺序查找公共资料区的 cover.png，
    // 文件名不区分大小写，可位于子目录。仅返回封面 URL，不公开资料列表。
    if !result.canvases.is_empty()
        && let Some(resources) =
            space::get_canvas(store, share.owner_uid, project.resources_canvas_id)
                .await?
                .filter(|c| c.project_id == share.project_id && c.role == CanvasRole::Resources)
        && let Some(file) = canvas::list_files(store, project.resources_canvas_id, usize::MAX)
            .await?
            .into_iter()
            .find(|file| {
                file.path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.eq_ignore_ascii_case("cover.png"))
            })
    {
        result.cover_url = Some(cover_file_url(&resources.slug, &file.path, file.entry.hash));
    }
    // 没有项目封面时兼容已有作品配置的制作画布封面。
    if result.cover_url.is_none()
        && let Some(cover) = &share.config.cover
        && let Some((_, c)) = result
            .canvases
            .iter()
            .find(|(id, _)| *id == cover.canvas_id)
        && let Some(file) = canvas::get_file(store, cover.canvas_id, &cover.path).await?
    {
        result.cover_url = Some(cover_file_url(&c.slug, &cover.path, file.hash));
    }
    Ok(result)
}

fn cover_file_url(slug: &str, path: &str, hash: keys::ContentHash) -> String {
    // 对文件路径按 UTF-8 字节编码，保留目录分隔符，避免 #、? 等改变 URL 语义。
    let path: String = path
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("{}{path}?v={}", canvas::base_url(slug), hash.to_hex8())
}
