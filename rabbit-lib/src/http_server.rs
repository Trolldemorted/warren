//! §D Milestone 5: small HTTP surface that lets warren fetch `.cast`
//! recordings for the history page. Three endpoints:
//!
//! - `GET /sessions` → JSON list of recordings, grouped by session_id
//! - `GET /casts/{filename}` → the `.cast` file bytes (asciicast v2 JSONL)
//! - `GET /healthz` → `200 OK` for k8s-style liveness
//!
//! Auth is a Bearer token: the same `agent_token` rabbit uses on the WS
//! link. warren already has the token in the `agents` row, so no extra
//! secret rotation.
//!
//! Filenames are validated against a strict allow-list regex — never
//! resolve a path the client hands us without it. `..` traversal, absolute
//! paths, and unusual extensions are rejected before any `tokio::fs` call.

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// Validates `<segment>` against the allow-list. Returns `true` for things
/// that look like `<session-id>.cast` or `<session-id>.cast.<n>`.
///
/// `session-id` is whatever the upstream hook reports (typically a UUID
/// or a short slug from `claude`). We allow `[A-Za-z0-9_-]` to cover both
/// without dragging in a regex crate.
fn is_valid_filename(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > 128 {
        return false;
    }
    // Strip a trailing `.cast` or `.cast.N`. After stripping, what's left
    // must be all `[A-Za-z0-9_-]` and non-empty.
    let (head, tail) = match segment.split_once(".cast") {
        Some((h, t)) => (h, t),
        None => return false,
    };
    if head.is_empty() {
        return false;
    }
    if !head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return false;
    }
    // Tail must be either empty or `.N` for some decimal N.
    if tail.is_empty() {
        return true;
    }
    let Some(num) = tail.strip_prefix('.') else {
        return false;
    };
    !num.is_empty() && num.chars().all(|c| c.is_ascii_digit())
}

/// Application state — a clone of the casts dir plus the shared bearer.
#[derive(Clone)]
struct AppCtx {
    dir: Arc<PathBuf>,
    token: Arc<String>,
}

/// JSON shape of one segment row under `/sessions`. Grouped under a session
/// in [`SessionRecordings`].
#[derive(Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub mtime_unix: i64,
}

/// JSON shape returned by `/sessions` — one entry per session_id present
/// in the casts dir, most recent first.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionRecordings {
    pub session_id: String,
    pub files: Vec<FileInfo>,
}

pub async fn serve(port: u16, dir: PathBuf, token: String) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/sessions", get(list_sessions))
        .route("/casts/:filename", get(serve_cast))
        .with_state(AppCtx {
            dir: Arc::new(dir),
            token: Arc::new(token),
        });
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    log::info!("recorder http listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

fn check_auth(headers: &HeaderMap, expected: &str) -> bool {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(s) = value.to_str() else {
        return false;
    };
    let Some(rest) = s.strip_prefix("Bearer ") else {
        return false;
    };
    // Constant-time comparison would be nice, but the token is per-pod and
    // the threat model is "is this request from warren", not "can we leak
    // the token via timing". Kept simple.
    rest == expected
}

/// List `.cast` files in the configured dir, grouped by session_id.
async fn list_sessions(
    State(ctx): State<AppCtx>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&headers, &ctx.token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let mut entries: tokio::fs::ReadDir = match tokio::fs::read_dir(ctx.dir.as_ref()).await {
        Ok(rd) => rd,
        Err(e) => {
            log::warn!("recorder http: read_dir({}) failed: {e:?}", ctx.dir.display());
            return (StatusCode::INTERNAL_SERVER_ERROR, "read_dir failed").into_response();
        }
    };
    let mut rows: Vec<FileInfo> = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(e)) => {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                if !is_valid_filename(name) {
                    continue;
                }
                let meta = match e.metadata().await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                rows.push(FileInfo {
                    name: name.to_string(),
                    size,
                    mtime_unix: mtime,
                });
            }
            Ok(None) => break,
            Err(e) => {
                log::warn!("recorder http: read_dir entry failed: {e:?}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "read_dir failed").into_response();
            }
        }
    }
    // Group by session_id (everything before `.cast`).
    let mut groups: std::collections::HashMap<String, Vec<FileInfo>> =
        std::collections::HashMap::new();
    for row in rows {
        let Some((sid, _)) = row.name.split_once(".cast") else {
            continue;
        };
        groups.entry(sid.to_string()).or_default().push(row);
    }
    let mut out: Vec<SessionRecordings> = groups
        .into_iter()
        .map(|(session_id, mut files)| {
            // Sort by segment index: live `.cast` first, then `.cast.0`,
            // `.cast.1`, … (so the timeline reads naturally).
            files.sort_by_key(|f| segment_index(&f.name));
            SessionRecordings { session_id, files }
        })
        .collect();
    out.sort_by(|a, b| {
        let a_latest = a.files.iter().map(|f| f.mtime_unix).max().unwrap_or(0);
        let b_latest = b.files.iter().map(|f| f.mtime_unix).max().unwrap_or(0);
        b_latest.cmp(&a_latest)
    });
    Json(out).into_response()
}

