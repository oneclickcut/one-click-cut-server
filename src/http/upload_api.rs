//! 分块上传四件套 + tar 批量上传。
//!
//! 这两条都是**为 agent 准备的通道**，不是给人在页面上点的：
//!
//! - **批量接口是必需的，不是优化。** 一个画布 350 个文件，350 次 HTTP 往返
//!   光 TLS 与 RTT 就是几十秒。
//! - **分块上传是 mp4 的唯一通道。** Cloudflare 免费版单请求体上限 100MB，
//!   而且家宽上行断一次就得全部重传。

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::AppState;
use super::auth::Caller;
use super::error::{AppError, AppResult};
use super::file_api::{Need, authorize_canvas};
use crate::domain::canvas;
use crate::domain::upload::{self, UploadError};
use crate::storage::keys::UploadId;
use crate::util;

pub fn router(max_part_bytes: usize) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(initiate_upload))
        .routes(routes!(upload_status, complete_upload, abort_upload))
        // 分片与 tar 批量的 body 都很大，各自单独设上限；
        // 两条都用手工 route（tar 与分片都不是 JSON body，且 path 有通配需求）。
        .route(
            "/canvases/{id}/uploads/{upload_id}/{part_no}",
            axum::routing::put(put_part)
                .layer(axum::extract::DefaultBodyLimit::max(max_part_bytes)),
        )
        .route(
            "/canvases/{id}/files:batch",
            axum::routing::post(batch_upload)
                .layer(axum::extract::DefaultBodyLimit::max(max_part_bytes)),
        )
}

fn map_upload_error(e: UploadError) -> AppError {
    match e {
        UploadError::QuotaExceeded => AppError::InsufficientStorage("用户配额不足".into()),
        UploadError::CanvasNotFound | UploadError::SessionNotFound => AppError::NotFound,
        UploadError::PartOutOfRange => {
            AppError::BadRequest(format!("分片号必须小于 {}", upload::MAX_PARTS))
        }
        UploadError::BadPartSize(msg) => AppError::BadRequest(msg),
        UploadError::MissingParts(missing) => {
            AppError::BadRequest(format!("缺少分片 {missing:?}，补齐后再调 complete"))
        }
        UploadError::NoParts => AppError::BadRequest("一个分片都没上传".into()),
        // 不变量 1：同一个路径不能同时是文件和目录。与 PUT 那条走同一个 409。
        UploadError::PathConflict(c) => AppError::Conflict(match c {
            canvas::PathConflict::TargetIsDir => "这个路径已经是一个目录".to_owned(),
            canvas::PathConflict::AncestorIsFile => {
                "路径上有一段已经是文件，同一个名字不能既是文件又是目录".to_owned()
            }
        }),
    }
}

