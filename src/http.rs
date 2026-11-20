use crate::{
    auth::Auth,
    error::{AppError, Result},
    fs::{Disk, Lock, blocking},
    store::{Content, MAX_MANIFEST, Store, check_digest, check_name},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

#[derive(Default)]
pub struct Metrics {
    pub requests: AtomicU64,
    pub auth_failures: AtomicU64,
    pub gc_objects: AtomicU64,
    pub gc_failures: AtomicU64,
}
#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub auth: Auth,
    pub metrics: Arc<Metrics>,
}
impl AppState {
    pub fn new(store: Store, auth: Auth) -> Self {
        Self {
            store,
            auth,
            metrics: Arc::default(),
        }
    }
}
pub fn router(state: AppState) -> Router {
    Router::new().fallback(dispatch).with_state(state)
}
async fn dispatch(State(state): State<AppState>, request: Request) -> Response {
    let start = std::time::Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned(); // Never record query strings or credentials.
    let version = request.version();
    let has_content_type = request.headers().contains_key(header::CONTENT_TYPE);
    let has_content_length = request.headers().contains_key(header::CONTENT_LENGTH);
    let has_content_range = request.headers().contains_key(header::CONTENT_RANGE);
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let mut response = match handle(&state, request).await {
        Ok(response) => response,
        Err(e) => e.into_response(),
    };
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        state.metrics.auth_failures.fetch_add(1, Ordering::Relaxed);
    }
    response.headers_mut().insert(
        "docker-distribution-api-version",
        header::HeaderValue::from_static("registry/2.0"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        let details = response.extensions().get::<crate::error::ErrorDetails>();
        tracing::info!(%method, %path, http_version = ?version, status = status.as_u16(),
            error_code = details.map(|e| e.code).unwrap_or("HTTP_ERROR"),
            error_message = details.map(|e| e.message.as_str()).unwrap_or_else(|| status.canonical_reason().unwrap_or("request failed")),
            has_content_type, has_content_length, has_content_range,
            elapsed_ms = start.elapsed().as_millis() as u64, "request completed");
    } else {
        tracing::info!(%method, %path, http_version = ?version, status = status.as_u16(),
            has_content_type, has_content_length, has_content_range,
            elapsed_ms = start.elapsed().as_millis() as u64, "request completed");
    }
    response
}
fn response(status: StatusCode, headers: &[(&str, String)], body: Body) -> Result<Response> {
    let mut response = Response::builder().status(status);
    for (name, value) in headers {
        response = response.header(*name, value);
    }
    response.body(body).map_err(AppError::internal)
}
fn empty(status: StatusCode, headers: &[(&str, String)]) -> Result<Response> {
    response(status, headers, Body::empty())
}
fn challenge(auth: &Auth, error: AppError, name: Option<&str>, action: &str) -> Response {
    let mut result = error.into_response();
    if result.status() == StatusCode::UNAUTHORIZED {
        result.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            auth.challenge(name, action)
                .parse()
                .expect("validated origins and repository name"),
        );
    }
    result
}
fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .into_owned()
        .collect()
}
fn parameter(pairs: &[(String, String)], name: &str) -> Result<Option<String>> {
    let mut values = pairs
        .iter()
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v.clone());
    let value = values.next();
    if values.next().is_some() {
        return Err(AppError::bad("UNSUPPORTED", "duplicate query parameter"));
    }
    Ok(value)
}
async fn handle(state: &AppState, request: Request) -> Result<Response> {
    let (method, uri, headers) = (
        request.method().clone(),
        request.uri().clone(),
        request.headers().clone(),
    );
    let path = uri.path();
    if path == "/livez" && method == Method::GET {
        return empty(StatusCode::OK, &[]);
    }
    if path == "/readyz" && method == Method::GET {
        let disk = state.store.disk.clone();
        blocking(move || {
            if disk
                .open_read(Path::new("gc/signing-secret"))?
                .metadata()?
                .len()
                != 32
            {
                return Err(AppError::unavailable("storage not ready"));
            }
            Ok(())
        })
        .await?;
        return empty(StatusCode::OK, &[]);
    }
    if path == "/metrics" && method == Method::GET {
        let m = &state.metrics;
        let body = format!(
            "# TYPE registry_requests_total counter\nregistry_requests_total {}\n# TYPE registry_auth_failures_total counter\nregistry_auth_failures_total {}\n# TYPE registry_lock_waits_total counter\nregistry_lock_waits_total {}\n# TYPE registry_gc_objects_total counter\nregistry_gc_objects_total {}\n# TYPE registry_gc_failures_total counter\nregistry_gc_failures_total {}\n",
            m.requests.load(Ordering::Relaxed),
            m.auth_failures.load(Ordering::Relaxed),
            state.store.disk.lock_waits.load(Ordering::Relaxed),
            m.gc_objects.load(Ordering::Relaxed),
            m.gc_failures.load(Ordering::Relaxed)
        );
        return response(
            StatusCode::OK,
            &[("content-type", "text/plain; version=0.0.4".into())],
            Body::from(body),
        );
    }
    if path == "/auth/token" && method == Method::GET {
        return match state.auth.token(&headers, uri.query()).await {
            Ok(token) => {
                let mut result = axum::Json(token).into_response();
                result.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                Ok(result)
            }
            Err(e) => {
                let mut result = e.into_response();
                result.headers_mut().insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-store"),
                );
                if result.status() == StatusCode::UNAUTHORIZED {
                    result.headers_mut().insert(
                        header::WWW_AUTHENTICATE,
                        header::HeaderValue::from_static("Basic realm=\"GitLab CI ID token\""),
                    );
                }
                Ok(result)
            }
        };
    }
    if (path == "/v2/" || path == "/v2") && (method == Method::GET || method == Method::HEAD) {
        return match state.auth.bearer(&headers) {
            Ok(_) => response(
                StatusCode::OK,
                &[
                    ("content-type", "application/json".into()),
                    ("content-length", "2".into()),
                ],
                if method == Method::HEAD {
                    Body::empty()
                } else {
                    Body::from("{}")
                },
            ),
            Err(e) => Ok(challenge(&state.auth, e, None, "pull")),
        };
    }
    let tail = path
        .strip_prefix("/v2/")
        .ok_or_else(|| AppError::missing("UNSUPPORTED"))?;
    if tail.contains('%') || tail.contains('\\') {
        return Err(AppError::bad(
            "NAME_INVALID",
            "encoded or ambiguous repository paths are not accepted",
        ));
    }
    let pairs = query_pairs(uri.query());
    if let Some((name, reference)) = tail.rsplit_once("/manifests/") {
        check_name(name)?;
        if reference.is_empty() || reference.contains('/') {
            return Err(AppError::missing("UNSUPPORTED"));
        }
        let store = state.store.clone();
        let name = name.to_owned();
        let reference = reference.to_owned();
        if method == Method::GET || method == Method::HEAD {
            let content = blocking(move || store.content(&name, &reference, true)).await?;
            if !accepts(&headers, &content.media_type) {
                return Err(AppError::new(
                    StatusCode::NOT_ACCEPTABLE,
                    "UNSUPPORTED",
                    "manifest media type is not acceptable",
                ));
            }
            return serve_content(content, &headers, method == Method::HEAD, false).await;
        }
        let action = if method == Method::DELETE {
            "delete"
        } else {
            "push"
        };
        let owner = match state.auth.authorize(&headers, &name, action) {
            Ok(o) => o,
            Err(e) => return Ok(challenge(&state.auth, e, Some(&name), action)),
        };
        if method == Method::PUT {
            let media_type = headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_owned();
            let body = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                to_bytes(request.into_body(), MAX_MANIFEST),
            )
            .await
            .map_err(|_| AppError::bad("MANIFEST_INVALID", "manifest read timeout"))?
            .map_err(|_| {
                AppError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "MANIFEST_INVALID",
                    "manifest is too large or unreadable",
                )
            })?;
            let location_name = name.clone();
            let d =
                blocking(move || store.put_manifest(&name, &reference, &owner, &media_type, &body))
                    .await?;
            return empty(
                StatusCode::CREATED,
                &[
                    ("location", format!("/v2/{location_name}/manifests/{d}")),
                    ("docker-content-digest", d),
                ],
            );
        }
        if method == Method::DELETE {
            blocking(move || store.delete_manifest(&name, &reference, &owner)).await?;
            return empty(StatusCode::ACCEPTED, &[]);
        }
        return Err(AppError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "method unsupported",
        ));
    }
    if let Some(name) = tail.strip_suffix("/tags/list") {
        check_name(name)?;
        if method != Method::GET {
            return Err(AppError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "method unsupported",
            ));
        }
        let store = state.store.clone();
        let owned = name.to_owned();
        let tags = blocking(move || store.tags(&owned)).await?;
        let n = parameter(&pairs, "n")?
            .map(|s| {
                s.parse::<usize>()
                    .map_err(|_| AppError::bad("UNSUPPORTED", "invalid page size"))
            })
            .transpose()?
            .unwrap_or(1000)
            .min(1000);
        let last = parameter(&pairs, "last")?.unwrap_or_default();
        let mut remaining = tags.into_iter().filter(|t| t > &last);
        let page: Vec<_> = remaining.by_ref().take(n).collect();
        let more = n > 0 && remaining.next().is_some();
        let mut result = axum::Json(json!({"name":name,"tags":page})).into_response();
        if more {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("n", &n.to_string())
                .append_pair("last", page.last().unwrap())
                .finish();
            result.headers_mut().insert(
                header::LINK,
                format!("</v2/{name}/tags/list?{query}>; rel=\"next\"")
                    .parse()
                    .map_err(AppError::internal)?,
            );
        }
        return Ok(result);
    }
    if let Some((name, id)) = tail.rsplit_once("/blobs/uploads/") {
        check_name(name)?;
        let owner = match state.auth.authorize(&headers, name, "push") {
            Ok(o) => o,
            Err(e) => return Ok(challenge(&state.auth, e, Some(name), "pull,push")),
        };
        let store = state.store.clone();
        let name = name.to_owned();
        let id = id.to_owned();
        if id.is_empty() && method == Method::POST {
            let digest = parameter(&pairs, "digest")?;
            if let Some(d) = &digest {
                check_digest(d)?;
            }
            if let Some(mount) = parameter(&pairs, "mount")? {
                check_digest(&mount)?;
                if let Some(from) = parameter(&pairs, "from")? {
                    let (s, n, o, d) = (store.clone(), name.clone(), owner.clone(), mount.clone());
                    if blocking(move || s.mount(&n, &from, &d, &o)).await? {
                        return empty(
                            StatusCode::CREATED,
                            &[
                                ("location", format!("/v2/{name}/blobs/{mount}")),
                                ("docker-content-digest", mount),
                            ],
                        );
                    }
                }
            }
            let (s, n, o) = (store.clone(), name.clone(), owner.clone());
            let upload = blocking(move || s.start_upload(&n, &o)).await?;
            if let Some(d) = digest {
                let disk = store.disk.clone();
                let active_id = upload.id.clone();
                let _active =
                    blocking(move || disk.lock(&format!("upload-active:{active_id}"), true))
                        .await?;
                let staged = stage(&store.disk, &headers, request.into_body()).await?;
                let id = upload.id.clone();
                let n = name.clone();
                let dd = d.clone();
                blocking(move || {
                    store.append_upload(&n, &id, &owner, &staged.path, None, Some(&dd))
                })
                .await?;
                return empty(
                    StatusCode::CREATED,
                    &[
                        ("location", format!("/v2/{name}/blobs/{d}")),
                        ("docker-content-digest", d),
                    ],
                );
            }
            return upload_response(&name, &upload, StatusCode::ACCEPTED);
        }
        crate::store::upload_path(&id, "json")?;
        if method == Method::GET || method == Method::HEAD {
            let n = name.clone();
            let u = blocking(move || store.upload_status(&n, &id, &owner)).await?;
            return upload_response(&name, &u, StatusCode::NO_CONTENT);
        }
        if method == Method::DELETE {
            blocking(move || store.cancel_upload(&name, &id, &owner)).await?;
            return empty(StatusCode::NO_CONTENT, &[]);
        }
        if method == Method::PATCH || method == Method::PUT {
            let finish = if method == Method::PUT {
                Some(
                    parameter(&pairs, "digest")?
                        .ok_or_else(|| AppError::bad("DIGEST_INVALID", "missing digest"))?,
                )
            } else {
                None
            };
            if let Some(d) = &finish {
                check_digest(d)?;
            }
            // Older Docker ReadFrom pushers omit all three content headers.
            // Missing type is unambiguous here: this endpoint only accepts opaque blob bytes.
            if method == Method::PATCH
                && headers.contains_key(header::CONTENT_TYPE)
                && headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|h| h.to_str().ok())
                    .map(|v| v.split(';').next().unwrap_or("").trim())
                    != Some("application/octet-stream")
            {
                return Err(AppError::bad(
                    "BLOB_UPLOAD_INVALID",
                    "PATCH requires application/octet-stream",
                ));
            }
            let range = parse_upload_range(&headers)?;
            let disk = store.disk.clone();
            let active_id = id.clone();
            // Protect the session for the entire streamed body, not just its commit.
            // GC only tries this lease nonblockingly while holding publication.
            let _active =
                blocking(move || disk.lock(&format!("upload-active:{active_id}"), true)).await?;
            let staged = stage(&store.disk, &headers, request.into_body()).await?;
            let (n, f) = (name.clone(), finish.clone());
            let (status_store, status_id, status_owner) =
                (store.clone(), id.clone(), owner.clone());
            let result = blocking(move || {
                store.append_upload(&n, &id, &owner, &staged.path, range, f.as_deref())
            })
            .await;
            let upload = match result {
                Ok(upload) => upload,
                Err(error) if error.status == StatusCode::RANGE_NOT_SATISFIABLE => {
                    let n = name.clone();
                    let committed =
                        blocking(move || status_store.upload_status(&n, &status_id, &status_owner))
                            .await?;
                    let mut result = error.into_response();
                    result.headers_mut().insert(
                        header::RANGE,
                        format!("0-{}", committed.offset.saturating_sub(1))
                            .parse()
                            .map_err(AppError::internal)?,
                    );
                    return Ok(result);
                }
                Err(error) => return Err(error),
            };
            if let Some(d) = finish {
                return empty(
                    StatusCode::CREATED,
                    &[
                        ("location", format!("/v2/{name}/blobs/{d}")),
                        ("docker-content-digest", d),
                    ],
                );
            }
            return upload_response(&name, &upload, StatusCode::ACCEPTED);
        }
        return Err(AppError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "method unsupported",
        ));
    }
    if let Some((name, d)) = tail.rsplit_once("/blobs/") {
        check_name(name)?;
        check_digest(d)?;
        if method != Method::GET && method != Method::HEAD {
            return Err(AppError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "UNSUPPORTED",
                "blob deletion is managed by GC",
            ));
        }
        let store = state.store.clone();
        let name = name.to_owned();
        let d = d.to_owned();
        let content = blocking(move || store.content(&name, &d, false)).await?;
        return serve_content(content, &headers, method == Method::HEAD, true).await;
    }
    Err(AppError::missing("UNSUPPORTED"))
}
fn accepts(headers: &HeaderMap, media_type: &str) -> bool {
    let values: Vec<_> = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if values.is_empty() {
        return true;
    }
    // The most specific matching media range determines quality, including explicit q=0.
    let mut best: Option<(u8, f32)> = None;
    for value in values {
        for part in value.split(',') {
            let mut params = part.split(';');
            let kind = params.next().unwrap_or("").trim();
            let quality = params
                .find_map(|p| p.trim().strip_prefix("q="))
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(1.0);
            let specificity = if kind == media_type {
                2
            } else if kind == "application/*" {
                1
            } else if kind == "*/*" {
                0
            } else {
                continue;
            };
            if best.is_none_or(|(s, q)| specificity > s || specificity == s && quality > q) {
                best = Some((specificity, quality));
            }
        }
    }
    best.is_some_and(|(_, q)| q > 0.0 && q <= 1.0)
}
fn upload_response(name: &str, u: &crate::store::Upload, status: StatusCode) -> Result<Response> {
    empty(
        status,
        &[
            ("location", format!("/v2/{name}/blobs/uploads/{}", u.id)),
            ("docker-upload-uuid", u.id.clone()),
            ("range", format!("0-{}", u.offset.saturating_sub(1))),
            ("content-length", "0".into()),
        ],
    )
}
fn parse_upload_range(headers: &HeaderMap) -> Result<Option<(u64, u64)>> {
    let Some(value) = headers.get(header::CONTENT_RANGE) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| AppError::bad("BLOB_UPLOAD_INVALID", "invalid chunk range"))?;
    let value = value.strip_prefix("bytes ").unwrap_or(value);
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| AppError::bad("BLOB_UPLOAD_INVALID", "invalid chunk range"))?;
    Ok(Some((
        start
            .parse()
            .map_err(|_| AppError::bad("BLOB_UPLOAD_INVALID", "invalid chunk start"))?,
        end.parse()
            .map_err(|_| AppError::bad("BLOB_UPLOAD_INVALID", "invalid chunk end"))?,
    )))
}
async fn serve_content(
    content: Content,
    headers: &HeaderMap,
    head: bool,
    ranges: bool,
) -> Result<Response> {
    let mut status = StatusCode::OK;
    let (mut start, mut length) = (0, content.size);
    let mut out = vec![
        ("content-type", content.media_type),
        ("docker-content-digest", content.digest.clone()),
        ("etag", format!("\"{}\"", content.digest)),
    ];
    if ranges {
        out.push(("accept-ranges", "bytes".into()));
        if !head && let Some(raw) = headers.get(header::RANGE) {
            let permitted = headers.get(header::IF_RANGE).is_none_or(|v| {
                v.to_str().ok() == Some(format!("\"{}\"", content.digest).as_str())
            });
            if permitted {
                let range = raw
                    .to_str()
                    .ok()
                    .and_then(|s| download_range(s, content.size));
                let Some((first, last)) = range else {
                    return empty(
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        &[("content-range", format!("bytes */{}", content.size))],
                    );
                };
                start = first;
                length = last - first + 1;
                status = StatusCode::PARTIAL_CONTENT;
                out.push((
                    "content-range",
                    format!("bytes {first}-{last}/{}", content.size),
                ));
            }
        }
    }
    out.push(("content-length", length.to_string()));
    if head {
        return empty(status, &out);
    }
    let mut file = tokio::fs::File::from_std(content.file);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    response(
        status,
        &out,
        Body::from_stream(tokio_util::io::ReaderStream::new(file.take(length))),
    )
}
fn download_range(raw: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let (start, end) = raw.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 {
            return None;
        }
        Some((size.saturating_sub(suffix), size - 1))
    } else {
        let first = start.parse::<u64>().ok()?;
        let last = if end.is_empty() {
            size - 1
        } else {
            end.parse::<u64>().ok()?.min(size - 1)
        };
        if first >= size || first > last {
            None
        } else {
            Some((first, last))
        }
    }
}
struct Staged {
    path: PathBuf,
    relative: PathBuf,
    disk: Disk,
    _lease: Lock,
}
impl Drop for Staged {
    fn drop(&mut self) {
        let disk = self.disk.clone();
        let path = self.relative.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let _ = disk.remove(&path);
            });
        } else {
            let _ = disk.remove(&path);
        }
    }
}
async fn stage(disk: &Disk, headers: &HeaderMap, body: Body) -> Result<Staged> {
    const LIMIT: u64 = 100 * 1024 * 1024 * 1024;
    let expected = headers
        .get(header::CONTENT_LENGTH)
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| AppError::bad("SIZE_INVALID", "invalid content length"))
        })
        .transpose()?;
    if expected.is_some_and(|n| n > LIMIT) {
        return Err(AppError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "SIZE_INVALID",
            "request exceeds 100 GiB",
        ));
    }
    let disk = disk.clone();
    let (staged, file) = blocking(move || {
        use std::os::unix::fs::OpenOptionsExt;
        let id = uuid::Uuid::new_v4().to_string();
        let relative = PathBuf::from(format!("incoming/{id}"));
        let path = disk.path(&relative)?;
        let lease = disk.lock(&format!("incoming:{id}"), true)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok((
            Staged {
                path,
                relative,
                disk,
                _lease: lease,
            },
            file,
        ))
    })
    .await?;
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut size = 0u64;
    loop {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(60), stream.next())
            .await
            .map_err(|_| AppError::bad("BLOB_UPLOAD_INVALID", "upload idle timeout"))?;
        let Some(chunk) = chunk else { break };
        let chunk =
            chunk.map_err(|_| AppError::bad("BLOB_UPLOAD_INVALID", "request stream failed"))?;
        size = size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| AppError::bad("SIZE_INVALID", "request too large"))?;
        if size > LIMIT {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "SIZE_INVALID",
                "request exceeds 100 GiB",
            ));
        }
        file.write_all(&chunk).await?;
    }
    if expected.is_some_and(|n| n != size) {
        return Err(AppError::bad("SIZE_INVALID", "content length mismatch"));
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    Ok(staged)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges() {
        assert_eq!(download_range("bytes=-3", 10), Some((7, 9)));
        assert_eq!(download_range("bytes=2-100", 10), Some((2, 9)));
        for s in ["bytes=10-", "bytes=2-1", "bytes=0-1,3-4", "bytes=-0"] {
            assert!(download_range(s, 10).is_none());
        }
    }
    #[test]
    fn negotiation() {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            "*/*;q=1, application/vnd.oci.image.index.v1+json;q=0"
                .parse()
                .unwrap(),
        );
        assert!(!accepts(&h, crate::store::OCI_INDEX));
        assert!(accepts(&h, crate::store::OCI_MANIFEST));
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use crate::auth::fixtures;
    use tower::ServiceExt;
    fn app() -> (tempfile::TempDir, AppState, Router) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let state = AppState::new(store, fixtures::auth());
        let app = router(state.clone());
        (root, state, app)
    }
    async fn send(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        kind: Option<&str>,
        body: impl Into<Body>,
    ) -> Response {
        let mut request = axum::http::Request::builder().method(method).uri(path);
        if let Some(t) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(k) = kind {
            request = request.header(header::CONTENT_TYPE, k);
        }
        app.clone()
            .oneshot(request.body(body.into()).unwrap())
            .await
            .unwrap()
    }
    async fn token(state: &AppState, path: &str, level: u64) -> String {
        let mut claims = fixtures::claims();
        claims["user_access_level"] = json!(level);
        state.auth.token(&fixtures::headers(&fixtures::sign(&claims)),Some(&format!("service=https%3A%2F%2Fregistry.example.com&scope=repository:{path}:pull,push,delete"))).await.unwrap().token
    }
    async fn bytes(response: Response) -> Vec<u8> {
        to_bytes(response.into_body(), 20 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec()
    }
    /// Local Docker acceptance only: the fixture key never enters production builds.
    #[tokio::test]
    async fn legacy_streamed_patch_missing_optional_headers() {
        for content_type in [
            None,
            Some("application/octet-stream"),
            Some("application/octet-stream; charset=binary"),
        ] {
            let (_root, state, app) = app();
            let t = token(&state, "team/project", 40).await;
            let r = send(
                &app,
                "POST",
                "/v2/team/project/blobs/uploads/",
                Some(&t),
                None,
                Body::empty(),
            )
            .await;
            let location = r.headers()[header::LOCATION].to_str().unwrap().to_owned();
            let body = Body::from_stream(futures_util::stream::iter([
                Ok::<_, std::io::Error>("first"),
                Ok("second"),
            ]));
            let r = send(&app, "PATCH", &location, Some(&t), content_type, body).await;
            assert_eq!(r.status(), StatusCode::ACCEPTED);
            assert_eq!(r.headers()[header::RANGE], "0-10");
            let digest = crate::store::digest(b"firstsecond");
            // Docker's bodyless commit percent-encodes the colon in the query, not the path.
            let r = send(
                &app,
                "PUT",
                &format!("{location}?digest={}", digest.replace(':', "%3A")),
                Some(&t),
                None,
                Body::empty(),
            )
            .await;
            assert_eq!(r.status(), StatusCode::CREATED);
            let r = send(
                &app,
                "GET",
                &format!("/v2/team/project/blobs/{digest}"),
                None,
                None,
                Body::empty(),
            )
            .await;
            assert_eq!(bytes(r).await, b"firstsecond");
        }
    }
    #[tokio::test]
    async fn request_failure_logs_exclude_queries_and_credentials() {
        if std::env::var_os("RORO_HTTP_LOG_TEST_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "http::protocol_tests::request_failure_logs_exclude_queries_and_credentials",
                ])
                .env("RORO_HTTP_LOG_TEST_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        use std::io::Write;
        use tracing::instrument::WithSubscriber;
        #[derive(Clone)]
        struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Logs {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Logs(Default::default());
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        let mut bearer = String::new();
        async {
            let (_root, state, app) = app();
            bearer = token(&state, "team/project", 40).await;
            let r = send(
                &app,
                "POST",
                "/v2/team/project/blobs/uploads/",
                Some(&bearer),
                None,
                Body::empty(),
            )
            .await;
            let location = r.headers()[header::LOCATION].to_str().unwrap().to_owned();
            let r = send(
                &app,
                "PATCH",
                &format!("{location}?secret=never-log-query"),
                Some(&bearer),
                Some("text/plain"),
                "data",
            )
            .await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
            let r = send(
                &app,
                "POST",
                "/v2/team/project/blobs/uploads/?secret=never-log-query",
                None,
                None,
                Body::empty(),
            )
            .await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
            let d = crate::store::digest(b"missing");
            assert_eq!(
                send(
                    &app,
                    "HEAD",
                    &format!("/v2/team/project/blobs/{d}"),
                    None,
                    None,
                    Body::empty()
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );
        }
        .with_subscriber(subscriber)
        .await;
        let out = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(!out.contains("never-log-query"));
        assert!(!out.contains("?secret"));
        assert!(!out.contains(&bearer));
        let events: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for (status, code) in [
            (400, "BLOB_UPLOAD_INVALID"),
            (401, "UNAUTHORIZED"),
            (404, "BLOB_UNKNOWN"),
        ] {
            let fields = &events
                .iter()
                .find(|e| e["fields"]["status"] == status)
                .expect("missing request failure")["fields"];
            assert_eq!(fields["error_code"], code);
            assert!(fields["error_message"].as_str().unwrap().len() > 3);
            assert!(fields["path"].as_str().unwrap().starts_with("/v2/"));
            assert!(fields["method"].is_string());
            assert!(fields["http_version"].is_string());
        }
    }
    #[tokio::test]
    #[ignore = "requires loopback sockets"]
    async fn legacy_streamed_patch_over_tls_http1_and_http2() {
        let (root, state, app) = app();
        let mut config = fixtures::config();
        config.data_root = root.path().into();
        config.public_url = "https://localhost".parse().unwrap();
        let tls = crate::tls::load(&config, state.store.disk.clone())
            .await
            .unwrap();
        let cert = std::fs::read(root.path().join(crate::tls::PUBLIC_CERT)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = axum_server::Handle::new();
        let server_handle = handle.clone();
        let task = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener.into_std().unwrap(), tls)
                .unwrap()
                .handle(server_handle)
                .serve(app.into_make_service())
                .await
                .unwrap()
        });
        let token = token(&state, "team/project", 40).await;
        for h2 in [false, true] {
            let builder = reqwest::Client::builder()
                .add_root_certificate(reqwest::Certificate::from_pem(&cert).unwrap())
                .no_proxy();
            let client = if h2 {
                builder.http2_prior_knowledge()
            } else {
                builder.http1_only()
            }
            .build()
            .unwrap();
            let expected = if h2 {
                reqwest::Version::HTTP_2
            } else {
                reqwest::Version::HTTP_11
            };
            let origin = format!("https://localhost:{port}");
            let r = client
                .post(format!("{origin}/v2/team/project/blobs/uploads/"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap();
            assert_eq!(r.version(), expected);
            assert_eq!(r.status(), 202);
            let url = format!(
                "{origin}{}",
                r.headers()[header::LOCATION].to_str().unwrap()
            );
            for content_type in [false, true] {
                let stream = futures_util::stream::iter([
                    Ok::<_, std::io::Error>(vec![7; 1024 * 1024]),
                    Ok(vec![8; 1024 * 1024]),
                ]);
                let mut req = client
                    .patch(&url)
                    .bearer_auth(&token)
                    .body(reqwest::Body::wrap_stream(stream));
                if content_type {
                    req = req.header(header::CONTENT_TYPE, "application/octet-stream");
                }
                let r = req.send().await.unwrap();
                assert_eq!(r.version(), expected);
                assert_eq!(r.status(), 202);
            }
            use sha2::{Digest, Sha256};
            let mut hash = Sha256::new();
            for _ in 0..2 {
                hash.update(vec![7; 1024 * 1024]);
                hash.update(vec![8; 1024 * 1024]);
            }
            let digest = format!("sha256:{}", hex::encode(hash.finalize()));
            let r = client
                .put(&url)
                .query(&[("digest", &digest)])
                .bearer_auth(&token)
                .send()
                .await
                .unwrap();
            assert_eq!(r.version(), expected);
            assert_eq!(r.status(), 201);
            let r = client
                .get(format!("{origin}/v2/team/project/blobs/{digest}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.version(), expected);
            assert_eq!(r.status(), 200);
            assert_eq!(r.bytes().await.unwrap().len(), 4 * 1024 * 1024);
        }
        handle.graceful_shutdown(Some(std::time::Duration::from_secs(2)));
        task.await.unwrap();
    }
    #[tokio::test]
    #[ignore]
    async fn docker_acceptance_fixture() {
        let root = std::path::PathBuf::from(
            std::env::var("RORO_DOCKER_FIXTURE_DIR").expect("set a disposable fixture directory"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = fixtures::config();
        config.data_root = root.join("data");
        config.public_url = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        )
        .parse()
        .unwrap();
        let store = Store::open(&config.data_root).unwrap();
        let mut tls = crate::tls::load(&config, store.disk.clone()).await.unwrap();
        if std::env::var_os("RORO_DOCKER_HTTP1_ONLY").is_some() {
            let mut inner = (*tls.get_inner()).clone();
            inner.alpn_protocols = vec![b"http/1.1".to_vec()];
            tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(inner));
        }
        let auth = Auth::testing(
            config.clone(),
            serde_json::from_str(include_str!("../tests/fixtures/ci-test-jwks.json")).unwrap(),
        );
        let mut claims = fixtures::claims();
        claims["aud"] = json!(config.audience());
        claims["exp"] = json!(crate::now() + 3600);
        let backend_origin = config.audience();
        let mut origin = backend_origin.clone();
        let bridge = if std::env::var_os("RORO_DOCKER_HTTP2_BRIDGE").is_some() {
            // Go's classic Docker transport does not negotiate h2. Keep its real
            // pusher unchanged and forward its streaming bytes over verified TLS h2.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            origin = format!(
                "https://localhost:{}",
                listener.local_addr().unwrap().port()
            );
            Some(listener)
        } else {
            None
        };
        let metadata = json!({"origin":origin, "backendOrigin":backend_origin, "project":"team/project", "token":fixtures::sign(&claims),
            "certificate":config.data_root.join(crate::tls::PUBLIC_CERT)});
        tracing_subscriber::fmt()
            .json()
            .with_env_filter("roro=info")
            .try_init()
            .unwrap();
        let app = router(AppState::new(store, auth));
        if let Some(bridge_listener) = bridge {
            let cert = std::fs::read(config.data_root.join(crate::tls::PUBLIC_CERT)).unwrap();
            let client = reqwest::Client::builder()
                .no_proxy()
                .http2_prior_knowledge()
                .redirect(reqwest::redirect::Policy::none())
                .add_root_certificate(reqwest::Certificate::from_pem(&cert).unwrap())
                .build()
                .unwrap();
            let bridge_app = Router::new().fallback(move |request: Request| {
                let client = client.clone();
                let backend_origin = backend_origin.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let mut headers = parts.headers;
                    for name in [
                        "host",
                        "connection",
                        "transfer-encoding",
                        "upgrade",
                        "keep-alive",
                        "proxy-connection",
                    ] {
                        headers.remove(name);
                    }
                    let response = client
                        .request(parts.method, format!("{backend_origin}{}", parts.uri))
                        .headers(headers)
                        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(
                        response.version(),
                        reqwest::Version::HTTP_2,
                        "bridge must actually use h2"
                    );
                    let status = response.status();
                    let headers = response.headers().clone();
                    let mut result = Response::new(Body::from_stream(response.bytes_stream()));
                    *result.status_mut() = status;
                    *result.headers_mut() = headers;
                    result
                }
            });
            let mut front = (*tls.get_inner()).clone();
            front.alpn_protocols = vec![b"http/1.1".to_vec()];
            let front = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(front));
            tokio::spawn(async move {
                axum_server::from_tcp_rustls(listener.into_std().unwrap(), tls)
                    .unwrap()
                    .serve(app.into_make_service())
                    .await
                    .unwrap();
            });
            std::fs::write(
                root.join("fixture.json"),
                serde_json::to_vec(&metadata).unwrap(),
            )
            .unwrap();
            axum_server::from_tcp_rustls(bridge_listener.into_std().unwrap(), front)
                .unwrap()
                .serve(bridge_app.into_make_service())
                .await
                .unwrap();
        } else {
            std::fs::write(
                root.join("fixture.json"),
                serde_json::to_vec(&metadata).unwrap(),
            )
            .unwrap();
            axum_server::from_tcp_rustls(listener.into_std().unwrap(), tls)
                .unwrap()
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
    }
    #[tokio::test]
    async fn gitlab_owner_docker_handshake_large_upload_and_both_manifest_formats() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        let state = AppState::new(
            Store::open(root.path()).unwrap(),
            fixtures::deployment_auth(),
        );
        let app = router(state.clone());
        let origin = "https://registry.example.com";
        let response = send(&app, "GET", "/v2/", None, None, Body::empty()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            format!("Bearer realm=\"{origin}/auth/token\",service=\"{origin}\"")
        );
        async fn exchange(app: &Router) -> String {
            let jwt = fixtures::sign(&fixtures::gitlab_job_claims());
            let basic = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("gitlab-ci-token:{jwt}"))
            );
            use base64::Engine;
            let response = app.clone().oneshot(Request::builder()
                .uri("/auth/token?service=https%3A%2F%2Fregistry.example.com&scope=repository:acme/widget:push,pull&account=gitlab-ci-token&client_id=docker")
                .header(header::AUTHORIZATION, basic).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let value: serde_json::Value = serde_json::from_slice(&bytes(response).await).unwrap();
            assert_eq!(value["token"], value["access_token"]);
            assert_eq!(value["expires_in"], 300);
            value["token"].as_str().unwrap().to_owned()
        }
        let token = exchange(&app).await;
        assert_eq!(
            send(&app, "GET", "/v2/", Some(&token), None, Body::empty())
                .await
                .status(),
            StatusCode::OK
        );
        let response = send(
            &app,
            "POST",
            "/v2/acme/widget/blobs/uploads/",
            Some(&token),
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let location = response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_owned();
        // Hundreds of MiB, but bounded memory in both request generation and download checking.
        let chunk = axum::body::Bytes::from(vec![0xa7; 1024 * 1024]);
        let mut hash = Sha256::new();
        for _ in 0..320 {
            hash.update(&chunk);
        }
        let digest = format!("sha256:{}", hex::encode(hash.finalize()));
        let length = 320_u64 * 1024 * 1024;
        let stream = futures_util::stream::iter(
            (0..320).map(move |_| Ok::<_, std::io::Error>(chunk.clone())),
        );
        let request = Request::builder()
            .method("PATCH")
            .uri(&location)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, length)
            .body(Body::from_stream(stream))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers()[header::RANGE],
            format!("0-{}", length - 1)
        );
        assert_eq!(
            send(
                &app,
                "PUT",
                &format!("{location}?digest={digest}"),
                Some(&token),
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::CREATED
        );
        let config = b"{}";
        let config_digest = crate::store::digest(config);
        assert_eq!(
            send(
                &app,
                "POST",
                &format!("/v2/acme/widget/blobs/uploads/?digest={config_digest}"),
                Some(&token),
                Some("application/octet-stream"),
                config.to_vec()
            )
            .await
            .status(),
            StatusCode::CREATED
        );
        for (tag, manifest_type, config_type, layer_type) in [
            (
                "docker",
                crate::store::DOCKER_MANIFEST,
                "application/vnd.docker.container.image.v1+json",
                "application/vnd.docker.image.rootfs.diff.tar.gzip",
            ),
            (
                "oci",
                crate::store::OCI_MANIFEST,
                "application/vnd.oci.image.config.v1+json",
                "application/vnd.oci.image.layer.v1.tar+gzip",
            ),
        ] {
            // Opaque blob bytes exercise the registry protocol, not a runnable filesystem image.
            let manifest = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":manifest_type,
                "config":{"mediaType":config_type,"digest":config_digest,"size":config.len()},
                "layers":[{"mediaType":layer_type,"digest":digest,"size":length}]}))
            .unwrap();
            let path = format!("/v2/acme/widget/manifests/{tag}");
            let response = send(
                &app,
                "PUT",
                &path,
                Some(&token),
                Some(manifest_type),
                manifest.clone(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CREATED);
            let response = send(&app, "GET", &path, None, None, Body::empty()).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(bytes(response).await, manifest);
        }
        let response = send(
            &app,
            "GET",
            &format!("/v2/acme/widget/blobs/{digest}"),
            None,
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        let mut received = 0_u64;
        let mut hash = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            received += chunk.len() as u64;
            hash.update(&chunk);
        }
        assert_eq!(received, length);
        assert_eq!(format!("sha256:{}", hex::encode(hash.finalize())), digest);
        // Simulate token expiry without sleeping or weakening production validation.
        let mut bearer = HeaderMap::new();
        bearer.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        let mut claims = state.auth.bearer(&bearer).unwrap();
        claims.exp = crate::now() - 1;
        let expired = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(&[42; 32]),
        )
        .unwrap();
        let response = send(
            &app,
            "POST",
            "/v2/acme/widget/blobs/uploads/",
            Some(&expired),
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("scope=\"repository:acme/widget:pull,push\"")
        );
        let renewed = exchange(&app).await;
        assert_eq!(
            send(
                &app,
                "POST",
                "/v2/acme/widget/blobs/uploads/",
                Some(&renewed),
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
    }
    #[tokio::test]
    async fn anonymous_pull_authenticated_push_and_delete() {
        let (_root, state, app) = app();
        let t = token(&state, "team/project", 40).await;
        let r = send(&app, "GET", "/v2/", None, None, Body::empty()).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(
            r.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("/auth/token")
        );
        assert_eq!(
            send(&app, "GET", "/v2/", Some(&t), None, Body::empty())
                .await
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            send(
                &app,
                "POST",
                "/v2/team/project/blobs/uploads/",
                None,
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        let config = b"{}";
        let d = crate::store::digest(config);
        let r = send(
            &app,
            "POST",
            &format!("/v2/team/project/blobs/uploads/?digest={d}"),
            Some(&t),
            Some("application/octet-stream"),
            config.to_vec(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let body=serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":crate::store::OCI_MANIFEST,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":d,"size":2},"layers":[]})).unwrap();
        let r = send(
            &app,
            "PUT",
            "/v2/team/project/manifests/latest",
            Some(&t),
            Some(crate::store::OCI_MANIFEST),
            body.clone(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let manifest_digest = r.headers()["docker-content-digest"]
            .to_str()
            .unwrap()
            .to_owned();
        let r = send(
            &app,
            "GET",
            "/v2/team/project/manifests/latest",
            None,
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(bytes(r).await, body);
        let r = send(
            &app,
            "HEAD",
            &format!("/v2/team/project/blobs/{d}"),
            None,
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(r.headers()[header::CONTENT_LENGTH], "2");
        assert!(bytes(r).await.is_empty());
        let r = send(
            &app,
            "GET",
            "/v2/team/project/tags/list?n=1",
            None,
            None,
            Body::empty(),
        )
        .await;
        let v: serde_json::Value = serde_json::from_slice(&bytes(r).await).unwrap();
        assert_eq!(v["tags"], json!(["latest"]));
        let dev = token(&state, "team/project", 30).await;
        assert_eq!(
            send(
                &app,
                "DELETE",
                "/v2/team/project/manifests/latest",
                Some(&dev),
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                &app,
                "DELETE",
                &format!("/v2/team/project/manifests/{manifest_digest}"),
                Some(&t),
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            send(
                &app,
                "GET",
                "/v2/team/project/manifests/latest",
                None,
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    #[tokio::test]
    async fn chunked_upload_range_resume_cancel_and_download_range() {
        let (_root, state, app) = app();
        let t = token(&state, "team/project", 40).await;
        let r = send(
            &app,
            "POST",
            "/v2/team/project/blobs/uploads/",
            Some(&t),
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let location = r.headers()[header::LOCATION].to_str().unwrap().to_owned();
        let r = send(
            &app,
            "PATCH",
            &location,
            Some(&t),
            Some("application/octet-stream"),
            "abc",
        )
        .await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        assert_eq!(r.headers()[header::RANGE], "0-2");
        let other = router(AppState::new(
            Store::open(&state.store.disk.root).unwrap(),
            state.auth.clone(),
        ));
        let r = send(&other, "GET", &location, Some(&t), None, Body::empty()).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(r.headers()[header::RANGE], "0-2");
        let d = crate::store::digest(b"abcdef");
        let r = send(
            &other,
            "PUT",
            &format!("{location}?digest={d}"),
            Some(&t),
            Some("application/octet-stream"),
            "def",
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let request = Request::builder()
            .uri(format!("/v2/team/project/blobs/{d}"))
            .header(header::RANGE, "bytes=2-4")
            .body(Body::empty())
            .unwrap();
        let r = app.clone().oneshot(request).await.unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(bytes(r).await, b"cde");
        let request = Request::builder()
            .uri(format!("/v2/team/project/blobs/{d}"))
            .header(header::RANGE, "bytes=100-")
            .body(Body::empty())
            .unwrap();
        let r = app.clone().oneshot(request).await.unwrap();
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let r = send(
            &app,
            "POST",
            "/v2/team/project/blobs/uploads/",
            Some(&t),
            None,
            Body::empty(),
        )
        .await;
        let l = r.headers()[header::LOCATION].to_str().unwrap().to_owned();
        assert_eq!(
            send(&app, "DELETE", &l, Some(&t), None, Body::empty())
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            send(&app, "GET", &l, Some(&t), None, Body::empty())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    #[tokio::test]
    async fn exchange_scope_and_bad_paths() {
        let (_root, state, app) = app();
        let r=send(&app,"GET","/auth/token?service=https%3A%2F%2Fregistry.example.com&scope=repository:team/project:pull",None,None,Body::empty()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
        let t = token(&state, "team/project", 40).await;
        assert_eq!(
            send(
                &app,
                "POST",
                "/v2/team/project-other/blobs/uploads/",
                Some(&t),
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        for path in [
            "/v2/a/../b/tags/list",
            "/v2/a%2fb/tags/list",
            "/v2/a//b/tags/list",
        ] {
            assert_eq!(
                send(&app, "GET", path, None, None, Body::empty())
                    .await
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
    }
    #[tokio::test]
    async fn pagination_mount_negotiation_and_bad_chunk_offset() {
        let (_root, state, app) = app();
        let token = token(&state, "team/project", 40).await;
        let digest = crate::store::digest(b"{}");
        assert_eq!(
            send(
                &app,
                "POST",
                &format!("/v2/team/project/blobs/uploads/?digest={digest}"),
                Some(&token),
                Some("application/octet-stream"),
                "{}"
            )
            .await
            .status(),
            StatusCode::CREATED
        );
        let body = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":crate::store::OCI_MANIFEST,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":digest,"size":2},"layers":[]})).unwrap();
        for tag in ["b", "a"] {
            assert_eq!(
                send(
                    &app,
                    "PUT",
                    &format!("/v2/team/project/manifests/{tag}"),
                    Some(&token),
                    Some(crate::store::OCI_MANIFEST),
                    body.clone()
                )
                .await
                .status(),
                StatusCode::CREATED
            );
        }
        let first = send(
            &app,
            "GET",
            "/v2/team/project/tags/list?n=1",
            None,
            None,
            Body::empty(),
        )
        .await;
        let link = first.headers()[header::LINK].to_str().unwrap().to_owned();
        let value: serde_json::Value = serde_json::from_slice(&bytes(first).await).unwrap();
        assert_eq!(value["tags"], json!(["a"]));
        assert!(link.contains("last=a"));
        let second = send(
            &app,
            "GET",
            "/v2/team/project/tags/list?n=1&last=a",
            None,
            None,
            Body::empty(),
        )
        .await;
        assert!(!second.headers().contains_key(header::LINK));
        let value: serde_json::Value = serde_json::from_slice(&bytes(second).await).unwrap();
        assert_eq!(value["tags"], json!(["b"]));
        let request = Request::builder()
            .uri("/v2/team/project/manifests/a")
            .header(
                header::ACCEPT,
                "application/vnd.docker.distribution.manifest.v1+json",
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::NOT_ACCEPTABLE
        );
        let child_token = super::protocol_tests::token(&state, "team/project/worker", 40).await;
        let mounted = send(
            &app,
            "POST",
            &format!("/v2/team/project/worker/blobs/uploads/?mount={digest}&from=team%2Fproject"),
            Some(&child_token),
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(mounted.status(), StatusCode::CREATED);
        assert_eq!(
            send(
                &app,
                "GET",
                &format!("/v2/team/project/worker/blobs/{digest}"),
                None,
                None,
                Body::empty()
            )
            .await
            .status(),
            StatusCode::OK
        );
        let started = send(
            &app,
            "POST",
            "/v2/team/project/blobs/uploads/",
            Some(&token),
            None,
            Body::empty(),
        )
        .await;
        let location = started.headers()[header::LOCATION].to_str().unwrap();
        let request = Request::builder()
            .method("PATCH")
            .uri(location)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_RANGE, "5-7")
            .body(Body::from("abc"))
            .unwrap();
        let rejected = app.clone().oneshot(request).await.unwrap();
        assert_eq!(rejected.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(rejected.headers()[header::RANGE], "0-0");
        let denied = send(
            &app,
            "POST",
            "/v2/team/project/blobs/uploads/",
            None,
            None,
            Body::empty(),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(state.metrics.auth_failures.load(Ordering::Relaxed) > 0);
    }
}
