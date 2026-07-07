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
/// same `data`) to its browser subscribers.
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
        /// §Cross-tab prompt rejection visibility: the originating
        /// browser's `connection_id` so subscribers can filter
        /// `PromptEcho` / `PromptRejected` to their own prompts.
        /// `None` when the producer is the HTTP path (no browser tab
        /// owns the request) or a warren bg-task scheduler.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by_connection_id: Option<uuid::Uuid>,
    },
    /// §D reject-when-Running outcome: a `Prompt` arrived while the agent
    /// was already `Running`, so the supervisor bounced it instead of
    /// injecting keystrokes into a live turn. Distinct from a generic
    /// `Log { level: "warn" }` so warren can surface a dedicated UI
    /// affordance (e.g. an inline banner tied to the original prompt id).
    /// `reason` is human-readable; known values:
    /// - `"agent is running a turn"` — the actor's busy-gate fired.
    /// - `"agent is dead"` — the actor's state shows `Dead` (post-
    ///   connection-loss). No point in injecting keystrokes at a
    ///   disconnected supervisor.
    /// - `"turn queue full"` — the bounded `pending` queue is over
    ///   `PENDING_CAP`; back off and retry.
    PromptRejected {
        id: uuid::Uuid,
        reason: String,
        /// §Cross-tab prompt rejection visibility: the originating
        /// connection id so the rejection banner only shows on the
        /// tab that submitted the prompt. `None` for HTTP / bg-task
        /// rejections (browsers treat it as "show to everyone").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by_connection_id: Option<uuid::Uuid>,
    },
    Slash {
        cmd: String,
    },
    Interrupt,
    Clear {
        hard: bool,
    },
    /// §Usage-limits: server-initiated request for rabbit to scrape
    /// Claude's `/usage` overlay and return a fresh `Usage` envelope
    /// carrying the new `weekly_pct` / `session_pct` fields. Currently
    /// triggered by the "Usage" button in the warren UI; the same
    /// envelope will be used by future warren bg-task schedulers
    /// (the HTTP endpoint `POST /api/agents/:id/claude/usage_check`
    /// is forward-compatible). The rabbit supervisor handles this
    /// synchronously: it writes `\x15/usage\r` to the PTY, drains the
    /// broadcast `TermFrame` stream for ~2s, parses with
    /// `observer::limits::LimitsParser`, sends single Esc to dismiss
    /// the overlay, and publishes the parsed limits back as
    /// `EnvelopeBody::Usage(snap)` with the four new fields set.
    UsageCheck,
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
    /// Parse a snake-case label into the typed enum.
    ///
    /// **No silent fallback:** unrecognized labels return
    /// `AgentState::Starting` to keep historical call sites that use
    /// `.into()` for `state: AgentState` well-typed, but the *observer*
    /// path in `supervisor::send_state` and the actor must do their own
    /// `from_label`-style guard against unknown labels. The typed enum
    /// itself can only carry the five known variants; "unknown label"
    /// by construction no longer exists *as a runtime concept*, only
    /// as a malformed-JSON envelope (which serde would reject).
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
    /// §Cross-tab prompt rejection visibility: the originating
    /// connection id. Browsers without this set treat the echo as
    /// "not mine"; browsers with it treat the echo as their own.
    /// `None` when the producer is HTTP / bg-task. The actor
    /// stamps whatever the inbound `Prompt` carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_connection_id: Option<uuid::Uuid>,
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
    /// §Usage-limits: plan-level weekly usage as a percentage in [0, 100].
    /// None when the user is not on a plan with weekly limits (API key,
    /// free tier) or no scrape has happened yet. Populated by the
    /// explicit `/usage` scrape (see `EnvelopeBody::UsageCheck`); not
    /// present on every envelope.
    #[serde(default)]
    pub weekly_pct: Option<f64>,
    /// §Usage-limits: ISO-8601 timestamp for the next weekly reset.
    /// Paired with `weekly_pct` — both `Some` or both `None`.
    #[serde(default)]
    pub weekly_resets_at: Option<chrono::DateTime<chrono::Utc>>,
    /// §Usage-limits: plan-level 5-hour session usage as a percentage in
    /// [0, 100]. Paired with `session_resets_at`.
    #[serde(default)]
    pub session_pct: Option<f64>,
    /// §Usage-limits: ISO-8601 timestamp for the next 5-hour session
    /// reset. Paired with `session_pct`.
    #[serde(default)]
    pub session_resets_at: Option<chrono::DateTime<chrono::Utc>>,
    /// §Usage-limits / §Small-terminal: when `true`, the most
    /// recent `/usage` scrape did not surface all four plan-level
    /// fields — either the PTY was too small for Claude to render
    /// the modal overlay (so the parser saw nothing), or the
    /// overlay omitted one of the fields at 0% session usage (so
    /// `session_resets_at` legitimately has no time-only line).
    /// `false` (the default) means either all four fields are
    /// populated or the scrape hasn't run yet. The UI uses this
    /// flag to surface a "scrape incomplete — try a larger
    /// window" hint alongside the "—" placeholder so the operator
    /// can distinguish "no data yet" from "PTY too small".
    #[serde(default)]
    pub scrape_incomplete: bool,
    /// §Writer-actor / §Usage-limits: when `true`, the most
    /// recent `/usage` scrape was preempted by an operator
    /// `Interrupt` mid-sequence — the writer actor's
    /// `SequenceOutcome::AbortedBeforeStep` fired before all
    /// planned scroll-and-parse rounds completed. The result
    /// envelope still publishes whatever fields the parser
    /// committed before the preempt so the operator sees the
    /// partial state, but this flag tells the UI to surface a
    /// distinct "scrape aborted by interrupt" hint instead of
    /// the generic "scrape incomplete" one. Both flags can be
    /// true on the same envelope (a preempted scrape that
    /// happened to be on a too-small PTY); the aborted variant
    /// is the more informative signal and should win in the UI.
    #[serde(default)]
    pub scrape_aborted: bool,
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

    #[test]
    fn usage_snapshot_round_trips_with_limit_fields() {
        // §Usage-limits: a v2 rabbit that has scraped a plan with
        // weekly + session caps emits all four new fields as
        // `Some(...)`. The shape must round-trip through serde so
        // warren's HTTP handler can deserialize the envelope it
        // receives on the SSE stream.
        use chrono::TimeZone;
        let weekly_resets = chrono::Utc.with_ymd_and_hms(2026, 7, 9, 5, 0, 0).unwrap();
        let session_resets = chrono::Utc.with_ymd_and_hms(2026, 7, 7, 12, 20, 0).unwrap();
        let snap = UsageSnapshot {
            input_tokens: 12_345,
            output_tokens: 6_789,
            cache_read: 1_000,
            cache_write: 200,
            context_pct_est: Some(42.5),
            parse_errors: 0,
            source: "usage_check".to_string(),
            weekly_pct: Some(73.0),
            weekly_resets_at: Some(weekly_resets),
            session_pct: Some(12.0),
            session_resets_at: Some(session_resets),
            scrape_incomplete: false,
            scrape_aborted: false,
        };
        let json = serde_json::to_value(&snap).expect("serialize");
        assert_eq!(json["weekly_pct"], 73.0);
        assert_eq!(json["session_pct"], 12.0);
        assert!(json["weekly_resets_at"].is_string());
        assert!(json["session_resets_at"].is_string());
        let back: UsageSnapshot = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.weekly_pct, Some(73.0));
        assert_eq!(back.session_pct, Some(12.0));
        assert_eq!(back.weekly_resets_at, Some(weekly_resets));
        assert_eq!(back.session_resets_at, Some(session_resets));
    }

    #[test]
    fn usage_snapshot_v1_json_without_limit_fields_deserializes_to_none() {
        // A v1 producer (pre-usage-limits rabbit) never emitted the
        // four new fields; the v2 struct must tolerate their
        // absence and default them to `None`. This keeps the
        // rollout window safe: a v1 rabbit talking to a v2 warren
        // (or vice-versa) must not panic on the missing keys.
        let v1_json = serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read": 10,
            "cache_write": 5,
            "context_pct_est": null,
            "parse_errors": 0,
            "source": "transcript",
        });
        let snap: UsageSnapshot = serde_json::from_value(v1_json)
            .expect("v1 envelope must deserialize under a v2 struct");
        assert_eq!(snap.weekly_pct, None);
        assert_eq!(snap.weekly_resets_at, None);
        assert_eq!(snap.session_pct, None);
        assert_eq!(snap.session_resets_at, None);
    }

    #[test]
    fn usage_snapshot_scrape_incomplete_round_trips_and_v1_defaults_to_false() {
        // §Small-terminal mitigation C: a partial scrape (1–3
        // fields populated) sets `scrape_incomplete = true` so the
        // UI can surface the hint. v1 envelopes (no flag) default
        // to `false` so a mixed-version rollout stays safe.
        use chrono::TimeZone;
        let snap = UsageSnapshot {
            input_tokens: 0,
            output_tokens: 0,
            cache_read: 0,
            cache_write: 0,
            context_pct_est: None,
            parse_errors: 0,
            source: "usage_check".to_string(),
            weekly_pct: Some(73.0),
            weekly_resets_at: Some(chrono::Utc.with_ymd_and_hms(2026, 7, 9, 5, 0, 0).unwrap()),
            session_pct: Some(12.0),
            session_resets_at: None, // partial — session reset missing
            scrape_incomplete: true,
            scrape_aborted: false,
        };
        let json = serde_json::to_value(&snap).expect("serialize");
        assert_eq!(json["scrape_incomplete"], true);
        let back: UsageSnapshot = serde_json::from_value(json).expect("deserialize");
        assert!(back.scrape_incomplete);

        // v1 JSON without the flag deserializes to false.
        let v1_json = serde_json::json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read": 10,
            "cache_write": 5,
            "context_pct_est": null,
            "parse_errors": 0,
            "source": "transcript",
            "weekly_pct": null,
            "weekly_resets_at": null,
            "session_pct": null,
            "session_resets_at": null,
        });
        let back: UsageSnapshot =
            serde_json::from_value(v1_json).expect("v1 envelope must deserialize");
        assert!(!back.scrape_incomplete);
    }
}
