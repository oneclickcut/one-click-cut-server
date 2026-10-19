//! Manual exports reuse durable leases, and publish both variants in one batch.
use super::{
    canvas, identity,
    jobs::{self, InputFile, JobError, JobRecord, ResourceRequest, conflict, invalid},
    space,
};
use crate::storage::{
    Batch, Cf, Store, blob,
    keys::{self, CanvasId, TaskId, Uid},
    records::{self, CanvasRole, ProjectStatus},
};
use anyhow::Result;
use serde_json::{Value, json};

pub fn export_key(cid: CanvasId) -> Vec<u8> {
    format!("mp4_export:{cid}").into_bytes()
}
pub const VARIANTS: [(&str, &str); 2] = [
    ("mp4", "story.mp4"),
    ("mp4_no_subtitles", "story-no-subtitles.mp4"),
];
pub fn variant_path(kind: &str) -> Result<&'static str> {
    VARIANTS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, p)| *p)
        .ok_or_else(|| invalid("MP4 版本无效"))
}
pub fn candidate_path(job: &JobRecord, kind: &str) -> Result<String> {
    Ok(format!("exports/{}/{}", job.job_id, variant_path(kind)?))
}
pub async fn state(store: &Store, cid: CanvasId) -> Result<Value> {
    let published: Option<Value> = store
        .get(Cf::Meta, export_key(cid))
        .await?
        .map(|v| serde_json::from_slice(&v))
        .transpose()?;
    let current = canvas::get_file(store, cid, "storyboard.json").await?;
    let stale = if let Some(p) = &published {
        current.is_none_or(|f| p["storyboard_hash"] != f.hash.to_hex())
    } else {
        store
            .get(Cf::Meta, keys::resource_version_key(cid))
            .await?
            .is_some()
    };
    Ok(json!({"exports_stale":stale,"published":published}))
}
pub async fn prepare(
    store: &Store,
    uid: Uid,
    cid: CanvasId,
    request: ResourceRequest,
    now: u64,
) -> Result<JobRecord> {
    if !(8..=100).contains(&request.request_id.len())
        || !request
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || request.expected_storyboard_hash.len() != 64
        || !request
            .expected_storyboard_hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("版本或 request_id 无效"));
    }
    let c = space::get_canvas(store, uid, cid)
        .await?
        .ok_or(JobError::NotFound)?;
    let p = space::get_project(store, uid, c.project_id)
        .await?
        .ok_or(JobError::NotFound)?;
    if c.role != CanvasRole::Production
        || p.status != ProjectStatus::Active
        || c.entry.as_deref().is_some_and(|v| v != "storyboard.json")
    {
        return Err(invalid("此画布不可导出"));
    }
    if !jobs::page(store, None, Some(cid), None, true, 1)
        .await?
        .is_empty()
    {
        return Err(conflict("该剧集已有活动任务"));
    }
    let board = jobs::read_json(store, cid, "storyboard.json").await?;
    let scenes = board["scenes"]
        .as_array()
        .filter(|s| !s.is_empty() && s.len() <= 500)
        .ok_or_else(|| invalid("分镜无效"))?;
    let mut urls = vec![];
    for scene in scenes {
        for key in ["image_url", "audio_url"] {
            let u = scene[key]
                .as_str()
                .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
                .ok_or_else(|| invalid("分镜资源未完成"))?;
            urls.push(u);
        }
        if !scene["audio_duration_seconds"]
            .as_f64()
            .is_some_and(|d| d > 0. && d.is_finite())
        {
            return Err(invalid("分镜音频时长无效"));
        }
    }
    let mut dependencies = vec![];
    for id in [cid, p.resources_canvas_id] {
        let canvas = space::get_canvas(store, uid, id)
            .await?
            .ok_or(JobError::NotFound)?;
        for row in canvas::list_files(store, id, canvas::MAX_FILES_PER_CANVAS as usize).await? {
            let url = format!(
                "/f/{}/{}?v={}",
                canvas.slug,
                row.path,
                row.entry.hash.to_hex()
            );
            if (id == cid && row.path == "storyboard.json")
                || urls.iter().any(|u| jobs::reference_matches(u, &url))
            {
                dependencies.push(InputFile {
                    canvas_id: id,
                    path: row.path,
                    hash: row.entry.hash.to_hex(),
                    url,
                });
            }
        }
    }
    if !dependencies.iter().any(|d| {
        d.canvas_id == cid
            && d.path == "storyboard.json"
            && d.hash == request.expected_storyboard_hash
    }) {
        return Err(conflict("分镜版本已变化，请刷新后重试"));
    }
    // The worker also checks the downloaded external asset bytes before publishing.
    Ok(JobRecord {
        schema_version: 2,
        job_id: TaskId::new().to_string(),
        kind: "mp4.export".into(),
        owner_uid: uid,
        project_id: c.project_id,
        canvas_id: cid,
        request,
        status: "queued".into(),
        stage: "queued".into(),
        created_at: now,
        updated_at: now,
        attempt: 0,
        lease: None,
        input: json!({"storyboard":board}),
        dependencies,
        candidate: None,
        result: None,
        error: None,
    })
}
pub async fn commit(store: &Store, job: &mut JobRecord, now: u64) -> Result<()> {
    jobs::assert_inputs(store, job).await?;
    let candidates = job
        .candidate
        .as_ref()
        .ok_or_else(|| invalid("两个 MP4 尚未上传完成"))?;
    let mut c = space::get_canvas(store, job.owner_uid, job.canvas_id)
        .await?
        .ok_or(JobError::NotFound)?;
    let mut user = identity::find_by_uid(store, job.owner_uid)
        .await?
        .ok_or(JobError::NotFound)?;
    let mut b = Batch::new();
    let mut published = json!({});
    for (kind, target) in VARIANTS {
        let path = candidate_path(job, kind)?;
        let candidate = &candidates[kind];
        let mut entry = canvas::get_file(store, job.canvas_id, &path)
            .await?
            .ok_or_else(|| invalid("MP4 候选缺失"))?;
        if candidate["hash"] != entry.hash.to_hex() || candidate["size"] != entry.size {
            return Err(conflict("MP4 候选已变化"));
        }
        if super::canvas_dir::conflict_for_file(store, job.canvas_id, target)
            .await?
            .is_some()
        {
            return Err(conflict("MP4 路径已被目录占用"));
        }
        if let Some(old) = canvas::get_file(store, job.canvas_id, target).await? {
            blob::stage_dereference(store, &mut b, old.hash, now).await?;
            // Both old variants can share one hash. Each decrement observes the
            // pre-batch count, so also schedule a GC check for their combined delta.
            // GC rechecks the final reference count before deleting any content.
            b.put(
                Cf::Gc,
                keys::gc_key(keys::GcKind::Blob, now + blob::GC_DELAY_SECS, old.hash),
                Vec::new(),
            );
            c.bytes -= old.size;
            user.bytes_used -= old.size;
            c.file_count -= 1;
        }
        // Move the candidate's reference; do not load large videos into memory.
        entry.updated_at = now;
        b.delete(Cf::Meta, keys::file_key(job.canvas_id, &path)?);
        b.put(
            Cf::Meta,
            keys::file_key(job.canvas_id, target)?,
            records::encode(&entry)?,
        );
        published[kind] = json!({"path":target,"hash":entry.hash.to_hex(),"size":entry.size});
    }
    c.updated_at = now;
    b.put(
        Cf::Meta,
        keys::canvas_key(job.owner_uid, job.canvas_id),
        records::encode(&c)?,
    );
    b.put(
        Cf::Meta,
        keys::user_key(job.owner_uid),
        records::encode(&user)?,
    );
    let receipt = json!({"storyboard_hash":job.request.expected_storyboard_hash,"job_id":job.job_id,"updated_at":now,"files":published});
    b.put(
        Cf::Meta,
        export_key(job.canvas_id),
        serde_json::to_vec(&receipt)?,
    );
    job.status = "succeeded".into();
    job.stage = "committed".into();
    job.updated_at = now;
    job.result = Some(receipt);
    jobs::stage_job(&mut b, job)?;
    b.delete(Cf::Meta, keys::project_execution_key(job.project_id));
    store.write(b).await
}

/// Remove unpublished candidates after failure; completed exports have already moved them.
pub async fn cleanup(store: &Store, job: &JobRecord, now: u64) -> Result<()> {
    if job.kind != "mp4.export" || job.active() || job.status == "succeeded" {
        return Ok(());
    }
    for (kind, _) in VARIANTS {
        if let Some(id) = job.input["uploads"][kind]["upload_id"].as_str() {
            let _ = super::upload::abort(store, keys::UploadId::parse(id)?, now).await?;
        }
    }
    let paths: Vec<String> = VARIANTS
        .iter()
        .map(|(kind, _)| candidate_path(job, kind))
        .collect::<Result<_>>()?;
    canvas::delete_files(store, job.owner_uid, job.canvas_id, &paths, now)
        .await?
        .map_err(|e| invalid(&format!("候选清理失败: {e:?}")))?;
    Ok(())
}