// ---------------------------------------------------------------------------
// DTO
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct InitiateRequest {
    /// 目标路径。
    #[schema(example = "final.mp4")]
    pub path: String,
    /// 分片大小。省略即 16MB。**必须是 4MB 的整数倍**，见响应说明。
    pub part_size: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct InitiateResponse {
    pub upload_id: String,
    /// 客户端应当按这个大小切分，**只有最后一个分片可以短**。
    ///
    /// 它必须是存储层块大小（4MB）的整数倍：不对齐会在文件中间产生短块，
    /// 破坏 Range 读取按偏移索引块的前提。
    #[schema(example = 16777216_u32)]
    pub part_size: u32,
    /// 会话超时时刻。超时未 complete 的会话会被清理。
    pub expires_at: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PartResponse {
    pub part_no: u32,
    pub size: u64,
    /// 这个分片被切成了几个 4MB 块。
    pub chunks: usize,
    /// 这个分片之前已经传过（重传是幂等的）。
    pub duplicate: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct StatusResponse {
    pub path: String,
    pub part_size: u32,
    /// 已收到的分片号，升序。断点续传时据此只补缺的那些。
    pub parts: Vec<u32>,
    pub bytes_received: u64,
    pub expires_at: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CompleteResponse {
    pub path: String,
    pub hash: String,
    pub size: u64,
    #[schema(example = "final.mp4?v=1a2b3c4d")]
    pub url: String,
    pub replaced: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BatchResponse {
    /// 成功写入的文件数。
    pub written: u64,
    /// 总字节数。
    pub bytes: u64,
    /// 命中去重、没有写入新字节的文件数。
    pub deduplicated: u64,
}

fn parse_upload_id(raw: &str) -> AppResult<UploadId> {
    UploadId::parse(raw).map_err(|_| AppError::NotFound)
}

async fn check_upload_canvas(
    state: &AppState,
    upload_id: UploadId,
    canvas_id: crate::storage::keys::CanvasId,
) -> AppResult<()> {
    let raw = state
        .store
        .get(
            crate::storage::Cf::Stage,
            crate::storage::keys::stage_info_key(upload_id),
        )
        .await?
        .ok_or(AppError::NotFound)?;
    let info: crate::storage::records::StageInfo =
        crate::storage::records::decode(&raw).map_err(AppError::Internal)?;
    if info.canvas_id != canvas_id {
        return Err(AppError::NotFound);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 分块上传
// ---------------------------------------------------------------------------

/// 初始化分块上传
///
/// 32MB 以上的内容必须走这条通道：Cloudflare 免费版单请求体上限 100MB，
/// 而且分块顺带提供断点续传。
#[utoipa::path(
    summary = "创建分块上传会话",
    post,
    path = "/canvases/{id}/uploads",
    tag = "agent",
    params(("id" = String, Path, description = "画布 id")),
    request_body = InitiateRequest,
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 201, description = "会话已建立", body = InitiateResponse),
        (status = 400, description = "路径或 part_size 不合规", body = super::error::ErrorBody),
        (status = 403, description = "agent token 没有写权限", body = super::error::ErrorBody),
        (status = 404, description = "画布不存在或不属于调用者", body = super::error::ErrorBody),
        (status = 409, description = "目标路径已经是一个目录", body = super::error::ErrorBody),
    ),
)]
async fn initiate_upload(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    Json(body): Json<InitiateRequest>,
) -> AppResult<(StatusCode, Json<InitiateResponse>)> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Write).await?;

    super::file_api::check_storyboard_write(&state, canvas_id, &body.path, None).await?;
    let initiated = upload::initiate(
        &state.store,
        uid,
        canvas_id,
        &body.path,
        body.part_size,
        util::now_secs(),
    )
    .await
    .map_err(|e| AppError::BadRequest(e.to_string()))?
    .map_err(map_upload_error)?;

    Ok((
        StatusCode::CREATED,
        Json(InitiateResponse {
            upload_id: initiated.upload_id.to_text(),
            part_size: initiated.part_size,
            expires_at: initiated.expires_at,
        }),
    ))
}

/// 上传一个分片
///
/// body 是原始字节。分片可以乱序上传，重传同一个分片是幂等的。
#[utoipa::path(
    summary = "上传文件分块",
    put,
    // 手工登记进 ApiDoc，所以写完整路径。
    path = "/api/canvases/{id}/uploads/{upload_id}/{part_no}",
    tag = "agent",
    params(
        ("id" = String, Path, description = "画布 id"),
        ("upload_id" = String, Path, description = "会话 id"),
        ("part_no" = u32, Path, description = "分片号，从 0 开始"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "分片已收下", body = PartResponse),
        (status = 400, description = "分片号或大小不合规", body = super::error::ErrorBody),
        (status = 404, description = "会话不存在或已过期", body = super::error::ErrorBody),
        (status = 413, description = "分片超过上限", body = super::error::ErrorBody),
    ),
)]
pub async fn put_part(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, upload_id, part_no)): Path<(String, String, u32)>,
    body: Bytes,
) -> AppResult<Json<PartResponse>> {
    let _guard = state.operation_lock.lock().await;
    let (_, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Write).await?;
    let upload_id = parse_upload_id(&upload_id)?;
    check_upload_canvas(&state, upload_id, canvas_id).await?;

    let accepted = upload::put_part(&state.store, upload_id, part_no, &body, util::now_secs())
        .await?
        .map_err(map_upload_error)?;

    Ok(Json(PartResponse {
        part_no: accepted.part_no,
        size: accepted.size,
        chunks: accepted.chunks,
        duplicate: accepted.duplicate,
    }))
}

