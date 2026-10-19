//! Owner-facing operations and authenticated Worker lifecycle. Never return credentials to owners.
use super::{
    AppState,
    auth::{Caller, UserCaller},
    error::{AppError, AppResult},
    file_api::{Need, authorize_canvas},
};
use crate::{
    domain::{
        jobs::{self, JobError, ResourceRequest},
        task_queue,
    },
    storage::{
        Batch, Cf,
        keys::{self, ContentHash, ProjectId, TaskId},
    },
};
use axum::{
    Json,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};
use utoipa_axum::{
    router::{OpenApiRouter, UtoipaMethodRouterExt},
    routes,
};
fn error(e: anyhow::Error) -> AppError {
    match e.downcast_ref::<JobError>() {
        Some(JobError::Invalid(s)) => AppError::BadRequest(s.clone()),
        Some(JobError::Conflict(s)) => AppError::Conflict(s.clone()),
        Some(JobError::NotFound) => AppError::NotFound,
        None => AppError::Internal(e),
    }
}
pub fn router(max_bytes: usize) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create))
        .routes(routes!(create_export))
        .routes(routes!(export_progress))
        .routes(routes!(export_upload))
        .routes(routes!(export_part).layer(DefaultBodyLimit::max(16 * 1024 * 1024)))
        .routes(routes!(export_complete))
        .routes(routes!(list))
        .routes(routes!(get))
        .routes(routes!(state))
        .routes(routes!(claim))
        .routes(routes!(heartbeat))
        .routes(routes!(checkpoint))
        .routes(routes!(commit))
        .routes(routes!(fail))
        .routes(routes!(execution))
        .routes(routes!(execution_heartbeat))
        .routes(routes!(execution_release))
        .routes(routes!(input))
        .routes(routes!(candidate).layer(DefaultBodyLimit::max(max_bytes)))
}
fn as_caller(u: &UserCaller) -> Caller {
    Caller::User {
        uid: u.uid,
        role: u.role,
    }
}
#[utoipa::path(post,path="/canvases/{id}/resource-jobs",tag="jobs",request_body=ResourceRequest,security(("bearer_jwt"=[])),responses((status=201,body=Value),(status=200,body=Value),(status=409,description="版本或幂等键冲突"),(status=429,description="任务额度用尽")))]
async fn create(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<ResourceRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let _g = s.operation_lock.lock().await;
    let (owner, cid) = authorize_canvas(&s, &as_caller(&u), &id, Need::Read).await?;
    // Creation is an owner operation, not a way for administrators to bill other users.
    if owner != u.uid {
        return Err(AppError::Forbidden);
    }
    jobs::sweep(&s.store, crate::util::now_secs())
        .await
        .map_err(error)?;
    if let Some(raw) = s
        .store
        .get(Cf::Idx, keys::job_request_key(u.uid, &r.request_id))
        .await?
    {
        let existing = jobs::get(
            &s.store,
            std::str::from_utf8(&raw).map_err(|_| AppError::NotFound)?,
        )
        .await
        .map_err(error)?;
        if existing.canvas_id != cid || existing.request != r {
            return Err(AppError::Conflict("request_id 已用于另一操作".into()));
        }
        return Ok((StatusCode::OK, Json(existing.public())));
    }
    let now = crate::util::now_secs();
    let job = jobs::prepare(&s.store, owner, cid, r, now)
        .await
        .map_err(error)?;
    let mut b = Batch::new();
    if let Some(task_queue::PushOutcome::LimitReached {
        used,
        limit,
        next_reset_at,
    }) = task_queue::stage_submission(&s.store, &mut b, owner, &s.config.plans, now).await?
    {
        return Err(AppError::MonthlyPushLimit(format!(
            "本周期 {used}/{limit}，重置时间 {next_reset_at}"
        )));
    }
    jobs::stage_job(&mut b, &job).map_err(error)?;
    b.put(
        Cf::Idx,
        keys::job_request_key(owner, &job.request.request_id),
        job.job_id.as_bytes(),
    );
    s.store.write(b).await?;
    Ok((StatusCode::CREATED, Json(job.public())))
}
#[derive(Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct Filter {
    canvas_id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    active: Option<bool>,
    before: Option<String>,
}
#[utoipa::path(get,path="/jobs",tag="jobs",params(Filter),security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn list(
    State(s): State<AppState>,
    u: UserCaller,
    Query(q): Query<Filter>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    jobs::sweep(&s.store, crate::util::now_secs())
        .await
        .map_err(error)?;
    if u.is_admin() {
        tracing::info!(audit=true, admin=%u.uid, action="jobs.list", "管理员查询资源任务");
    }
    let cid = q
        .canvas_id
        .as_deref()
        .map(crate::storage::keys::CanvasId::parse)
        .transpose()
        .map_err(|_| AppError::BadRequest("画布 ID 无效".into()))?;
    if let Some(before) = &q.before {
        TaskId::parse(before).map_err(|_| AppError::BadRequest("分页游标无效".into()))?;
    }
    let mut rows = if q
        .kind
        .as_deref()
        .is_some_and(|k| !["resource.regenerate", "mp4.export"].contains(&k))
    {
        vec![]
    } else {
        jobs::page_kind(
            &s.store,
            if u.is_admin() { None } else { Some(u.uid) },
            cid,
            q.before.as_deref(),
            q.active.unwrap_or(false),
            101,
            q.kind.as_deref(),
        )
        .await
        .map_err(error)?
    };
    let more = rows.len() > 100;
    rows.truncate(100);
    let next = if more {
        rows.last().map(|j| j.job_id.clone())
    } else {
        None
    };
    Ok(Json(
        json!({"jobs":rows.iter().map(|j|j.public()).collect::<Vec<_>>(),"next_before":next}),
    ))
}
#[utoipa::path(get,path="/jobs/{id}",tag="jobs",security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn get(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    jobs::sweep(&s.store, crate::util::now_secs())
        .await
        .map_err(error)?;
    let j = jobs::get(&s.store, &id).await.map_err(error)?;
    if j.owner_uid != u.uid && !u.is_admin() {
        return Err(AppError::NotFound);
    }
    if u.is_admin() && j.owner_uid != u.uid {
        super::auth::audit_admin_action(u.uid, "jobs.get", j.owner_uid, &id);
    }
    let mut result = j.public();
    if u.is_admin() {
        if let Some(receipt) = j.input.get("provider_receipt") {
            result["provider_receipt"] = receipt.clone();
        }
    }
    Ok(Json(result))
}
#[utoipa::path(get,path="/canvases/{id}/resource-state",tag="jobs",security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn state(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let (_, cid) = authorize_canvas(&s, &as_caller(&u), &id, Need::Read).await?;
    let mut v = match s
        .store
        .get(Cf::Meta, keys::resource_version_key(cid))
        .await?
    {
        Some(raw) => serde_json::from_slice(&raw).map_err(anyhow::Error::from)?,
        None => json!({"exports_stale":false}),
    };
    let export = crate::domain::mp4_jobs::state(&s.store, cid).await?;
    v["exports_stale"] = export["exports_stale"].clone();
    v["mp4_export"] = export["published"].clone();
    Ok(Json(v))
}
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct Claim {
    accepted_types: Vec<String>,
    #[serde(default)]
    exclude_project_ids: Vec<String>,
}
#[utoipa::path(post,path="/internal/jobs/claim",tag="jobs",request_body=Claim,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn claim(
    State(s): State<AppState>,
    u: UserCaller,
    Json(r): Json<Claim>,
) -> AppResult<Json<Value>> {
    super::task_api::authorize_reader(&s, &u).await?;
    if r.accepted_types.is_empty()
        || r.accepted_types
            .iter()
            .any(|k| !["resource.regenerate", "mp4.export"].contains(&k.as_str()))
    {
        return Err(AppError::BadRequest("不支持的任务能力".into()));
    }
    let _g = s.operation_lock.lock().await;
    if r.exclude_project_ids.len() > 100 {
        return Err(AppError::BadRequest("项目排除列表过长".into()));
    }
    let excluded: Vec<_> = r
        .exclude_project_ids
        .iter()
        .map(|id| ProjectId::parse(id))
        .collect::<Result<_, _>>()
        .map_err(|_| AppError::BadRequest("项目 ID 无效".into()))?;
    let job = jobs::claim_types(
        &s.store,
        u.uid,
        crate::util::now_secs(),
        &excluded,
        &r.accepted_types,
    )
    .await
    .map_err(error)?;
    Ok(Json(json!({"job":job})))
}
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct Operation {
    lease_token: String,
    lease_epoch: u64,
    stage: Option<String>,
    error: Option<String>,
    parameters: Option<Value>,
    provider_receipt: Option<Value>,
}
async fn leased(
    s: &AppState,
    u: &UserCaller,
    id: &str,
    r: &Operation,
) -> AppResult<jobs::JobRecord> {
    super::task_api::authorize_reader(s, u).await?;
    let job = jobs::get(&s.store, id).await.map_err(error)?;
    jobs::verify_lease(
        &s.store,
        &job,
        u.uid,
        &r.lease_token,
        r.lease_epoch,
        crate::util::now_secs(),
    )
    .await
    .map_err(error)?;
    Ok(job)
}
#[utoipa::path(post,path="/internal/jobs/{id}/heartbeat",tag="jobs",request_body=Operation,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn heartbeat(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<Operation>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let mut j = leased(&s, &u, &id, &r).await?;
    jobs::heartbeat(&s.store, &mut j, crate::util::now_secs())
        .await
        .map_err(error)?;
    Ok(Json(json!({"expires_at":j.lease.unwrap().expires_at})))
}
#[utoipa::path(post,path="/internal/jobs/{id}/checkpoint",tag="jobs",request_body=Operation,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn checkpoint(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<Operation>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let mut j = leased(&s, &u, &id, &r).await?;
    if r.stage.as_deref() != Some("generating")
        || !["queued", "preparing"].contains(&j.stage.as_str())
    {
        return Err(AppError::Conflict("生成阶段不可重复提交".into()));
    }
    jobs::assert_inputs(&s.store, &j).await.map_err(error)?;
    // Persist before the provider call. A crash from here until candidate upload is ambiguous.
    j.stage = "generating".into();
    j.updated_at = crate::util::now_secs();
    if let Some(p) = r.parameters {
        if serde_json::to_vec(&p).map_err(anyhow::Error::from)?.len() > 16384 {
            return Err(AppError::PayloadTooLarge);
        }
        j.input["generation_parameters"] = p;
    }
    let mut b = Batch::new();
    jobs::stage_job(&mut b, &j).map_err(error)?;
    s.store.write(b).await?;
    Ok(Json(j.public()))
}
#[utoipa::path(get,path="/internal/jobs/{id}/inputs/{index}",tag="jobs",security(("bearer_jwt"=[])),responses((status=200,description="冻结输入文件字节")))]
async fn input(
    State(s): State<AppState>,
    u: UserCaller,
    Path((id, index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> AppResult<Bytes> {
    let r = header_lease(&headers)?;
    let j = leased(&s, &u, &id, &r).await?;
    let file = j.dependencies.get(index).ok_or(AppError::NotFound)?;
    let hash = ContentHash::parse_hex(&file.hash).map_err(anyhow::Error::from)?;
    let bytes = crate::storage::blob::read_all(&s.store, hash)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Bytes::from(bytes))
}
fn header_lease(h: &HeaderMap) -> AppResult<Operation> {
    let token = h
        .get("x-job-lease")
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthorized)?;
    let epoch = h
        .get("x-job-epoch")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or(AppError::Unauthorized)?;
    Ok(Operation {
        lease_token: token.into(),
        lease_epoch: epoch,
        stage: None,
        error: None,
        parameters: None,
        provider_receipt: None,
    })
}
#[derive(Deserialize, utoipa::ToSchema)]
struct Media {
    extension: String,
    audio_duration_seconds: Option<f64>,
}
#[utoipa::path(put,path="/internal/jobs/{id}/candidate",tag="jobs",request_body(content=Vec<u8>,content_type="application/octet-stream"),security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn candidate(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Query(m): Query<Media>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let r = header_lease(&headers)?;
    let mut j = leased(&s, &u, &id, &r).await?;
    if !["generating", "uploaded"].contains(&j.stage.as_str()) {
        return Err(AppError::Conflict("任务不在上传阶段".into()));
    }
    let valid = match (j.request.resource_type.as_str(), m.extension.as_str()) {
        ("image", "png") => body.starts_with(b"\x89PNG\r\n\x1a\n"),
        ("image", "jpg") => body.starts_with(b"\xff\xd8\xff"),
        ("image", "webp") => body.starts_with(b"RIFF") && body.get(8..12) == Some(b"WEBP"),
        ("audio", "mp3") => {
            (body.starts_with(b"ID3") || (body.len() > 1 && body[0] == 255 && body[1] & 224 == 224))
                && m.audio_duration_seconds
                    .is_some_and(|d| d.is_finite() && d > 0. && d < 86400.)
        }
        _ => false,
    };
    if !valid || body.len() < 16 {
        return Err(AppError::BadRequest("媒体格式或时长无效".into()));
    }
    let hash = blake3::hash(&body).to_hex().to_string();
    if let Some(c) = &j.candidate {
        if c["hash"] != hash || c["audio_duration_seconds"] != json!(m.audio_duration_seconds) {
            return Err(AppError::Conflict("候选资源不可覆盖".into()));
        }
        return Ok(Json(c.clone()));
    }
    let path = format!(
        "{}/scene-{:04}-regen-{}.{}",
        if j.request.resource_type == "image" {
            "images"
        } else {
            "audio"
        },
        j.request.scene_index + 1,
        j.job_id,
        m.extension
    );
    let mut b = Batch::new();
    let out = crate::domain::canvas::stage_put_file(
        &s.store,
        &mut b,
        j.owner_uid,
        j.canvas_id,
        &path,
        &crate::domain::canvas::mime_for_path(&path),
        &body,
        crate::util::now_secs(),
    )
    .await?
    .map_err(super::file_api::map_canvas_error_pub)?;
    j.candidate = Some(
        json!({"path":path,"hash":out.hash.to_hex(),"size":out.size,"audio_duration_seconds":m.audio_duration_seconds}),
    );
    j.stage = "uploaded".into();
    j.updated_at = crate::util::now_secs();
    jobs::stage_job(&mut b, &j).map_err(error)?;
    s.store.write(b).await?;
    Ok(Json(j.candidate.unwrap()))
}
#[utoipa::path(post,path="/internal/jobs/{id}/commit",tag="jobs",request_body=Operation,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn commit(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<Operation>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    super::task_api::authorize_reader(&s, &u).await?;
    let existing = jobs::get(&s.store, &id).await.map_err(error)?;
    if existing.status == "succeeded"
        && existing.lease.as_ref().is_some_and(|l| {
            l.worker_uid == u.uid && l.token == r.lease_token && l.epoch == r.lease_epoch
        })
    {
        return Ok(Json(existing.public()));
    }
    let mut j = leased(&s, &u, &id, &r).await?;
    jobs::commit(&s.store, &mut j, crate::util::now_secs())
        .await
        .map_err(error)?;
    Ok(Json(j.public()))
}
#[utoipa::path(post,path="/internal/jobs/{id}/fail",tag="jobs",request_body=Operation,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn fail(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<Operation>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let mut j = leased(&s, &u, &id, &r).await?;
    let code = r.error.as_deref().unwrap_or("resource_generation_failed");
    if code.len() > 100 || !code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(AppError::BadRequest("错误码无效".into()));
    }
    if let Some(receipt) = r.provider_receipt {
        let fields = receipt
            .as_object()
            .ok_or_else(|| AppError::BadRequest("供应商回执无效".into()))?;
        if fields.iter().any(|(key, value)| {
            !["task_id", "idempotency_key"].contains(&key.as_str())
                || value.as_str().is_none_or(|s| {
                    s.is_empty()
                        || s.len() > 200
                        || !s
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_:-".contains(&b))
                })
        }) {
            return Err(AppError::BadRequest("供应商回执无效".into()));
        }
        j.input["provider_receipt"] = receipt;
    }
    jobs::fail(&s.store, &mut j, code, crate::util::now_secs())
        .await
        .map_err(error)?;
    Ok(Json(j.public()))
}
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct LegacyLease {
    task_id: String,
    agent_token: Option<String>,
    lease_token: Option<String>,
}
#[utoipa::path(post,path="/internal/projects/{id}/execution-lease",tag="jobs",request_body=LegacyLease,security(("bearer_jwt"=[])),responses((status=200,body=Value),(status=409,description="凭证已失效"),(status=423,description="项目执行权被占用")))]
async fn execution(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<LegacyLease>,
) -> AppResult<Json<Value>> {
    super::task_api::authorize_reader(&s, &u).await?;
    let _g = s.operation_lock.lock().await;
    let pid = ProjectId::parse(&id).map_err(|_| AppError::NotFound)?;
    TaskId::parse(&r.task_id).map_err(|_| AppError::BadRequest("task_id 无效".into()))?;
    let token = r.agent_token.as_deref().ok_or(AppError::Unauthorized)?;
    let now = crate::util::now_secs();
    let a = crate::domain::agent::verify(&s.store, token, now)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if a.project_id != pid {
        return Err(AppError::Forbidden);
    }
    let hash = ContentHash::from_bytes(crate::util::sha256(token.as_bytes())).to_hex();
    if let Some(e) = jobs::execution(&s.store, pid).await?
        && e.lease.expires_at > now
    {
        if e.holder == r.task_id
            && e.agent_token_hash.as_deref() == Some(&hash)
            && e.lease.worker_uid == u.uid
        {
            return Ok(Json(json!(e.lease)));
        }
        return Err(AppError::ProjectExecutionBusy);
    }
    if s.store
        .get(Cf::Meta, keys::execution_token_key(&hash))
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(
            "旧制作凭证已失去执行权，请重新提交任务".into(),
        ));
    }
    let l = jobs::Lease {
        token: TaskId::new().to_string(),
        epoch: 1,
        expires_at: now + jobs::LEASE_SECONDS,
        worker_uid: u.uid,
    };
    let e = jobs::Execution {
        owner_uid: a.uid,
        holder: r.task_id,
        lease: l.clone(),
        agent_token_hash: Some(hash.clone()),
    };
    let mut b = Batch::new();
    b.put(
        Cf::Meta,
        keys::project_execution_key(pid),
        serde_json::to_vec(&e).map_err(anyhow::Error::from)?,
    );
    b.put(
        Cf::Meta,
        keys::execution_token_key(&hash),
        l.token.as_bytes(),
    );
    s.store.write(b).await?;
    Ok(Json(json!(l)))
}
async fn change_execution(
    s: &AppState,
    u: &UserCaller,
    id: &str,
    r: LegacyLease,
    release: bool,
) -> AppResult<Value> {
    super::task_api::authorize_reader(s, u).await?;
    let pid = ProjectId::parse(id).map_err(|_| AppError::NotFound)?;
    let mut e = jobs::execution(&s.store, pid)
        .await
        .map_err(error)?
        .ok_or(AppError::Conflict("执行权已失效".into()))?;
    let now = crate::util::now_secs();
    if e.agent_token_hash.is_none()
        || e.holder != r.task_id
        || e.lease.worker_uid != u.uid
        || Some(&e.lease.token) != r.lease_token.as_ref()
        || e.lease.expires_at <= now
    {
        return Err(AppError::Conflict("执行权已失效".into()));
    }
    if release {
        s.store
            .delete(Cf::Meta, keys::project_execution_key(pid))
            .await?;
    } else {
        e.lease.expires_at = now + jobs::LEASE_SECONDS;
        s.store
            .put(
                Cf::Meta,
                keys::project_execution_key(pid),
                serde_json::to_vec(&e).map_err(anyhow::Error::from)?,
            )
            .await?;
    }
    Ok(json!(e.lease))
}
#[utoipa::path(post,path="/internal/projects/{id}/execution-lease/heartbeat",tag="jobs",request_body=LegacyLease,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn execution_heartbeat(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<LegacyLease>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    Ok(Json(change_execution(&s, &u, &id, r, false).await?))
}
#[utoipa::path(post,path="/internal/projects/{id}/execution-lease/release",tag="jobs",request_body=LegacyLease,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn execution_release(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<LegacyLease>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    Ok(Json(change_execution(&s, &u, &id, r, true).await?))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct ExportRequest {
    request_id: String,
    expected_storyboard_hash: String,
}
#[utoipa::path(post,path="/canvases/{id}/mp4-jobs",tag="jobs",request_body=ExportRequest,security(("bearer_jwt"=[])),responses((status=201,body=Value),(status=200,body=Value)))]
async fn create_export(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<ExportRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let _g = s.operation_lock.lock().await;
    let (owner, cid) = authorize_canvas(&s, &as_caller(&u), &id, Need::Read).await?;
    if owner != u.uid {
        return Err(AppError::Forbidden);
    }
    let request = ResourceRequest {
        request_id: r.request_id,
        expected_storyboard_hash: r.expected_storyboard_hash,
        resource_type: "mp4".into(),
        scene_index: 0,
    };
    jobs::sweep(&s.store, crate::util::now_secs())
        .await
        .map_err(error)?;
    if let Some(raw) = s
        .store
        .get(Cf::Idx, keys::job_request_key(owner, &request.request_id))
        .await?
    {
        let j = jobs::get(
            &s.store,
            std::str::from_utf8(&raw).map_err(|_| AppError::NotFound)?,
        )
        .await
        .map_err(error)?;
        if j.kind != "mp4.export" || j.canvas_id != cid || j.request != request {
            return Err(AppError::Conflict("request_id 已用于另一操作".into()));
        }
        return Ok((StatusCode::OK, Json(j.public())));
    }
    let j =
        crate::domain::mp4_jobs::prepare(&s.store, owner, cid, request, crate::util::now_secs())
            .await
            .map_err(error)?;
    let mut b = Batch::new();
    jobs::stage_job(&mut b, &j).map_err(error)?;
    // Exporting does not call a paid generation provider or spend generation submissions.
    b.put(
        Cf::Idx,
        keys::job_request_key(owner, &j.request.request_id),
        j.job_id.as_bytes(),
    );
    s.store.write(b).await?;
    Ok((StatusCode::CREATED, Json(j.public())))
}
#[derive(Deserialize, utoipa::ToSchema)]
struct ExportProgress {
    lease_token: String,
    lease_epoch: u64,
    progress: Value,
}
#[utoipa::path(post,path="/internal/jobs/{id}/export-progress",tag="jobs",request_body=ExportProgress,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn export_progress(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    Json(r): Json<ExportProgress>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let op = Operation {
        lease_token: r.lease_token,
        lease_epoch: r.lease_epoch,
        stage: None,
        error: None,
        parameters: None,
        provider_receipt: None,
    };
    let mut j = leased(&s, &u, &id, &op).await?;
    if j.kind != "mp4.export"
        || serde_json::to_vec(&r.progress)
            .map_err(anyhow::Error::from)?
            .len()
            > 65536
    {
        return Err(AppError::BadRequest("导出进度无效".into()));
    }
    j.input["export_progress"] = r.progress;
    j.stage = "exporting".into();
    j.updated_at = crate::util::now_secs();
    let mut b = Batch::new();
    jobs::stage_job(&mut b, &j).map_err(error)?;
    s.store.write(b).await?;
    Ok(Json(j.public()))
}
#[derive(Deserialize, utoipa::ToSchema)]
struct ExportVariant {
    variant: String,
}
async fn leased_export(
    s: &AppState,
    u: &UserCaller,
    id: &str,
    headers: &HeaderMap,
    variant: &str,
) -> AppResult<jobs::JobRecord> {
    crate::domain::mp4_jobs::variant_path(variant).map_err(error)?;
    let j = leased(s, u, id, &header_lease(headers)?).await?;
    if j.kind != "mp4.export" {
        return Err(AppError::BadRequest("任务不是 MP4 导出".into()));
    }
    Ok(j)
}
#[utoipa::path(post,path="/internal/jobs/{id}/export-upload",tag="jobs",request_body=ExportVariant,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn export_upload(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(r): Json<ExportVariant>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let mut j = leased_export(&s, &u, &id, &headers, &r.variant).await?;
    if let Some(c) = j.candidate.as_ref().and_then(|c| c.get(&r.variant)) {
        return Ok(Json(json!({"completed":c})));
    }
    if let Some(session) = j.input.get("uploads").and_then(|v| v.get(&r.variant)) {
        return Ok(Json(session.clone()));
    }
    let path = crate::domain::mp4_jobs::candidate_path(&j, &r.variant).map_err(error)?;
    let session = crate::domain::upload::initiate(
        &s.store,
        j.owner_uid,
        j.canvas_id,
        &path,
        None,
        crate::util::now_secs(),
    )
    .await?
    .map_err(|e| AppError::BadRequest(format!("MP4 上传初始化失败: {e:?}")))?;
    let result = json!({"upload_id":session.upload_id.to_string(),"part_size":session.part_size});
    if !j.input["uploads"].is_object() {
        j.input["uploads"] = json!({});
    }
    j.input["uploads"][&r.variant] = result.clone();
    let mut b = Batch::new();
    jobs::stage_job(&mut b, &j).map_err(error)?;
    s.store.write(b).await?;
    Ok(Json(result))
}
fn export_session(j: &jobs::JobRecord, variant: &str) -> AppResult<keys::UploadId> {
    keys::UploadId::parse(
        j.input["uploads"][variant]["upload_id"]
            .as_str()
            .ok_or(AppError::NotFound)?,
    )
    .map_err(|_| AppError::NotFound)
}
#[utoipa::path(put,path="/internal/jobs/{id}/export-parts/{variant}/{part}",tag="jobs",request_body(content=Vec<u8>,content_type="application/octet-stream"),security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn export_part(
    State(s): State<AppState>,
    u: UserCaller,
    Path((id, variant, part)): Path<(String, String, u32)>,
    headers: HeaderMap,
    body: Bytes,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let j = leased_export(&s, &u, &id, &headers, &variant).await?;
    if j.candidate
        .as_ref()
        .is_some_and(|c| c.get(&variant).is_some())
    {
        return Err(AppError::Conflict("候选已完成".into()));
    }
    if part == 0 && (body.len() < 12 || body.get(4..8) != Some(b"ftyp")) {
        return Err(AppError::BadRequest("MP4 格式无效".into()));
    }
    crate::domain::upload::put_part(
        &s.store,
        export_session(&j, &variant)?,
        part,
        &body,
        crate::util::now_secs(),
    )
    .await?
    .map_err(|e| AppError::BadRequest(format!("MP4 分片无效: {e:?}")))?;
    Ok(Json(json!({"accepted":true})))
}
#[utoipa::path(post,path="/internal/jobs/{id}/export-complete",tag="jobs",request_body=ExportVariant,security(("bearer_jwt"=[])),responses((status=200,body=Value)))]
async fn export_complete(
    State(s): State<AppState>,
    u: UserCaller,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(r): Json<ExportVariant>,
) -> AppResult<Json<Value>> {
    let _g = s.operation_lock.lock().await;
    let mut j = leased_export(&s, &u, &id, &headers, &r.variant).await?;
    if let Some(c) = j.candidate.as_ref().and_then(|v| v.get(&r.variant)) {
        return Ok(Json(c.clone()));
    }
    let path = crate::domain::mp4_jobs::candidate_path(&j, &r.variant).map_err(error)?;
    // A completed upload may have committed just before the worker lost the response.
    if crate::domain::canvas::get_file(&s.store, j.canvas_id, &path)
        .await?
        .is_none()
    {
        crate::domain::upload::complete(
            &s.store,
            j.owner_uid,
            export_session(&j, &r.variant)?,
            crate::util::now_secs(),
        )
        .await?
        .map_err(|e| AppError::BadRequest(format!("MP4 上传未完成: {e:?}")))?;
    }
    let entry = crate::domain::canvas::get_file(&s.store, j.canvas_id, &path)
        .await?
        .ok_or(AppError::NotFound)?;
    let c = crate::domain::space::get_canvas(&s.store, j.owner_uid, j.canvas_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let result = json!({"path":path,"hash":entry.hash.to_hex(),"size":entry.size,"url":format!("/f/{}/{}?v={}",c.slug,path,entry.hash.to_hex())});
    if j.candidate.is_none() {
        j.candidate = Some(json!({}));
    }
    j.candidate.as_mut().unwrap()[&r.variant] = result.clone();
    j.updated_at = crate::util::now_secs();
    let mut b = Batch::new();
    jobs::stage_job(&mut b, &j).map_err(error)?;
    s.store.write(b).await?;
    Ok(Json(result))
}
