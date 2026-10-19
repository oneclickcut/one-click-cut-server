//! Durable operations. All mutators are called under the server operation_lock.
use super::{canvas, identity, space};
use crate::storage::{
    Batch, Cf, Store, blob,
    keys::{self, CanvasId, ProjectId, TaskId, Uid},
    records::{CanvasRole, ProjectStatus},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const LEASE_SECONDS: u64 = 120;
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("资源不存在")]
    NotFound,
}
pub(super) fn conflict(message: &str) -> anyhow::Error {
    JobError::Conflict(message.into()).into()
}
pub(super) fn invalid(message: &str) -> anyhow::Error {
    JobError::Invalid(message.into()).into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequest {
    pub request_id: String,
    pub resource_type: String,
    pub scene_index: usize,
    pub expected_storyboard_hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFile {
    pub canvas_id: CanvasId,
    pub path: String,
    pub hash: String,
    pub url: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub token: String,
    pub epoch: u64,
    pub expires_at: u64,
    pub worker_uid: Uid,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub owner_uid: Uid,
    pub holder: String,
    pub lease: Lease,
    pub agent_token_hash: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord {
    pub schema_version: u8,
    pub job_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub owner_uid: Uid,
    pub project_id: ProjectId,
    pub canvas_id: CanvasId,
    pub request: ResourceRequest,
    pub status: String,
    pub stage: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub attempt: u64,
    pub lease: Option<Lease>,
    pub input: Value,
    pub dependencies: Vec<InputFile>,
    pub candidate: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<String>,
}
impl JobRecord {
    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }
    pub fn public(&self) -> Value {
        json!({"schema_version":2,"job_id":self.job_id,"type":self.kind,"user_id":self.owner_uid,
            "project_id":self.project_id,"canvas_id":self.canvas_id,"request_id":self.request.request_id,
            "resource_type":self.request.resource_type,"scene_index":self.request.scene_index,
            "expected_storyboard_hash":self.request.expected_storyboard_hash,
            "status":self.status,"stage":self.stage,"created_at":self.created_at,"updated_at":self.updated_at,
            "attempt":self.attempt,"result":self.result,"error":self.error,"progress":self.input.get("export_progress")})
    }
}
pub async fn get(store: &Store, id: &str) -> Result<JobRecord> {
    TaskId::parse(id).map_err(|_| JobError::NotFound)?;
    let raw = store
        .get(Cf::Meta, keys::job_key(id))
        .await?
        .ok_or(JobError::NotFound)?;
    Ok(serde_json::from_slice(&raw)?)
}
pub fn stage_job(batch: &mut Batch, job: &JobRecord) -> Result<()> {
    batch.put(
        Cf::Meta,
        keys::job_key(&job.job_id),
        serde_json::to_vec(job)?,
    );
    batch.put(
        Cf::Idx,
        keys::job_index_key(keys::job_canvas_prefix(job.canvas_id), &job.job_id),
        job.job_id.as_bytes(),
    );
    batch.put(
        Cf::Idx,
        keys::job_index_key(keys::job_owner_prefix(job.owner_uid), &job.job_id),
        job.job_id.as_bytes(),
    );
    if job.active() {
        batch.put(
            Cf::Idx,
            keys::job_ready_key(&job.job_id),
            job.job_id.as_bytes(),
        );
    } else {
        batch.delete(Cf::Idx, keys::job_ready_key(&job.job_id));
    }
    Ok(())
}
pub async fn list(store: &Store) -> Result<Vec<JobRecord>> {
    store
        .scan_prefix(Cf::Meta, keys::job_prefix())
        .await?
        .into_iter()
        .map(|(_, raw)| Ok(serde_json::from_slice(&raw)?))
        .collect()
}
pub async fn read_json(store: &Store, id: CanvasId, path: &str) -> Result<Value> {
    let entry = canvas::get_file(store, id, path)
        .await?
        .ok_or_else(|| invalid(&format!("缺少 {path}")))?;
    if entry.size > 8 * 1024 * 1024 {
        return Err(invalid("输入 JSON 过大"));
    }
    let data = blob::read_all(store, entry.hash)
        .await?
        .ok_or(JobError::NotFound)?;
    serde_json::from_slice(&data).map_err(|_| invalid("输入 JSON 无效"))
}
pub async fn execution(store: &Store, pid: ProjectId) -> Result<Option<Execution>> {
    store
        .get(Cf::Meta, keys::project_execution_key(pid))
        .await?
        .map(|raw| Ok(serde_json::from_slice(&raw)?))
        .transpose()
}
pub async fn assert_inputs(store: &Store, job: &JobRecord) -> Result<()> {
    let owner = identity::find_by_uid(store, job.owner_uid)
        .await?
        .ok_or(JobError::NotFound)?;
    let p = space::get_project(store, job.owner_uid, job.project_id)
        .await?
        .ok_or(JobError::NotFound)?;
    let c = space::get_canvas(store, job.owner_uid, job.canvas_id)
        .await?
        .ok_or(JobError::NotFound)?;
    if owner.disabled
        || p.status != ProjectStatus::Active
        || c.project_id != job.project_id
        || c.entry.as_deref().is_some_and(|p| p != "storyboard.json")
    {
        return Err(conflict("项目或播放入口已变化"));
    }
    for dep in &job.dependencies {
        let current = canvas::get_file(store, dep.canvas_id, &dep.path).await?;
        if current.is_none_or(|f| f.hash.to_hex() != dep.hash) {
            return Err(conflict("分镜或参考资料已变化，请刷新后重试"));
        }
    }
    if job.kind == "mp4.export" {
        return Ok(());
    }
    // Detect newly added inputs too (for example a reference-assets.json added
    // while this job was queued). Candidate media are not creative inputs.
    for id in [job.canvas_id, p.resources_canvas_id] {
        let current = canvas::list_files(store, id, canvas::MAX_FILES_PER_CANVAS as usize).await?;
        let paths: std::collections::BTreeSet<_> = current
            .iter()
            .filter(|row| is_input(id == job.canvas_id, &row.path))
            .map(|row| row.path.as_str())
            .collect();
        let expected: std::collections::BTreeSet<_> = job
            .dependencies
            .iter()
            .filter(|dep| dep.canvas_id == id)
            .map(|dep| dep.path.as_str())
            .collect();
        if paths != expected {
            return Err(conflict("参考资料清单已变化，请刷新后重试"));
        }
    }
    Ok(())
}
fn is_input(production: bool, path: &str) -> bool {
    !production
        || path == "storyboard.json"
        || path.starts_with("job/")
        || path.contains("character")
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
        || !["image", "audio"].contains(&request.resource_type.as_str())
        || request.expected_storyboard_hash.len() != 64
        || !request
            .expected_storyboard_hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("资源类型、版本或 request_id 无效"));
    }
    let c = space::get_canvas(store, uid, cid)
        .await?
        .ok_or(JobError::NotFound)?;
    let p = space::get_project(store, uid, c.project_id)
        .await?
        .ok_or(JobError::NotFound)?;
    if c.role != CanvasRole::Production
        || c.entry.as_deref().is_some_and(|p| p != "storyboard.json")
        || p.status != ProjectStatus::Active
    {
        return Err(invalid("此画布不是可编辑分镜作品"));
    }
    if page(store, None, Some(cid), None, true, 1)
        .await?
        .iter()
        .any(|j| j.active())
    {
        return Err(conflict("该剧集已有资源任务"));
    }
    let board = read_json(store, cid, "storyboard.json").await?;
    let scene = board["scenes"]
        .as_array()
        .and_then(|s| s.get(request.scene_index))
        .ok_or_else(|| invalid("分镜不存在"))?;
    let original = scene[format!("{}_url", request.resource_type)]
        .as_str()
        .unwrap_or("");
    if !(original.starts_with("https://") || original.starts_with("http://"))
        || !original.contains("/f/")
    {
        return Err(invalid("原资源缺少文件服务器地址"));
    }
    if scene["narration"]
        .as_str()
        .is_none_or(|s| s.trim().is_empty())
    {
        return Err(invalid("分镜缺少旁白"));
    }
    let constraints = read_json(store, p.resources_canvas_id, "constraints.json").await?;
    let mut dependencies = vec![];
    // Freeze existing creative inputs and references; output files are not copied into the job.
    for id in [cid, p.resources_canvas_id] {
        let canvas = space::get_canvas(store, uid, id)
            .await?
            .ok_or(JobError::NotFound)?;
        for row in canvas::list_files(store, id, canvas::MAX_FILES_PER_CANVAS as usize).await? {
            if !is_input(id == cid, &row.path) {
                continue;
            }
            dependencies.push(InputFile {
                canvas_id: id,
                path: row.path.clone(),
                hash: row.entry.hash.to_hex(),
                url: format!(
                    "/f/{}/{}?v={}",
                    canvas.slug,
                    row.path,
                    row.entry.hash.to_hex()
                ),
            });
        }
    }
    if !dependencies.iter().any(|d| {
        d.canvas_id == cid
            && d.path == "storyboard.json"
            && d.hash == request.expected_storyboard_hash
    }) {
        return Err(conflict("分镜版本已变化，请刷新后重试"));
    }
    // Only persisted project reference files can be supplied to a paid provider.
    if request.resource_type == "image" {
        for r in scene["references"]
            .as_array()
            .ok_or_else(|| invalid("分镜参考图无效"))?
        {
            let url = r["image_url"]
                .as_str()
                .ok_or_else(|| invalid("参考图尚未生成"))?;
            if !dependencies.iter().any(|d| reference_matches(url, &d.url)) {
                return Err(invalid("参考图必须属于当前项目公共资料"));
            }
        }
    }
    Ok(JobRecord {
        schema_version: 2,
        job_id: TaskId::new().to_string(),
        kind: "resource.regenerate".into(),
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
        input: json!({"storyboard":board,"constraints":constraints}),
        dependencies,
        candidate: None,
        result: None,
        error: None,
    })
}
pub fn reference_matches(address: &str, local: &str) -> bool {
    let path = address.split('?').next().unwrap_or("");
    let local = local.split('?').next().unwrap_or("");
    path == local
        || path
            .strip_prefix("https://")
            .or_else(|| path.strip_prefix("http://"))
            .and_then(|s| s.find('/').map(|i| &s[i..]))
            == Some(local)
}

