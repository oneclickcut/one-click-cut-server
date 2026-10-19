//! `/f/*` 静态分发：Range、ETag、Cache-Control、条件请求。
//!
//! # 这条路径上没有任何鉴权代码
//!
//! 读取一律公开。不校验凭证、不查 visibility、不换票、不心跳。未公开的画布靠
//! **slug 不可枚举**保护（12 字符随机 base62，约 71 bit 熵）。
//!
//! `visibility` 在这里**完全不参与判断**——它只影响可发现性（是否出现在公开
//! 列表与分享入口）。这是接口文档第 2 节明确确认过的语义。
//!
//! # 缓存失效靠 URL 版本化，不用 purge
//!
//! 带 `?v=` 的资源给 `immutable` 长缓存；覆盖后 hash 变 → `?v=` 变 → 缓存键变 →
//! CDN 自然取新内容。所以**播放器必须从 manifest 拿 URL，不能自己拼路径**。
//!
//! `?v=` 的值**不参与服务端判断**：我们始终返回当前内容。这样覆盖写之后旧链接
//! 不会 404，只是会拿到新内容（而 CDN 上的旧缓存条目按 LRU 自然淘汰）。

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;

use super::AppState;
use super::error::{AppError, AppResult};
use super::range::{self, RangeSpec};
use crate::domain::{canvas, space};
use crate::storage::blob;
use crate::storage::chunk::ByteRange;

/// manifest 的缓存时间。
///
/// 它是播放器的新鲜度入口，必须能很快反映画布变化，所以只给 30 秒。
const MANIFEST_MAX_AGE: u32 = 30;

/// 不带 `?v=` 的资源请求给多长缓存。
///
/// 手工访问（比如直接在浏览器里敲 URL）会走到这里。给一分钟是为了兼容，
/// 但绝不给长缓存——没有版本参数就没法保证内容变了 URL 也变。
const UNVERSIONED_MAX_AGE: u32 = 60;

