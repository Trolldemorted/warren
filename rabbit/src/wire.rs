use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const TERM_CHAN_CLAUDE: u8 = 0x01;
/// §D Milestone 5: secondary terminal channel for `/agent/:id/shell`.
/// A `bash` PTY on the same rabbit, distinct from the main Claude
/// channel so it can be subscribed to (and written to) independently.
/// Mirrors `warren::agents_live::wire::TERM_CHAN_SHELL`.
pub const TERM_CHAN_SHELL: u8 = 0x02;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub seq: i64,
    #[serde(flatten)]
    pub body: EnvelopeBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum EnvelopeBody {
    Hello(HelloUp),
    Ack {
        ack_seq: i64,
    },
    State(StateFrame),
    PromptEcho(PromptEcho),
    TurnDone(TurnDone),
    Usage(UsageSnapshot),
    Cleared {
        hard: bool,
    },
    Session(SessionInfo),
    TranscriptMsg {
        message: serde_json::Value,
    },
    Log(LogLine),
    Pong,
    Prompt {
        id: uuid::Uuid,
        text: String,
        by: String,
    },
    /// §D reject-when-Running outcome: a `Prompt` arrived while the agent
    /// was already `Running`, so the supervisor bounced it instead of
    /// injecting keystrokes into a live turn. Distinct from a generic
    /// `Log { level: "warn" }` so warren can surface a dedicated UI
    /// affordance (e.g. an inline banner tied to the original prompt id).
    /// `reason` is human-readable; today the only value is
    /// `"agent is running a turn"`.
    PromptRejected {
        id: uuid::Uuid,
        reason: String,
    },
    Slash {
        cmd: String,
    },
    Interrupt,
    Clear {
        hard: bool,
    },
    Restart {
        fresh: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Repaint,
    StopHook {
        prompt_id: uuid::Uuid,
        usage: Option<UsageSnapshot>,
        error: Option<String>,
    },
    /// §D Milestone 5 (Phase B): late-join screen dump. Sent by rabbit in
    /// response to a [`SnapshotRequest`] from warren so a fresh browser pane
    /// can paint an authoritative terminal state instead of relying on the
    /// SIGWINCH jiggle. `text.len() == rows`; each string is the VT's own
    /// space-padded grid row.
    ScreenSnapshot(ScreenSnapshotBody),
    /// §D Milestone 5 (Phase B): warren asks rabbit for a snapshot of the
    /// given channel's VT. Used by `ws_browser` after the replay buffer
    /// has been pushed into xterm.js.
    SnapshotRequest {
        chan: u8,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenSnapshotBody {
    pub chan: u8,
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_visible: bool,
    pub text: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloUp {
    pub agent_id: uuid::Uuid,
    pub protocol_v: u32,
    pub claude_version: String,
    pub session_id: Option<String>,
    pub state: String,
    pub term_size: TermSize,
    /// §D Milestone 5: absolute base URL of rabbit's recorder HTTP server
    /// (e.g. `http://10.0.0.42:7790`), populated when `enable_asciicast=1`
    /// and warren hits `/agent/:id/claude/history`. Empty/None when the
    /// recorder is off — handlers must check rather than assume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorder_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFrame {
    pub state: String,
    pub session_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptEcho {
    pub prompt_id: uuid::Uuid,
    pub text: String,
    pub by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnDone {
    pub prompt_id: uuid::Uuid,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub ended_at: chrono::DateTime<chrono::Utc>,
    pub usage: Option<UsageSnapshot>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub context_pct_est: Option<f64>,
    /// Cumulative count of transcript JSONL lines that failed to parse since
    /// rabbit started. §A.3 requires this counter so warren can alert on
    /// drift in the on-disk format; it is *not* fatal and never blocks the
    /// terminal plane. Surfaced via the next `Usage` envelope after each
    /// increment.
    #[serde(default)]
    pub parse_errors: u64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub resumed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub level: String,
    pub message: String,
}