/// 查上传进度
///
/// 断点续传用：返回已收到的分片号，客户端只补缺的那些。
#[utoipa::path(
    summary = "查询分块上传进度",
    get,
    path = "/canvases/{id}/uploads/{upload_id}",
    tag = "agent",
    params(
        ("id" = String, Path, description = "画布 id"),
        ("upload_id" = String, Path, description = "会话 id"),
    ),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "当前进度", body = StatusResponse),
        (status = 404, description = "会话不存在或已过期", body = super::error::ErrorBody),
    ),
)]
async fn upload_status(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, upload_id)): Path<(String, String)>,
) -> AppResult<Json<StatusResponse>> {
    let (_, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Read).await?;
    let upload_id = parse_upload_id(&upload_id)?;
    check_upload_canvas(&state, upload_id, canvas_id).await?;

    let st = upload::status(&state.store, upload_id, util::now_secs())
        .await?
        .map_err(map_upload_error)?;

    Ok(Json(StatusResponse {
        path: st.path,
        part_size: st.part_size,
        parts: st.parts,
        bytes_received: st.bytes_received,
        expires_at: st.expires_at,
    }))
}

/// 完成分块上传
///
/// 按分片号顺序拼成块列表，提交 blob 与 FileEntry。整文件哈希由服务端从落盘的
/// 块重新算出——**不信客户端给的哈希**。
#[utoipa::path(
    summary = "合并并提交分块上传",
    post,
    path = "/canvases/{id}/uploads/{upload_id}/complete",
    tag = "agent",
    params(
        ("id" = String, Path, description = "画布 id"),
        ("upload_id" = String, Path, description = "会话 id"),
    ),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "已提交", body = CompleteResponse),
        (status = 400, description = "分片缺号或大小不符", body = super::error::ErrorBody),
        (status = 404, description = "会话不存在或已过期", body = super::error::ErrorBody),
        (status = 409, description = "目标路径已经是一个目录", body = super::error::ErrorBody),
    ),
)]
async fn complete_upload(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, upload_id)): Path<(String, String)>,
) -> AppResult<Json<CompleteResponse>> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Write).await?;
    let upload_id = parse_upload_id(&upload_id)?;
    check_upload_canvas(&state, upload_id, canvas_id).await?;

    let info = upload::status(&state.store, upload_id, util::now_secs())
        .await?
        .map_err(map_upload_error)?;
    super::file_api::check_storyboard_write(&state, canvas_id, &info.path, None).await?;
    let done = upload::complete(&state.store, uid, upload_id, util::now_secs())
        .await?
        .map_err(map_upload_error)?;

    Ok(Json(CompleteResponse {
        url: format!("{}?v={}", done.path, done.hash.to_hex8()),
        path: done.path,
        hash: done.hash.to_hex(),
        size: done.size,
        replaced: done.replaced,
    }))
}