pub fn router() -> OpenApiRouter<AppState> {
    // 这两条都用多段通配，且不走 `routes!`（原因同 file_api：`{path}` 只匹配一段）。
    // 文档在 `openapi::ApiDoc` 的 `paths(...)` 里手工登记。
    OpenApiRouter::new().route(
        "/f/{slug}/{*path}",
        axum::routing::get(serve_file).head(serve_file),
    )
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct VersionQuery {
    /// 内容版本，取内容哈希的 hex 前 8 位。
    ///
    /// **只参与 CDN 缓存键，不参与服务端判断。** 值对不上也照样返回当前内容——
    /// 这样覆盖写之后旧链接不会 404。
    pub v: Option<String>,
}

/// 静态资源分发
///
/// 无需任何凭证。`manifest.json` 是这条路由下的特例，返回播放器契约。
#[utoipa::path(
    summary = "读取公开的画布文件",
    get,
    path = "/f/{slug}/{path}",
    tag = "static",
    params(
        ("slug" = String, Path, description = "画布的 12 字符随机 slug，全局唯一"),
        ("path" = String, Path, description = "画布内相对路径；`manifest.json` 返回播放器契约"),
        VersionQuery,
    ),
    responses(
        (status = 200, description = "完整内容", content_type = "application/octet-stream"),
        (status = 206, description = "部分内容（Range 请求）"),
        (status = 304, description = "ETag 未变（If-None-Match 命中）"),
        (status = 404, description = "画布或文件不存在"),
        (status = 416, description = "请求的字节区间无法满足，响应带 Content-Range: bytes */{size}"),
    ),
)]
pub async fn serve_file(
    State(state): State<AppState>,
    Path((slug, path)): Path<(String, String)>,
    Query(version): Query<VersionQuery>,
    headers: HeaderMap,
    method: axum::http::Method,
) -> AppResult<Response> {
    // 定位画布。slug 不合法时 resolve_slug 直接返回 None，所以这里不需要单独
    // 校验——非法输入与不存在是同一个结果。
    let Some((uid, canvas_id)) = space::resolve_slug(&state.store, &slug).await? else {
        return Err(AppError::NotFound);
    };

    let path = path.trim_start_matches('/');

    if path == "manifest.json" {
        return serve_manifest(&state, uid, canvas_id, &method).await;
    }

    let Some(entry) = canvas::get_file(&state.store, canvas_id, path).await? else {
        return Err(AppError::NotFound);
    };

    // ETag 用内容哈希，所以同内容永远同 ETag，跨机器也一致。
    let etag = format!("\"{}\"", entry.hash.to_hex());

    // 条件请求：ETag 没变就回 304，省掉整个响应体。
    if if_none_match_hits(&headers, &etag) {
        return Ok(build_304(&etag));
    }

    // 带了 `?v=` 才给长缓存。注意判断的是「有没有这个参数」而不是「值对不对」：
    // 值已经参与了 CDN 的缓存键，所以只要带上了，缓存就是安全的。
    let cache_control = cache_control_for(version.v.is_some());

    let mime = canvas::mime_for_path(path);
    let size = entry.size;

    // `If-Range` 不匹配时必须忽略 Range 返回全量：客户端手上的那份已经过期了，
    // 给它拼接一段新内容会拼出一个坏文件。
    let ignore_range = !if_range_matches(&headers, &etag);
    let spec = if ignore_range {
        RangeSpec::None
    } else {
        range::parse(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            size,
        )
    };

    match spec {
        RangeSpec::Unsatisfiable => Err(AppError::RangeNotSatisfiable(size)),
        // 语法不合法按 RFC 忽略，当成没带 Range。
        RangeSpec::None | RangeSpec::Invalid => {
            // HEAD 不带 body；空文件也没有可读区间（`ByteRange::full(0)` 会报错）。
            let body = if method == axum::http::Method::HEAD || size == 0 {
                Body::empty()
            } else {
                let bytes =
                    blob::read_range(&state.store, entry.hash, ByteRange::full(size)?).await?;
                Body::from(bytes)
            };

            let mut resp = Response::new(body);
            *resp.status_mut() = StatusCode::OK;
            fill_common_headers(resp.headers_mut(), &mime, &etag, &cache_control);
            set_len(resp.headers_mut(), size);
            Ok(resp)
        }
        RangeSpec::Single(r) => {
            let len = r.len();
            let body = if method == axum::http::Method::HEAD {
                Body::empty()
            } else {
                let bytes = blob::read_range(&state.store, entry.hash, r).await?;
                Body::from(bytes)
            };

            let mut resp = Response::new(body);
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            fill_common_headers(resp.headers_mut(), &mime, &etag, &cache_control);
            set_len(resp.headers_mut(), len);
            if let Ok(v) = format!("bytes {}-{}/{}", r.start, r.end, size).parse() {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            Ok(resp)
        }
    }
}

/// 公开的 manifest。与 `/api/canvases/{id}/manifest` 同内容，区别只是这里无需凭证。
async fn serve_manifest(
    state: &AppState,
    uid: crate::storage::keys::Uid,
    canvas_id: crate::storage::keys::CanvasId,
    method: &axum::http::Method,
) -> AppResult<Response> {
    let c = space::get_canvas(&state.store, uid, canvas_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows = canvas::list_files(&state.store, canvas_id, usize::MAX).await?;
    let manifest = canvas::build_manifest(canvas_id, &c, &rows);

    let mut dto: super::file_api::ManifestResponse = manifest.into();
    let export_state = crate::domain::mp4_jobs::state(&state.store, canvas_id).await?;
    dto.exports_stale = export_state["exports_stale"].as_bool().unwrap_or(false);
    dto.mp4_export = export_state["published"]
        .as_object()
        .map(|_| export_state["published"].clone());
    let json = serde_json::to_vec(&dto).map_err(|e| AppError::Internal(e.into()))?;
    let len = json.len() as u64;

    let body = if method == axum::http::Method::HEAD {
        Body::empty()
    } else {
        Body::from(json)
    };

    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // manifest 是新鲜度入口，必须能很快反映画布变化。
    if let Ok(v) = format!("public, max-age={MANIFEST_MAX_AGE}").parse() {
        h.insert(header::CACHE_CONTROL, v);
    }
    set_len(h, len);
    Ok(resp)
}

/// 按「URL 里有没有版本参数」决定缓存策略。
///
/// 带 `?v=` 的 URL 在内容变化时 URL 本身会变，所以可以永久缓存；不带的不行——
/// 内容变了 URL 不变，长缓存会让客户端一直拿到旧内容。
fn cache_control_for(versioned: bool) -> String {
    if versioned {
        "public, max-age=31536000, immutable".to_owned()
    } else {
        format!("public, max-age={UNVERSIONED_MAX_AGE}")
    }
}

/// 填上所有资源响应都要有的头。
fn fill_common_headers(h: &mut HeaderMap, mime: &str, etag: &str, cache_control: &str) {
    if let Ok(v) = mime.parse() {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = etag.parse() {
        h.insert(header::ETAG, v);
    }
    if let Ok(v) = cache_control.parse() {
        h.insert(header::CACHE_CONTROL, v);
    }
    // 告诉客户端可以发 Range 请求。没这个头，有些播放器根本不会尝试拖进度条。
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // 不设 Vary：响应内容与任何请求头无关，设了只会降低 CDN 命中率。
}

fn set_len(h: &mut HeaderMap, len: u64) {
    if let Ok(v) = len.to_string().parse() {
        h.insert(header::CONTENT_LENGTH, v);
    }
}

fn build_304(etag: &str) -> Response {
    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = StatusCode::NOT_MODIFIED;
    if let Ok(v) = etag.parse() {
        resp.headers_mut().insert(header::ETAG, v);
    }
    // 304 不带 body，也不该带 Content-Length。
    resp
}

/// `If-None-Match` 是否命中。
fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    let Some(raw) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let raw = raw.trim();
    if raw == "*" {
        return true;
    }
    // 逗号分隔的多个 ETag；也要容忍 `W/` 弱验证前缀。
    raw.split(',').any(|candidate| {
        let c = candidate.trim();
        let c = c.strip_prefix("W/").unwrap_or(c);
        c == etag
    })
}

/// `If-Range` 是否匹配（没带这个头时视为匹配）。
///
/// 不匹配意味着客户端手上那份内容已经变了，此时必须忽略 Range 返回全量——
/// 给它拼接一段新内容会拼出一个坏文件。
fn if_range_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(raw) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let raw = raw.trim();
    let raw = raw.strip_prefix("W/").unwrap_or(raw);
    raw == etag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), v.parse().expect("合法头值"));
        }
        h
    }

    const ETAG: &str = "\"abc123\"";

    #[test]
    fn if_none_match匹配() {
        assert!(
            !if_none_match_hits(&HeaderMap::new(), ETAG),
            "没带头不算命中"
        );
        assert!(if_none_match_hits(
            &headers(&[(header::IF_NONE_MATCH, ETAG)]),
            ETAG
        ));
        assert!(
            if_none_match_hits(&headers(&[(header::IF_NONE_MATCH, "*")]), ETAG),
            "`*` 匹配任何 ETag"
        );
        // 逗号分隔的多个候选，以及弱验证前缀。
        assert!(if_none_match_hits(
            &headers(&[(header::IF_NONE_MATCH, "\"other\", \"abc123\"")]),
            ETAG
        ));
        assert!(if_none_match_hits(
            &headers(&[(header::IF_NONE_MATCH, "W/\"abc123\"")]),
            ETAG
        ));
        assert!(!if_none_match_hits(
            &headers(&[(header::IF_NONE_MATCH, "\"different\"")]),
            ETAG
        ));
    }

    #[test]
    fn if_range匹配() {
        // 没带这个头视为匹配，Range 正常生效。
        assert!(if_range_matches(&HeaderMap::new(), ETAG));
        assert!(if_range_matches(
            &headers(&[(header::IF_RANGE, ETAG)]),
            ETAG
        ));
        assert!(if_range_matches(
            &headers(&[(header::IF_RANGE, "W/\"abc123\"")]),
            ETAG
        ));
        // 不匹配 → 调用方必须忽略 Range 返回全量，否则会拼出坏文件。
        assert!(!if_range_matches(
            &headers(&[(header::IF_RANGE, "\"stale\"")]),
            ETAG
        ));
    }

    #[test]
    fn 带版本参数给不可变长缓存() {
        assert_eq!(
            cache_control_for(true),
            "public, max-age=31536000, immutable"
        );
        // 没有版本参数就不能给长缓存：内容变了 URL 不变，缓存会一直给旧内容。
        assert_eq!(cache_control_for(false), "public, max-age=60");
    }
}