pub async fn verify_lease(
    store: &Store,
    job: &JobRecord,
    worker: Uid,
    token: &str,
    epoch: u64,
    now: u64,
) -> Result<()> {
    let l = job
        .lease
        .as_ref()
        .ok_or_else(|| conflict("任务没有执行租约"))?;
    let e = execution(store, job.project_id)
        .await?
        .ok_or_else(|| conflict("项目执行权已失效"))?;
    if !job.active()
        || l.worker_uid != worker
        || l.token != token
        || l.epoch != epoch
        || l.expires_at <= now
        || e.holder != job.job_id
        || e.lease.token != token
        || e.lease.expires_at <= now
    {
        return Err(conflict("执行租约已失效"));
    }
    Ok(())
}
fn stage_release(batch: &mut Batch, pid: ProjectId) {
    batch.delete(Cf::Meta, keys::project_execution_key(pid));
}
/// Recover only safe stages. Never repeat a provider call whose outcome is unknown.
pub async fn sweep(store: &Store, now: u64) -> Result<()> {
    for (_, id) in store.scan_prefix(Cf::Idx, keys::job_ready_prefix()).await? {
        let mut job = get(store, std::str::from_utf8(&id)?).await?;
        if job.status != "running" || job.lease.as_ref().is_some_and(|l| l.expires_at > now) {
            continue;
        }
        job.status = if job.kind == "mp4.export" {
            job.error = Some("export_worker_interrupted".into());
            "failed"
        } else if job.stage == "generating" && job.kind == "resource.regenerate" {
            "needs_review"
        } else {
            "queued"
        }
        .into();
        if job.status == "needs_review" {
            job.error = Some("provider_outcome_unknown".into());
        }
        job.updated_at = now;
        let mut b = Batch::new();
        if execution(store, job.project_id).await?.is_some_and(|e| {
            e.holder == job.job_id && job.lease.as_ref().is_some_and(|l| l.token == e.lease.token)
        }) {
            stage_release(&mut b, job.project_id);
        }
        job.lease = None;
        stage_job(&mut b, &job)?;
        store.write(b).await?;
        super::mp4_jobs::cleanup(store, &job, now).await?;
    }
    Ok(())
}
pub async fn claim(
    store: &Store,
    worker: Uid,
    now: u64,
    excluded: &[ProjectId],
) -> Result<Option<JobRecord>> {
    claim_types(
        store,
        worker,
        now,
        excluded,
        &["resource.regenerate".into()],
    )
    .await
}
pub async fn claim_types(
    store: &Store,
    worker: Uid,
    now: u64,
    excluded: &[ProjectId],
    kinds: &[String],
) -> Result<Option<JobRecord>> {
    sweep(store, now).await?;
    for (_, id) in store.scan_prefix(Cf::Idx, keys::job_ready_prefix()).await? {
        let mut job = get(store, std::str::from_utf8(&id)?).await?;
        if !kinds.contains(&job.kind)
            || job.status != "queued"
            || excluded.contains(&job.project_id)
            || execution(store, job.project_id)
                .await?
                .is_some_and(|e| e.lease.expires_at > now)
        {
            continue;
        }
        if assert_inputs(store, &job).await.is_err() {
            job.status = "conflict".into();
            job.error = Some("input_version_changed".into());
            job.updated_at = now;
            let mut b = Batch::new();
            stage_job(&mut b, &job)?;
            store.write(b).await?;
            continue;
        }
        job.attempt += 1;
        job.status = "running".into();
        job.updated_at = now;
        let lease = Lease {
            token: TaskId::new().to_string(),
            epoch: job.attempt,
            expires_at: now + LEASE_SECONDS,
            worker_uid: worker,
        };
        job.lease = Some(lease.clone());
        let e = Execution {
            owner_uid: job.owner_uid,
            holder: job.job_id.clone(),
            lease,
            agent_token_hash: None,
        };
        let mut b = Batch::new();
        stage_job(&mut b, &job)?;
        b.put(
            Cf::Meta,
            keys::project_execution_key(job.project_id),
            serde_json::to_vec(&e)?,
        );
        store.write(b).await?;
        return Ok(Some(job));
    }
    Ok(None)
}
pub async fn heartbeat(store: &Store, job: &mut JobRecord, now: u64) -> Result<()> {
    job.lease.as_mut().unwrap().expires_at = now + LEASE_SECONDS;
    job.updated_at = now;
    let mut e = execution(store, job.project_id)
        .await?
        .ok_or(JobError::NotFound)?;
    e.lease = job.lease.clone().unwrap();
    let mut b = Batch::new();
    stage_job(&mut b, job)?;
    b.put(
        Cf::Meta,
        keys::project_execution_key(job.project_id),
        serde_json::to_vec(&e)?,
    );
    store.write(b).await
}
pub async fn fail(store: &Store, job: &mut JobRecord, code: &str, now: u64) -> Result<()> {
    job.status = if job.stage == "generating" && job.kind == "resource.regenerate" {
        "needs_review"
    } else if code == "input_version_changed" {
        "conflict"
    } else {
        "failed"
    }
    .into();
    job.error = Some(code.into());
    job.updated_at = now;
    let mut b = Batch::new();
    stage_job(&mut b, job)?;
    stage_release(&mut b, job.project_id);
    store.write(b).await?;
    super::mp4_jobs::cleanup(store, job, now).await
}
/// Every ordinary writer, including tar/chunks, passes through this guard.
/// Registered legacy token identities remain fenced after their lease ends.
pub async fn authorize_write(
    store: &Store,
    pid: ProjectId,
    token_hash: Option<&str>,
    now: u64,
) -> Result<()> {
    let current = execution(store, pid).await?;
    if let Some(e) = &current
        && e.lease.expires_at > now
    {
        if token_hash.is_none() || e.agent_token_hash.as_deref() != token_hash {
            return Err(conflict("项目正在执行制作或资源任务"));
        }
    }
    if let Some(hash) = token_hash
        && let Some(bound) = store.get(Cf::Meta, keys::execution_token_key(hash)).await?
    {
        let token = std::str::from_utf8(&bound)?;
        if !current.is_some_and(|e| {
            e.lease.expires_at > now
                && e.lease.token == token
                && e.agent_token_hash.as_deref() == Some(hash)
        }) {
            return Err(conflict("制作执行权已失效"));
        }
    }
    Ok(())
}
pub async fn commit(store: &Store, job: &mut JobRecord, now: u64) -> Result<()> {
    if job.kind == "mp4.export" {
        return super::mp4_jobs::commit(store, job, now).await;
    }
    assert_inputs(store, job).await?;
    let candidate = job
        .candidate
        .clone()
        .ok_or_else(|| invalid("尚无已校验候选资源"))?;
    let path = candidate["path"]
        .as_str()
        .ok_or_else(|| invalid("候选路径无效"))?;
    let entry = canvas::get_file(store, job.canvas_id, path)
        .await?
        .ok_or(JobError::NotFound)?;
    if Some(entry.hash.to_hex().as_str()) != candidate["hash"].as_str() {
        return Err(conflict("候选资源发生变化"));
    }
    let c = space::get_canvas(store, job.owner_uid, job.canvas_id)
        .await?
        .ok_or(JobError::NotFound)?;
    let mut board = read_json(store, job.canvas_id, "storyboard.json").await?;
    // Keep the original file-server origin and write a unique, versioned media URL.
    let old = board["scenes"][job.request.scene_index]
        [format!("{}_url", job.request.resource_type)]
    .as_str()
    .unwrap_or("");
    let origin = old
        .find("/f/")
        .filter(|_| old.starts_with("https://") || old.starts_with("http://"))
        .map(|i| &old[..i])
        .ok_or_else(|| invalid("原资源缺少文件服务器地址"))?;
    let address = format!("{}/f/{}/{}?v={}", origin, c.slug, path, entry.hash.to_hex());
    board["scenes"][job.request.scene_index][format!("{}_url", job.request.resource_type)] =
        json!(address);
    if job.request.resource_type == "audio" {
        board["scenes"][job.request.scene_index]["audio_duration_seconds"] =
            candidate["audio_duration_seconds"].clone();
    }
    let mut b = Batch::new();
    let output = canvas::stage_put_file(
        store,
        &mut b,
        job.owner_uid,
        job.canvas_id,
        "storyboard.json",
        "application/json",
        &serde_json::to_vec(&board)?,
        now,
    )
    .await?
    .map_err(|e| invalid(&format!("无法保存分镜：{e:?}")))?;
    job.status = "succeeded".into();
    job.stage = "committed".into();
    job.updated_at = now;
    job.result = Some(
        json!({"storyboard_hash":output.hash.to_hex(),"previous_storyboard_hash":job.request.expected_storyboard_hash,
        "resource_url":address,"candidate":candidate,"exports_stale":true}),
    );
    stage_job(&mut b, job)?;
    stage_release(&mut b, job.project_id);
    b.put(Cf::Meta,keys::resource_version_key(job.canvas_id),serde_json::to_vec(&json!({"storyboard_hash":output.hash.to_hex(),"exports_stale":true,"job_id":job.job_id,"updated_at":now}))?);
    store.write(b).await
}