/// 放弃分块上传
///
/// 只清会话记录。已写入的块可能被别的文件引用（内容寻址），没人引用的那些
/// 由 GC 的孤儿扫描回收。
#[utoipa::path(
    summary = "取消分块上传",
    delete,
    path = "/canvases/{id}/uploads/{upload_id}",
    tag = "agent",
    params(
        ("id" = String, Path, description = "画布 id"),
        ("upload_id" = String, Path, description = "会话 id"),
    ),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 204, description = "已放弃"),
        (status = 404, description = "会话不存在", body = super::error::ErrorBody),
    ),
)]
async fn abort_upload(
    State(state): State<AppState>,
    caller: Caller,
    Path((id, upload_id)): Path<(String, String)>,
) -> AppResult<StatusCode> {
    let _guard = state.operation_lock.lock().await;
    let (_, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Write).await?;
    let upload_id = parse_upload_id(&upload_id)?;
    check_upload_canvas(&state, upload_id, canvas_id).await?;

    upload::abort(&state.store, upload_id, util::now_secs())
        .await?
        .map_err(map_upload_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// tar 批量上传
// ---------------------------------------------------------------------------

/// 批量上传（tar 流）
///
/// body 是一个未压缩的 tar。一次请求写入几百个小文件，避免 350 次 HTTP 往返。
///
/// **整批原子**：tar 里任何一个条目的路径不合规，整批拒绝且一个文件都不写入。
/// 这样 agent 不用去猜「哪几个成功了」。
#[utoipa::path(
    summary = "批量上传 tar 文件",
    post,
    // 手工登记进 ApiDoc，所以写完整路径。
    path = "/api/canvases/{id}/files:batch",
    tag = "agent",
    params(("id" = String, Path, description = "画布 id")),
    request_body(content = Vec<u8>, content_type = "application/x-tar"),
    security(("bearer_jwt" = []), ("agent_token" = [])),
    responses(
        (status = 200, description = "全部写入", body = BatchResponse),
        (status = 400, description = "tar 解析失败，或含不合规的路径（整批拒绝）", body = super::error::ErrorBody),
        (status = 404, description = "画布不存在或不属于调用者", body = super::error::ErrorBody),
        (status = 409, description = "超出单画布文件数上限", body = super::error::ErrorBody),
        (status = 413, description = "请求体超过上限", body = super::error::ErrorBody),
    ),
)]
pub async fn batch_upload(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    body: Bytes,
) -> AppResult<Json<BatchResponse>> {
    let _guard = state.operation_lock.lock().await;
    let (uid, canvas_id) = authorize_canvas(&state, &caller, &id, Need::Write).await?;

    // ==== 先全解一遍再写 ====
    //
    // 解 tar 与写入分成两步，是为了「整批原子」：第 300 个条目路径非法时，
    // 前 299 个也不该落盘。代价是整个 tar 要进内存——但批量接口本来就是给
    // 「几百个小文件」用的，大文件走分块通道。
    let entries = parse_tar(&body)?;
    if entries.is_empty() {
        return Err(AppError::BadRequest("tar 里没有可写入的文件".into()));
    }

    for (path, _) in &entries {
        super::file_api::check_storyboard_write(&state, canvas_id, path, None).await?;
    }
    let now = util::now_secs();
    let mut written = 0u64;
    let mut bytes = 0u64;
    let mut deduplicated = 0u64;

    for (path, data) in &entries {
        let mime = canvas::mime_for_path(path);
        let outcome = canvas::put_file(&state.store, uid, canvas_id, path, &mime, data, now)
            .await
            .map_err(|e| AppError::BadRequest(e.to_string()))?
            .map_err(super::file_api::map_canvas_error_pub)?;

        written += 1;
        bytes += outcome.size;
        if outcome.deduplicated {
            deduplicated += 1;
        }
    }

    tracing::info!(
        canvas = %canvas_id,
        files = written,
        bytes,
        deduplicated,
        "tar 批量上传完成"
    );

    Ok(Json(BatchResponse {
        written,
        bytes,
        deduplicated,
    }))
}

