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

pub struct TranscriptTail {
    path: PathBuf,
}

impl TranscriptTail {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub async fn run(self, tx: mpsc::Sender<UsageUpdate>, poll_ms: u64) -> Result<()> {
        let mut last_pos: u64 = 0;
        let mut last_message: Option<String> = None;
        loop {
            match self.scan_once(&tx, &mut last_pos, &mut last_message) {
                Ok(()) => {}
                Err(e) => log::debug!("transcript scan: {e:?}"),
            }
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;
        }
    }

    fn scan_once(
        &self,
        tx: &mpsc::Sender<UsageUpdate>,
        last_pos: &mut u64,
        last_message: &mut Option<String>,
    ) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let size = std::fs::metadata(&self.path)?.len();
        if size < *last_pos {
            *last_pos = 0;
            *last_message = None;
        }
        if size == *last_pos {
            return Ok(());
        }
        let mut f = std::fs::File::open(&self.path)?;
        f.seek(SeekFrom::Start(*last_pos))?;
        let reader = BufReader::new(f);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            *last_pos += line.len() as u64 + 1;
            if line.trim().is_empty() {
                continue;
            }
            let parsed: Result<JsonlRow, _> = serde_json::from_str(&line);
            let parsed = match parsed {
                Ok(p) => p,
                Err(_) => continue,
            };
            if let Some(msg) = parsed.message {
                if let Some(model) = msg.model {
                    *last_message = Some(model);
                }
                let usage = match msg.usage {
                    Some(u) => u,
                    None => continue,
                };
                let snap = UsageSnapshot {
                    input_tokens: usage.input_tokens.unwrap_or(0),
                    output_tokens: usage.output_tokens.unwrap_or(0),
                    cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                    cache_write: usage.cache_creation_input_tokens.unwrap_or(0),
                    context_pct_est: None,
                    source: "transcript".to_string(),
                };
                let kind = msg.role.unwrap_or_else(|| "unknown".to_string());
                let _ = tx.blocking_send(UsageUpdate {
                    usage: snap,
                    message_kind: kind,
                });
            }
        }
        Ok(())
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