/// Index-backed newest-first listing. Ordinary polling never scans other owners' histories.
pub async fn page(
    store: &Store,
    uid: Option<Uid>,
    cid: Option<CanvasId>,
    before: Option<&str>,
    active: bool,
    limit: usize,
) -> Result<Vec<JobRecord>> {
    page_kind(store, uid, cid, before, active, limit, None).await
}
pub async fn page_kind(
    store: &Store,
    uid: Option<Uid>,
    cid: Option<CanvasId>,
    before: Option<&str>,
    active: bool,
    limit: usize,
    kind: Option<&str>,
) -> Result<Vec<JobRecord>> {
    let (cf, prefix) = if active {
        (Cf::Idx, keys::job_ready_prefix())
    } else if let Some(cid) = cid {
        (Cf::Idx, keys::job_canvas_prefix(cid))
    } else if let Some(uid) = uid {
        (Cf::Idx, keys::job_owner_prefix(uid))
    } else {
        (Cf::Meta, keys::job_prefix())
    };
    let mut cursor = before.map(|id| keys::job_index_key(prefix.clone(), id));
    let mut result = Vec::new();
    loop {
        let rows = store
            .scan_prefix_page(cf, prefix.clone(), cursor, true, 100)
            .await?;
        let end = rows.len() < 100;
        cursor = rows.last().map(|(k, _)| k.clone());
        for (_, raw) in rows {
            let j: JobRecord = if cf == Cf::Meta {
                serde_json::from_slice(&raw)?
            } else {
                get(store, std::str::from_utf8(&raw)?).await?
            };
            if kind.is_none_or(|k| j.kind == k)
                && uid.is_none_or(|u| j.owner_uid == u)
                && cid.is_none_or(|c| j.canvas_id == c)
                && (!active || j.active())
            {
                result.push(j);
                if result.len() >= limit {
                    return Ok(result);
                }
            }
        }
        if end {
            break;
        }
    }
    Ok(result)
}