/// 解析 tar，返回 `(path, 内容)` 列表。
///
/// 路径校验在这里做完，任何一条不合规就整批失败。
fn parse_tar(body: &[u8]) -> AppResult<Vec<(String, Vec<u8>)>> {
    use std::io::Read as _;

    let mut archive = tar::Archive::new(std::io::Cursor::new(body));
    let mut out = Vec::new();

    let iter = archive
        .entries()
        .map_err(|e| AppError::BadRequest(format!("tar 解析失败: {e}")))?;

    for entry in iter {
        let mut entry = entry.map_err(|e| AppError::BadRequest(format!("tar 条目损坏: {e}")))?;

        // 只处理普通文件。目录条目直接跳过——画布内没有目录实体，
        // tar 里的目录项对我们没有意义。
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let raw_path = entry
            .path()
            .map_err(|e| AppError::BadRequest(format!("tar 里的路径非法: {e}")))?
            .to_string_lossy()
            .into_owned();

        // 只去掉 `./` 前缀：`tar -cf x.tar .` 生成的条目都带它，这是无害的
        // 打包工件。
        //
        // **绝对路径不做改写，直接交给校验拒掉。** 把 `/etc/passwd` 静默改成
        // `etc/passwd` 再放行，等于替攻击者把一个明显的恶意路径洗干净——
        // 宁可让合法但写法奇怪的 tar 失败，也不要静默接受可疑输入。
        let path = raw_path.strip_prefix("./").unwrap_or(&raw_path).to_owned();

        // 校验用与单文件 PUT 完全相同的规则——tar 是 agent 的主要通道，
        // 不能在这里放宽，否则穿越路径会从这个口子进来。
        crate::storage::keys::validate_path(&path).map_err(|e| {
            AppError::BadRequest(format!("tar 条目 {raw_path:?} 的路径不合规: {e}"))
        })?;

        let mut data = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut data)
            .map_err(|e| AppError::BadRequest(format!("读取 tar 条目 {path:?} 失败: {e}")))?;
        out.push((path, data));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个 tar。
    fn make_tar(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, *data)
                .expect("写 tar 条目");
        }
        builder.into_inner().expect("完成 tar")
    }

    #[test]
    fn 解析正常的tar() {
        let tar = make_tar(&[
            ("a.jpg", b"jpeg"),
            ("scene01/f001.jpg", b"frame"),
            ("timeline.json", b"{}"),
        ]);
        let entries = parse_tar(&tar).expect("应能解析");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].0, "a.jpg");
        assert_eq!(entries[1].0, "scene01/f001.jpg");
        assert_eq!(entries[1].1, b"frame");
    }

    /// 手工拼一个 tar 条目，绕过 `tar::Builder` 的安全检查。
    ///
    /// `tar` crate 的 builder 自己就拒绝写含 `..` 的路径，所以用它造不出
    /// 恶意 tar。但**真实攻击者不会用这个 crate 打包**，所以必须手工构造头部
    /// 来验证我们的解析侧挡得住。
    fn raw_tar_entry(path: &str, data: &[u8]) -> Vec<u8> {
        let mut block = [0u8; 512];
        let name = path.as_bytes();
        assert!(name.len() < 100, "测试只造短路径");
        block[..name.len()].copy_from_slice(name);
        // mode
        block[100..107].copy_from_slice(b"0000644");
        // uid / gid
        block[108..115].copy_from_slice(b"0000000");
        block[116..123].copy_from_slice(b"0000000");
        // size，11 位八进制
        let size = format!("{:011o}", data.len());
        block[124..135].copy_from_slice(size.as_bytes());
        // mtime
        block[136..147].copy_from_slice(b"00000000000");
        // typeflag：普通文件
        block[156] = b'0';
        // magic + version（ustar）
        block[257..263].copy_from_slice(b"ustar\0");
        block[263..265].copy_from_slice(b"00");
        // checksum 字段先填空格，算完再写回
        block[148..156].copy_from_slice(b"        ");
        let sum: u32 = block.iter().map(|b| *b as u32).sum();
        let chk = format!("{sum:06o}\0 ");
        block[148..156].copy_from_slice(chk.as_bytes());

        let mut out = block.to_vec();
        out.extend_from_slice(data);
        // 内容按 512 字节对齐补零
        let pad = (512 - data.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        // 归档结束：两个全零块
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    #[test]
    fn tar里的穿越路径整批拒绝() {
        // 这是 agent 的主要通道，路径校验不能比单文件 PUT 宽松——
        // 否则穿越路径会从这个口子进来。
        for bad in [
            "../escape.jpg",
            "a/../../x.jpg",
            "/abs.jpg",
            "a//b.jpg",
            "./",
        ] {
            let tar = raw_tar_entry(bad, b"malicious");
            let r = parse_tar(&tar);
            assert!(
                r.is_err() || r.as_ref().is_ok_and(|v| v.is_empty()),
                "含 {bad:?} 的 tar 应被拒绝或跳过，实际: {r:?}"
            );
        }
    }

    #[test]
    fn 合法路径的手工tar能正常解析() {
        // 反向确认上面那个手工构造器是对的——否则「被拒绝」可能只是因为
        // 我拼的 tar 头本身就坏了，测试就失去了意义。
        let tar = raw_tar_entry("scene01/f001.jpg", b"frame-bytes");
        let entries = parse_tar(&tar).expect("合法路径应能解析");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "scene01/f001.jpg");
        assert_eq!(entries[0].1, b"frame-bytes");
    }

    #[test]
    fn 去掉点斜杠前缀() {
        // `tar -cf x.tar .` 生成的条目都带 `./`。
        let tar = make_tar(&[("./a.jpg", b"x")]);
        let entries = parse_tar(&tar).expect("应能解析");
        assert_eq!(entries[0].0, "a.jpg");
    }

    #[test]
    fn 空tar返回空列表() {
        let tar = make_tar(&[]);
        assert!(parse_tar(&tar).expect("应能解析").is_empty());
    }

    #[test]
    fn 损坏的tar返回错误而不是panic() {
        for garbage in [&b""[..], &b"not a tar at all"[..], &[0xffu8; 512][..]] {
            // 不 panic 就算通过；返回 Ok(空) 或 Err 都可以接受。
            let _ = parse_tar(garbage);
        }
    }
}
