use crate::observer::hooks::ObserverHandle;
use crate::wire::UsageSnapshot;
use anyhow::Result;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct UsageUpdate {
    pub usage: UsageSnapshot,
    #[allow(dead_code)]
    pub message_kind: String,
}

/// Tail-follower for the on-disk transcript jsonl written by claude
/// (§A.3). It does *not* own a fixed path: it consults a path-provider on
/// every scan, so the real path — reported by `SessionStart` as
/// `transcript_path` in the hook payload — replaces the fallback once the
/// hook fires. Before then, the fallback is used (which usually does not
/// exist yet, and is silently skipped).
pub struct TranscriptTail {
    path_provider: Box<dyn Fn() -> Option<PathBuf> + Send + Sync>,
}

impl TranscriptTail {
    /// Construct a tailer that follows the observer-reported transcript path
    /// (`SessionStart` payload), falling back to `fallback` while the path
    /// is still unknown (i.e. before claude has emitted its first hook).
    pub fn with_observer(observer: ObserverHandle, fallback: PathBuf) -> Self {
        Self {
            path_provider: Box::new(move || {
                Some(
                    observer
                        .latest_transcript_path()
                        .unwrap_or_else(|| fallback.clone()),
                )
            }),
        }
    }

    pub async fn run(self, tx: mpsc::Sender<UsageUpdate>, poll_ms: u64) -> Result<()> {
        // Per-path scan state: as the transcript path can change across
        // session restarts, we key last_pos and the current model on the
        // path itself. A new path (different cwd / new session id) starts
        // fresh.
        let mut last_pos: std::collections::HashMap<PathBuf, u64> =
            std::collections::HashMap::new();
        let mut current_model: std::collections::HashMap<PathBuf, Option<String>> =
            std::collections::HashMap::new();
        let mut last_seen_path: Option<PathBuf> = None;
        let mut parse_errors: u64 = 0;
        loop {
            let path = (self.path_provider)();
            // Collect updates synchronously, then hand them to the channel with
            // an async `send`. We deliberately do NOT use `tx.blocking_send`
            // here: `run` executes on a Tokio task, and `blocking_send` panics
            // when called from an async context. Buffering into a Vec keeps the
            // parse loop sync while the await happens out here.
            let mut batch: Vec<UsageUpdate> = Vec::new();
            match self.scan_once(
                path.as_deref(),
                &mut batch,
                &mut last_pos,
                &mut current_model,
                &mut last_seen_path,
                &mut parse_errors,
            ) {
                Ok(()) => {}
                Err(e) => log::debug!("transcript scan: {e:?}"),
            }
            for update in batch {
                if tx.send(update).await.is_err() {
                    // Receiver gone; nothing left to feed. Stop tailing.
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_once(
        &self,
        path: Option<&Path>,
        out: &mut Vec<UsageUpdate>,
        last_pos: &mut std::collections::HashMap<PathBuf, u64>,
        current_model: &mut std::collections::HashMap<PathBuf, Option<String>>,
        last_seen_path: &mut Option<PathBuf>,
        parse_errors: &mut u64,
    ) -> Result<()> {
        let path = match path {
            Some(p) => p,
            None => return Ok(()),
        };
        if !path.exists() {
            return Ok(());
        }
        // If the path changed (new session, different cwd), reset per-path
        // state so we don't replay bytes from the previous file.
        if last_seen_path.as_deref() != Some(path) {
            last_pos.remove(path);
            current_model.remove(path);
            *last_seen_path = Some(path.to_path_buf());
        }
        let size = std::fs::metadata(path)?.len();
        let pos = last_pos.get(path).copied().unwrap_or(0);
        if size < pos {
            last_pos.insert(path.to_path_buf(), 0);
        }
        let pos = *last_pos.get(path).unwrap_or(&0);
        if size == pos {
            return Ok(());
        }
        let mut f = std::fs::File::open(path)?;
        f.seek(SeekFrom::Start(pos))?;
        let reader = BufReader::new(f);
        let mut new_pos = pos;
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    *parse_errors += 1;
                    continue;
                }
            };
            new_pos += line.len() as u64 + 1;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Result<JsonlRow, _> = serde_json::from_str(&line);
            let parsed = match parsed {
                Ok(p) => p,
                Err(_) => {
                    *parse_errors += 1;
                    continue;
                }
            };
            let Some(msg) = parsed.message else {
                continue;
            };
            if let Some(model) = msg.model {
                current_model.insert(path.to_path_buf(), Some(model));
            }
            let usage = match msg.usage {
                Some(u) => u,
                None => continue,
            };
            let input = usage.input_tokens.unwrap_or(0);
            let output = usage.output_tokens.unwrap_or(0);
            let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
            let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
            let model_name = current_model
                .get(path)
                .and_then(|m| m.clone())
                .unwrap_or_default();
            let context_pct_est = context_pct(&model_name, input, cache_read);
            let snap = UsageSnapshot {
                input_tokens: input,
                output_tokens: output,
                cache_read,
                cache_write,
                context_pct_est,
                parse_errors: *parse_errors,
                source: "transcript".to_string(),
            };
            let kind = msg.role.unwrap_or_else(|| "unknown".to_string());
            out.push(UsageUpdate {
                usage: snap,
                message_kind: kind,
            });
        }
        last_pos.insert(path.to_path_buf(), new_pos);
        Ok(())
    }
}

/// Estimate how full the model's context window is, as a percentage in
/// `[0.0, 100.0]`. §A.3 asks for "last turn's input tokens vs. the model's
/// window size"; we include cache_read because the cached tokens are also
/// occupying context, just cheaply. Unknown models default to 200k, which
/// is the conservative baseline for the Claude 3/3.5 family.
pub fn context_pct(model: &str, input_tokens: u64, cache_read_input_tokens: u64) -> Option<f64> {
    let window = model_window(model);
    if window == 0 {
        return None;
    }
    let used = input_tokens.saturating_add(cache_read_input_tokens);
    let pct = (used as f64) * 100.0 / (window as f64);
    Some(pct.clamp(0.0, 100.0))
}

fn model_window(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    // Sonnet 4 ships with a 1M-context beta. Treat the whole family as 1M
    // for the purposes of an *estimate*; if the operator is on the 200k tier
    // without the beta header the estimate will simply read higher than
    // reality, which is the right direction for an alert.
    if m.starts_with("claude-sonnet-4") {
        1_000_000
    } else {
        // Opus 4, the Claude 3/3.5 family, and unknown models all default to
        // 200k. Kept as a single fallthrough branch to stay clippy-clean.
        200_000
    }
}

#[derive(Debug, Deserialize)]
struct JsonlRow {
    #[serde(default)]
    message: Option<MsgObj>,
}

#[derive(Debug, Deserialize)]
struct MsgObj {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<UsageObj>,
}

#[derive(Debug, Deserialize)]
struct UsageObj {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

pub fn default_transcript_path(workdir: &Path) -> PathBuf {
    workdir.join(".claude").join("transcript.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_pct_zero_when_empty() {
        assert_eq!(context_pct("claude-3-5-sonnet-20240620", 0, 0), Some(0.0));
    }

    #[test]
    fn context_pct_uses_input_plus_cache_read() {
        // 100k tokens used out of a 200k window → 50%
        let pct = context_pct("claude-3-5-sonnet-20240620", 60_000, 40_000).unwrap();
        assert!((pct - 50.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn context_pct_caps_at_100() {
        let pct = context_pct("claude-3-5-sonnet-20240620", 300_000, 0).unwrap();
        assert_eq!(pct, 100.0);
    }

    #[test]
    fn context_pct_uses_1m_window_for_sonnet_4() {
        // 200k tokens out of 1M window → 20%
        let pct = context_pct("claude-sonnet-4-20250514", 200_000, 0).unwrap();
        assert!((pct - 20.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn context_pct_unknown_model_defaults_to_200k() {
        let pct = context_pct("mystery-model", 100_000, 0).unwrap();
        assert!((pct - 50.0).abs() < 0.01, "got {pct}");
    }
}
