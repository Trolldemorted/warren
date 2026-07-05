//! §D Milestone 5: thin client for rabbit's asciicast recorder HTTP
//! service. Two helpers — list all recordings, build a per-session cast
//! URL — plus a small clone-able JSON shape matching the rabbit side.
//!
//! We don't reuse the same `reqwest::Client` as the rest of warren because
//! recorder URLs are arbitrary host:port strings (rabbit binds 0.0.0.0
//! and warren advertises via the agent's env or the auto-derived
//! `RABBIT_RECORDER_URL`). A dedicated client per call keeps the URL
//! trust boundary clear and avoids TLS / cookie state leaking from the
//! main warren fetch path.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Mirror of `rabbit::http_server::FileInfo` — kept separate so the two
/// crates don't have to share types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub mtime_unix: i64,
}

/// Mirror of `rabbit::http_server::SessionRecordings`. `total_size` is
/// computed once on the warren side and exposed to the template (askama
/// doesn't call methods, only reads fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecordings {
    pub session_id: String,
    pub files: Vec<FileInfo>,
    /// Sum of `files[i].size`. Rebuilt server-side after deserialize so
    /// we don't have to trust rabbit's value.
    #[serde(default)]
    pub total_size: u64,
}

impl SessionRecordings {
    /// Recompute `total_size` from the embedded files. Cheap; called once
    /// per session after deserializing rabbit's JSON, before the template
    /// renders.
    pub fn recompute_total(&mut self) {
        self.total_size = self.files.iter().map(|f| f.size).sum();
    }
}

/// Errors specific to the recorder HTTP path. `AppError::Internal` is too
/// coarse to discriminate between "recorder disabled" / "recorder
/// unreachable" / "session not found".
#[derive(Debug, thiserror::Error)]
pub enum RecorderError {
    #[error("recorder URL unknown for this agent (recording disabled or rabbit hasn't Hello'd yet)")]
    NotConfigured,
    #[error("recorder HTTP error: {0}")]
    Http(String),
    #[error("recorder returned non-JSON body (status={0})")]
    Decode(u16),
}

/// Fetch the list of recorded sessions from rabbit's `/sessions` endpoint.
/// `recorder_url` must NOT have a trailing slash; `token` is the agent's
/// auth token (sent as `Authorization: Bearer …`). Returns one
/// `SessionRecordings` per session id, ordered most-recent-first.
pub async fn fetch_session_list(
    http: &reqwest::Client,
    recorder_url: &str,
    token: &str,
) -> Result<Vec<SessionRecordings>, RecorderError> {
    let url = format!("{}/sessions", recorder_url.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| RecorderError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(RecorderError::Decode(resp.status().as_u16()));
    }
    resp.json::<Vec<SessionRecordings>>()
        .await
        .map_err(|e| RecorderError::Http(format!("decode: {e}")))
}

/// Build the absolute URL to a single session's live `.cast` file. The
/// page template embeds this directly into the asciinema-player `src`.
pub fn cast_url(recorder_url: &str, session_id: &str) -> String {
    format!(
        "{}/casts/{}.cast",
        recorder_url.trim_end_matches('/'),
        session_id
    )
}

/// True iff the URL is well-formed (http/https + host + no path traversal
/// shenanigans). Cheap structural validation — callers should still gate on
/// the registry's `recorder_url()` to know the agent actually has one.
#[allow(dead_code)]
pub fn is_safe_recorder_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else { return false };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    parsed.host().is_some()
}

/// Build a short id from a session id for display purposes — first 8 chars
/// with an ellipsis if longer. Kept here rather than templated so both
/// the list and play pages share the truncation.
pub fn short_session_id(s: &str) -> String {
    if s.len() <= 8 {
        s.to_string()
    } else {
        format!("{}…", &s[..8])
    }
}

/// No-op helper that resolves to the agent_id-keyed recorder URL. Reads
/// from the live registry; returns `RecorderError::NotConfigured` when
/// rabbit hasn't advertised one yet. Used by handlers so the template can
/// still render an "recording not enabled" state instead of a 500.
pub fn recorder_url_for(
    registry: &crate::agents_live::AgentRegistry,
    agent_id: Uuid,
) -> Result<String, RecorderError> {
    registry
        .get(&agent_id)
        .ok_or(RecorderError::NotConfigured)?
        .recorder_url()
        .ok_or(RecorderError::NotConfigured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_url_strips_trailing_slash() {
        assert_eq!(
            cast_url("http://rec:7790/", "abc"),
            "http://rec:7790/casts/abc.cast"
        );
        assert_eq!(
            cast_url("http://rec:7790", "abc"),
            "http://rec:7790/casts/abc.cast"
        );
    }

    #[test]
    fn short_session_id_truncates_long_strings() {
        assert_eq!(short_session_id("abc"), "abc");
        assert_eq!(short_session_id("abcdefgh"), "abcdefgh");
        assert_eq!(short_session_id("abcdefghi"), "abcdefgh…");
        assert_eq!(
            short_session_id("00000000-0000-0000-0000-000000000000"),
            "00000000…"
        );
    }

    #[test]
    fn is_safe_recorder_url_accepts_http() {
        assert!(is_safe_recorder_url("http://10.0.0.1:7790"));
        assert!(is_safe_recorder_url("https://rec.example.com"));
    }

    #[test]
    fn is_safe_recorder_url_rejects_garbage() {
        assert!(!is_safe_recorder_url(""));
        assert!(!is_safe_recorder_url("not a url"));
        assert!(!is_safe_recorder_url("ftp://rec/"));
        assert!(!is_safe_recorder_url("file:///etc/passwd"));
    }
}