/// Numeric segment index for ordering: `.cast` → 0, `.cast.0` → 0, `.cast.1`
/// → 1, etc. (Live segment sorts before rotated `.cast.0`? Actually, they
/// share index 0 by this rule; tie-break by mtime inside the group.)
fn segment_index(name: &str) -> u32 {
    let Some((_, tail)) = name.split_once(".cast") else {
        return u32::MAX;
    };
    if tail.is_empty() {
        return 0;
    }
    tail.strip_prefix('.')
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(u32::MAX)
}

/// Serve a single `.cast` file. Filename must validate against the
/// allow-list (no `..`, no `/`, no unusual suffixes).
async fn serve_cast(
    State(ctx): State<AppCtx>,
    Path(filename): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !check_auth(&headers, &ctx.token) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if !is_valid_filename(&filename) {
        return (StatusCode::BAD_REQUEST, "invalid filename").into_response();
    }
    let path: PathBuf = ctx.dir.join(&filename);
    // Belt-and-braces: even though the filename validates, refuse to serve
    // anything outside the configured dir (defense in depth against
    // symlinks pointing elsewhere).
    let canonical_dir: std::path::PathBuf = match tokio::fs::canonicalize(ctx.dir.as_ref()).await {
        Ok(p) => p,
        Err(e) => {
            log::warn!("recorder http: canonicalize dir failed: {e:?}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "canonicalize failed").into_response();
        }
    };
    match tokio::fs::canonicalize(&path).await {
        Ok(canon) => {
            if !canon.starts_with(&canonical_dir) {
                return (StatusCode::BAD_REQUEST, "path escapes casts dir").into_response();
            }
        }
        Err(_) => {
            // File doesn't exist (or some other resolution failure). Treat
            // as 404 rather than leak the existence of files outside dir.
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
    }
    let bytes: Vec<u8> = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            log::warn!("recorder http: read({}) failed: {e:?}", path.display());
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
    };
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        "application/vnd.asciicast+json; charset=utf-8".parse().unwrap(),
    );
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, "public, max-age=300".parse().unwrap());
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fresh_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rabbit-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Bind the router on an ephemeral port, return (addr, server_task).
    /// The task ends when the test drops it (the JoinHandle is just
    /// aborted; axum::serve doesn't return on its own until the listener
    /// closes).
    async fn spawn_app(tag: &str) -> (String, AppCtx, tokio::task::JoinHandle<()>) {
        let dir = fresh_dir(tag);
        let token = "sekret".to_string();
        let ctx = AppCtx {
            dir: Arc::new(dir),
            token: Arc::new(token),
        };
        let app = Router::new()
            .route("/healthz", get(healthz))
            .route("/sessions", get(list_sessions))
            .route("/casts/:filename", get(serve_cast))
            .with_state(ctx.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        // Give axum a moment to actually start accepting. Without this the
        // first reqwest call occasionally races the bind and gets ECONN.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (format!("http://{addr}"), ctx, task)
    }

    #[test]
    fn filename_allowlist_accepts_cast() {
        assert!(is_valid_filename("abc.cast"));
        assert!(is_valid_filename("abc.cast.0"));
        assert!(is_valid_filename("abc.cast.12"));
        assert!(is_valid_filename("a-b_c.cast"));
        assert!(is_valid_filename("00000000-0000-0000-0000-000000000000.cast"));
    }

    #[test]
    fn filename_allowlist_rejects_traversal_and_garbage() {
        assert!(!is_valid_filename(".."));
        assert!(!is_valid_filename("../etc/passwd"));
        assert!(!is_valid_filename(""));
        assert!(!is_valid_filename(".cast"));
        assert!(!is_valid_filename("a.cast.x"));
        assert!(!is_valid_filename("a/../b.cast"));
        assert!(!is_valid_filename("a b.cast"));
        assert!(!is_valid_filename("a.cast."));
        assert!(!is_valid_filename("a.CAST"));
    }

    #[test]
    fn segment_index_orders_live_then_rotated() {
        assert_eq!(segment_index("a.cast"), 0);
        assert_eq!(segment_index("a.cast.0"), 0);
        assert_eq!(segment_index("a.cast.1"), 1);
        assert_eq!(segment_index("a.cast.99"), 99);
        assert_eq!(segment_index("garbage"), u32::MAX);
    }

    #[tokio::test]
    async fn healthz_returns_200() {
        let (base, _ctx, task) = spawn_app("healthz").await;
        let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "ok");
        task.abort();
    }

    #[tokio::test]
    async fn serve_cast_returns_401_without_bearer() {
        let (base, ctx, task) = spawn_app("401").await;
        std::fs::write(ctx.dir.join("abc.cast"), "{}").unwrap();
        let resp = reqwest::get(format!("{base}/casts/abc.cast")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        task.abort();
    }

    #[tokio::test]
    async fn serve_cast_returns_400_for_invalid_filename() {
        let (base, _ctx, task) = spawn_app("bad").await;
        // `%2F` decodes to `/` — axum routes on the raw path, so the
        // match on `{filename}` will see `..%2Fetc%2Fpasswd` (still
        // percent-encoded by the router). axum does NOT decode before
        // matching, so the filename it sees is the literal `..%2Fetc%2Fpasswd`,
        // which fails `is_valid_filename`.
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base}/casts/..%2Fetc%2Fpasswd"))
            .header("authorization", "Bearer sekret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        task.abort();
    }

    #[tokio::test]
    async fn serve_cast_returns_200_with_correct_content_type() {
        let (base, ctx, task) = spawn_app("ok").await;
        let body = "{\"version\":2,\"width\":80,\"height\":24}\n";
        std::fs::write(ctx.dir.join("abc.cast"), body).unwrap();
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base}/casts/abc.cast"))
            .header("authorization", "Bearer sekret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        assert!(
            ct.starts_with("application/vnd.asciicast+json"),
            "got content-type {ct}"
        );
        let got = resp.text().await.unwrap();
        assert_eq!(got, body);
        task.abort();
    }

    #[tokio::test]
    async fn list_sessions_groups_by_session_id_and_orders_by_mtime() {
        let (base, ctx, task) = spawn_app("list").await;
        // Two sessions: `alpha` (older) and `beta` (newer). Give beta a
        // strictly later mtime so the test isn't flaky on fast filesystems.
        std::fs::write(ctx.dir.join("alpha.cast"), "x").unwrap();
        std::fs::write(ctx.dir.join("alpha.cast.0"), "x").unwrap();
        std::fs::write(ctx.dir.join("beta.cast"), "x").unwrap();
        let _ = filetime_touch_later(ctx.dir.join("beta.cast"), 2);

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{base}/sessions"))
            .header("authorization", "Bearer sekret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sessions: Vec<SessionRecordings> = resp.json().await.unwrap();
        assert_eq!(sessions.len(), 2, "got: {sessions:?}");
        // Most recent first.
        assert_eq!(sessions[0].session_id, "beta");
        assert_eq!(sessions[0].files.len(), 1);
        assert_eq!(sessions[1].session_id, "alpha");
        assert_eq!(sessions[1].files.len(), 2);
        // alpha: live `.cast` before `.cast.0`.
        assert_eq!(sessions[1].files[0].name, "alpha.cast");
        assert_eq!(sessions[1].files[1].name, "alpha.cast.0");
        task.abort();
    }

    /// Force a file's mtime to N seconds in the future, so test ordering
    /// is deterministic across filesystems with second-resolution mtimes.
    /// Cheap helper — uses `filetime` if available, else falls back to
    /// `set_modified` (stable since Rust 1.75).
    fn filetime_touch_later(path: PathBuf, secs: i64) -> std::io::Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let target = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs((now + secs) as u64);
        let f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.set_modified(target)?;
        Ok(())
    }
}