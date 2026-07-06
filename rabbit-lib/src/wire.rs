use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 2;
pub const TERM_CHAN_CLAUDE: u8 = 0x01;
/// §D Milestone 5: secondary terminal channel for `/agent/:id/shell`.
/// A `bash` PTY on the same rabbit, distinct from the main Claude
/// channel so it can be subscribed to (and written to) independently.
pub const TERM_CHAN_SHELL: u8 = 0x02;

/// §A.7 / seq-numbered snapshot protocol — one server→browser binary
/// term-stream chunk, carrying the channel byte, the per-channel seq
/// that the producer (the blocking PTY reader thread or shell reader)
/// assigned it, and the raw PTY bytes for that chunk. The replay
/// buffer stores `TermFrame`s so the seq rides through reconnects; warren
/// relays each frame verbatim (byte-for-byte: same `chan`, same `seq`,
/// same `data`) to its browser subscribers and to the asciicast
/// recorder.
#[derive(Debug, Clone)]
pub struct TermFrame {
    pub chan: u8,
    pub seq: u64,
    pub data: Vec<u8>,
}

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
    /// §A.6 leader-based resize: server sends each browser a unique
    /// `connection_id` on WS open so the browser can identify itself in
    /// subsequent `ClaimLeader` / `ReleaseLeader` / `Resize` envelopes.
    /// Per-connection (sent directly on the WS, not broadcast).
    ConnectionAssigned {
        connection_id: Uuid,
    },
    /// §A.6 leader-based resize: browser requests leadership for its
    /// connection_id. Server replies via `LeaderChanged`. The browser
    /// reports its current xterm grid so the server can adopt that size
    /// for the kernel PTY (or skip if it matches the current size).
    ClaimLeader {
        cols: u16,
        rows: u16,
    },
    /// §A.6 leader-based resize: leader releases control voluntarily.
    /// Other browsers receive `LeaderChanged { leader_id: None }`.
    ReleaseLeader,
    /// §A.6 leader-based resize: broadcast to every connected browser on
    /// every leader transition (initial claim, transfer, release, leader
    /// disconnect). `leader_id = None` means "no leader right now."
    LeaderChanged {
        leader_id: Option<Uuid>,
        cols: u16,
        rows: u16,
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
    /// §A.7 / seq-numbered snapshot protocol — per-`chan` counter of the
    /// last byte whose cells are *fully represented* in `text`. `0` means
    /// "no bytes fed yet on this channel"; a positive value tells the
    /// browser which buffered live frames are already covered by the
    /// snapshot and can be discarded before the apply. `#[serde(default)]`
    /// keeps v1 envelopes (which had no `after_seq`) deserializable.
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloUp {
    pub agent_id: uuid::Uuid,
    pub protocol_v: u32,
    pub claude_version: String,
    pub session_id: Option<String>,
    /// Typed state per §6 of the migration plan — supervisor emits
    /// `AgentState` (snake_case on the wire: `starting`, `idle`,
    /// `running`, `ended`, `dead`) directly rather than free-form
    /// strings.
    pub state: AgentState,
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
    pub state: AgentState,
    pub session_id: Option<String>,
    pub reason: Option<String>,
}

/// §6 of the migration plan — the canonical typed state enum.
/// Serializes as snake_case strings (`starting` / `idle` / `running`
/// / `ended` / `dead`); the JSON wire shape is identical to the old
/// free-form `String` field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Idle,
    Running,
    Ended,
    Dead,
}

impl AgentState {
    /// Snake-case label, e.g. `AgentState::Idle` → `"idle"`. Used by
    /// log lines and SSE payloads that predate the typed enum.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Starting => "starting",
            AgentState::Idle => "idle",
            AgentState::Running => "running",
            AgentState::Ended => "ended",
            AgentState::Dead => "dead",
        }
    }
}

impl std::str::FromStr for AgentState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "starting" => Ok(AgentState::Starting),
            "idle" => Ok(AgentState::Idle),
            "running" => Ok(AgentState::Running),
            "ended" => Ok(AgentState::Ended),
            "dead" => Ok(AgentState::Dead),
            other => Err(format!("unknown agent state: {other}")),
        }
    }
}

impl From<&str> for AgentState {
    fn from(s: &str) -> Self {
        s.parse().unwrap_or(AgentState::Starting)
    }
}

/// §6 — the supervisor's hello is the same shape as the broker's
/// hello. They used to live in two crates as `HelloUp` and
/// `HelloDown`; unifying the state field lets us collapse them into
/// one type. The legacy alias is preserved so existing call sites
/// keep working.
pub type HelloDown = HelloUp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    //! §A.7 — serde roundtrip for `ScreenSnapshotBody::after_seq`. The
    //! field is added with `#[serde(default)]` so v1 envelopes (which
    //! had no `after_seq` key) still deserialize cleanly under a v2
    //! struct during the rollout window. These tests pin that property
    //! so a future "tighten the derive" refactor can't silently break
    //! cross-version reads.

    use super::*;

    #[test]
    fn screen_snapshot_body_v2_serializes_after_seq_field() {
        let body = ScreenSnapshotBody {
            chan: 0x01,
            cols: 80,
            rows: 24,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            text: vec!["".into()],
            after_seq: 42,
        };
        let v = serde_json::to_value(&body).expect("serialize");
        assert_eq!(v["after_seq"], 42);
    }

    #[test]
    fn screen_snapshot_body_v1_json_without_after_seq_deserializes_to_zero() {
        // A v1 producer never emitted `after_seq`; the v2 struct must
        // tolerate its absence (otherwise a mixed-version rollout would
        // fail to parse the older side's envelopes).
        let v1_json = serde_json::json!({
            "chan": 0x01,
            "cols": 80,
            "rows": 24,
            "cursor_col": 0,
            "cursor_row": 0,
            "cursor_visible": true,
            "text": [""],
        });
        let body: ScreenSnapshotBody = serde_json::from_value(v1_json)
            .expect("v1 envelope must deserialize under a v2 struct");
        assert_eq!(body.after_seq, 0);
    }
}
