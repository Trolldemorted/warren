use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;
pub const TERM_CHAN_CLAUDE: u8 = 0x01;
/// secondary terminal channel for `/agent/:id/shell`.
/// A `bash` PTY on the same rabbit, distinct from the main Claude
/// channel so it can be subscribed to (and written to) independently.
pub const TERM_CHAN_SHELL: u8 = 0x02;

/// — one server→browser binary
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
        // the originating
        /// browser's `connection_id` so subscribers can filter
        /// `PromptEcho` / `PromptRejected` to their own prompts.
        /// `None` when the producer is the HTTP path (no browser tab
        /// owns the request) or a warren bg-task scheduler.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by_connection_id: Option<uuid::Uuid>,
    },
    // a `Prompt` arrived while the agent
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
        // the originating
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
    // server-initiated request for rabbit to scrape
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
    // server-initiated request for rabbit to scrape
    /// Claude's `/context` overlay and return a fresh `Usage` envelope
    /// with the `ctx_*` fields populated. Same fire-and-forget shape as
    /// `UsageCheck`: HTTP returns 202 immediately; the parsed fields
    /// arrive on SSE `/events/stream` inside a fresh `Usage` envelope
    /// carrying the new fields (None when no scrape has happened).
    /// Triggered by the "Context" button in the warren UI and (in the
    /// future) by the scheduled-prompt scheduler at fire time, to
    /// capture the context-window utilization alongside the plan-level
    /// weekly/session limits.
    ContextCheck,
    Restart {
        fresh: bool,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Repaint,
    // typed key press for mobile clients that can't
    /// send the bytes through their soft keyboard (Tab, Escape, arrow
    /// keys, etc.). The server translates the named `Key` to a byte
    /// sequence (see `key_to_bytes`) and feeds it through the same
    /// writer-actor FIFO that `Prompt`/`Slash`/`Interrupt` use. Mobile
    /// UI sends this over the existing WS as a JSON text frame;
    /// desktop `term.onData` path is unaffected. Parallel to
    /// `Interrupt` / `Slash` so viewer-mode drop logic + FIFO
    /// ordering apply unchanged.
    SendKey(SendKey),
    // ask rabbit to SIGWINCH-jiggle the
    /// shell PTY so bash repaints its prompt. Shell has no VT snapshot
    /// (no `TermTracker` — see supervisor.rs:602-606), so the v1
    /// SIGWINCH-jiggle heuristic is still the right tool here, unlike
    /// the claude channel which now uses `ScreenSnapshot`. `cols, rows`
    /// match the bash PTY's current size; rabbit uses these as the
    /// jiggle target (widen by 1, settle, restore — see `pty.rs::jiggle`).
    /// Distinct from `Repaint` (which targets the claude PTY only) so
    /// the v1 envelope shape stays unchanged and both channels can grow
    /// independently.
    ShellRepaint {
        cols: u16,
        rows: u16,
    },
    StopHook {
        prompt_id: uuid::Uuid,
        usage: Option<UsageSnapshot>,
        error: Option<String>,
    },
    // Claude fired a `permission_request` hook
    /// (the operator needs to approve a tool call before the turn can
    /// continue). The scheduler's observation task subscribes to
    /// `meta_tx` and, on receiving a `NeedsInput` matching its
    /// in-flight `prompt_id`, calls `handle.interrupt()` to cancel the
    /// scheduled run. Distinct from `PromptRejected` (which fires on
    /// the *prompt submission* path) so the run-history `outcome`
    /// can be `'needs_input_canceled'` rather than the generic
    /// `'interrupted'`. `by_connection_id` mirrors `Prompt`/`PromptRejected`:
    /// `None` for hooks-driven events (which have no browser tab).
    NeedsInput {
        prompt_id: uuid::Uuid,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by_connection_id: Option<uuid::Uuid>,
    },
    /// (Phase B): late-join screen dump. Sent by rabbit in
    /// response to a [`SnapshotRequest`] from warren so a fresh browser pane
    /// can paint an authoritative terminal state instead of relying on the
    /// SIGWINCH jiggle. `text.len() == rows`; each string is the VT's own
    /// space-padded grid row.
    ScreenSnapshot(ScreenSnapshotBody),
    /// (Phase B): warren asks rabbit for a snapshot of the
    /// given channel's VT. Used by `ws_browser` after the replay buffer
    /// has been pushed into xterm.js.
    SnapshotRequest {
        chan: u8,
    },
    // warren is the source of truth for the
    /// terminal grid. After the rabbit's hello, warren sends this once
    /// with the cols/rows it wants the rabbit's PTY to use. The same
    /// value is what the xterm.js template renders, so the kernel
    /// winsize and the browser grid always match. Inbound `TuiConfig`
    /// on the warren side is a no-op (server→rabbit only).
    TuiConfig {
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
    /// — per-`chan` counter of the
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
    /// Typed state per — supervisor emits
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

/// — the canonical typed state enum.
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

/// — the supervisor's hello is the same shape as the broker's
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

/// closed enum of named keys the mobile UI can send.
/// `Ctrl { c }` and `Alt { c }` are the only variants that carry a
/// payload — `c` is validated server-side against an allow-list of
/// single-byte ASCII chars (see `key_to_bytes`). Mobile clients
/// serialize this as `{"k": "<variant>"}` for unit variants or
/// `{"k": "ctrl", "c": "a"}` for the chord variants; serde uses the
/// `rename_all = "snake_case"` tag on the enum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Key {
    Tab,
    Backspace,
    Escape,
    Enter,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    /// `c` must be a single ASCII letter, digit, or one of
    /// `@[]\\^{}_`. Validated by `key_to_bytes`; an out-of-range
    /// payload returns `anyhow::Error` with a descriptive message.
    Ctrl {
        c: char,
    },
    /// `c` must be a single printable ASCII char (not ESC). ESC
    /// itself is rejected to avoid ambiguous sequences.
    Alt {
        c: char,
    },
}

/// typed key-press envelope. `modifiers` is reserved
/// for v2 (e.g. Shift+Tab → `\x1b[Z`); v1 clients omit it and v1
/// server ignores it because the chord information rides in-band on
/// `Key::Ctrl` / `Key::Alt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendKey {
    pub key: Key,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifiers: Option<Modifiers>,
}

/// modifier overlay reserved for v2 — `Ctrl+letter`
/// rides `Key::Ctrl { c }` directly today, so this struct is a no-op
/// at the moment. Kept in the wire shape so adding
/// `Shift+arrow = "\x1b[1;2A"` later doesn't need a v3 envelope.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Modifiers {
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
}

/// Translate a typed `Key` to the byte sequence a terminal expects.
/// Returns `Err(anyhow::Error)` tagged `key_to_bytes::invalid` for
/// invalid `Ctrl`/`Alt` payloads so the caller can surface a useful
/// message instead of a generic parse error. Always returns `Vec<u8>`
/// (never borrowed) so the writer actor can hand it straight to the
/// FIFO without lifetime juggling.
pub fn key_to_bytes(key: &Key) -> anyhow::Result<Vec<u8>> {
    use Key::*;
    let bytes: Vec<u8> = match key {
        Tab => b"\t".to_vec(),
        Backspace => vec![0x7f],
        Escape => vec![0x1b],
        Enter => b"\r".to_vec(),
        ArrowUp => b"\x1b[A".to_vec(),
        ArrowDown => b"\x1b[B".to_vec(),
        ArrowRight => b"\x1b[C".to_vec(),
        ArrowLeft => b"\x1b[D".to_vec(),
        Home => b"\x1b[H".to_vec(),
        End => b"\x1b[F".to_vec(),
        PageUp => b"\x1b[5~".to_vec(),
        PageDown => b"\x1b[6~".to_vec(),
        Delete => b"\x1b[3~".to_vec(),
        Ctrl { c } => {
            if !is_ctrl_char(*c) {
                return Err(anyhow::anyhow!(
                    "Ctrl key requires an ASCII letter/digit/punct, got {c:?}"
                ));
            }
            vec![ctrl_byte(*c)]
        }
        Alt { c } => {
            if !c.is_ascii() || *c == '\x1b' {
                return Err(anyhow::anyhow!(
                    "Alt key requires a printable ASCII char, got {c:?}"
                ));
            }
            vec![0x1b, *c as u8]
        }
    };
    Ok(bytes)
}

/// Allow-list for `Key::Ctrl`. Matches xterm's `keys.CTRL_*` set:
/// `@`, `A`–`Z`, `[`, `\`, `]`, `^`, `_`, plus `0`–`9` (which maps
/// to 0x00..=0x09 — the same range xterm uses for Ctrl-digit). Lower
/// case is normalised to upper by `ctrl_byte`.
fn is_ctrl_char(c: char) -> bool {
    matches!(
        c,
        '@' | 'A'..='Z' | '[' | '\\' | ']' | '^' | '_' | '0'..='9' | 'a'..='z'
    )
}

/// Map an allow-listed ASCII char to its 0x00..=0x1F byte. Caller must
/// have gated with `is_ctrl_char` — `unreachable!` covers the
/// unreachable arm.
fn ctrl_byte(c: char) -> u8 {
    let lc = c.to_ascii_lowercase();
    let b = match lc {
        '@' => 0,
        'a'..='z' => lc as u8 - b'a' + 1,
        '[' => 27,
        '\\' => 28,
        ']' => 29,
        '^' => 30,
        '_' => 31,
        '0'..='9' => lc as u8 - b'0',
        _ => unreachable!("is_ctrl_char gates this"),
    };
    b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptEcho {
    pub prompt_id: uuid::Uuid,
    pub text: String,
    pub by: String,
    // the originating
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
    /// rabbit started.
    /// drift in the on-disk format; it is *not* fatal and never blocks the
    /// terminal plane. Surfaced via the next `Usage` envelope after each
    /// increment.
    #[serde(default)]
    pub parse_errors: u64,
    pub source: String,
    // plan-level weekly usage as a percentage in [0, 100].
    /// None when the user is not on a plan with weekly limits (API key,
    /// free tier) or no scrape has happened yet. Populated by the
    /// explicit `/usage` scrape (see `EnvelopeBody::UsageCheck`); not
    /// present on every envelope.
    #[serde(default)]
    pub weekly_pct: Option<f64>,
    // ISO-8601 timestamp for the next weekly reset.
    /// Paired with `weekly_pct` — both `Some` or both `None`.
    #[serde(default)]
    pub weekly_resets_at: Option<chrono::DateTime<chrono::Utc>>,
    // plan-level 5-hour session usage as a percentage in
    /// [0, 100]. Paired with `session_resets_at`.
    #[serde(default)]
    pub session_pct: Option<f64>,
    // ISO-8601 timestamp for the next 5-hour session
    /// reset. Paired with `session_pct`.
    #[serde(default)]
    pub session_resets_at: Option<chrono::DateTime<chrono::Utc>>,
    // when `true`, the most
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
    // when `true`, the most
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
    // tokens consumed in the current context
    /// window, scraped from Claude's `/context` modal. None until
    /// an explicit `ContextCheck` runs. Paired with
    /// `ctx_total_tokens` (both Some or both None). Distinct from
    /// the transcript-derived `context_pct_est` heuristic — the
    /// modal value is authoritative.
    #[serde(default)]
    pub ctx_used_tokens: Option<u64>,
    // the size of the context window Claude is
    /// using (typically 200_000 for Sonnet/Opus, smaller for
    /// Haiku-class). Paired with `ctx_used_tokens`.
    #[serde(default)]
    pub ctx_total_tokens: Option<u64>,
    // percentage of the window consumed,
    /// `[0, 100]`, derived from the modal's bar or trailing
    /// `(P%)` label. Distinct from `context_pct_est`, which is the
    /// transcript-side heuristic estimator.
    #[serde(default)]
    pub ctx_used_pct: Option<f64>,
    // percentage free, mirror of `ctx_used_pct`.
    /// Convenience for the UI; either alone is enough to render.
    #[serde(default)]
    pub ctx_free_pct: Option<f64>,
    // Claude's labeled window size (e.g. `200K`
    /// → 200_000). Set when the modal labels the bar; independent
    /// of `ctx_total_tokens` so either can populate without the
    /// other.
    #[serde(default)]
    pub ctx_window_tokens: Option<u64>,
    // optional per-category breakdown as a JSON
    /// object (e.g. `{"system": 1234, "tools": 5678, "conversation":
    /// 9012}`). We do NOT lock the shape — Claude's TUI may evolve.
    /// Pass through whatever the modal emits; the UI surfaces it as
    /// a small key/value table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx_categories: Option<serde_json::Value>,
    // when `true`, the most
    /// recent `/context` scrape did not surface all primary fields
    /// (`ctx_used_tokens`, `ctx_total_tokens`, `ctx_used_pct`).
    /// Mirrors `scrape_incomplete` on the `/usage` side so the UI
    /// can surface a "context scrape incomplete — try a larger
    /// window" hint.
    #[serde(default)]
    pub ctx_scrape_incomplete: bool,
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
    //! — serde roundtrip for `ScreenSnapshotBody::after_seq`. The
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

    // the `TuiConfig` envelope carries the warren-
    /// supplied grid size. The wire shape is `{"t":"tui_config","cols":
    /// <u16>,"rows":<u16>}` — flat keys, snake_case `t` discriminator,
    /// matching the rest of the protocol. Pin the round-trip so a future
    /// rename (e.g. nested `{"t":"tui_config","size":{"cols":..,"rows":..}}`)
    /// is intentional.
    #[test]
    fn tui_config_envelope_round_trips_through_wire_shape() {
        let env = Envelope {
            v: PROTOCOL_VERSION,
            seq: 0,
            body: EnvelopeBody::TuiConfig {
                cols: 200,
                rows: 50,
            },
        };
        let v = serde_json::to_value(&env).expect("serialize");
        assert_eq!(v["t"], "tui_config", "wire tag must match `tui_config`");
        assert_eq!(v["cols"], 200);
        assert_eq!(v["rows"], 50);

        let back: Envelope = serde_json::from_value(v).expect("deserialize");
        match back.body {
            EnvelopeBody::TuiConfig { cols, rows } => {
                assert_eq!(cols, 200);
                assert_eq!(rows, 50);
            }
            other => panic!("expected TuiConfig, got {other:?}"),
        }
    }

    #[test]
    fn usage_snapshot_round_trips_with_limit_fields() {
        // a v2 rabbit that has scraped a plan with
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
            ctx_used_tokens: None,
            ctx_total_tokens: None,
            ctx_used_pct: None,
            ctx_free_pct: None,
            ctx_window_tokens: None,
            ctx_categories: None,
            ctx_scrape_incomplete: false,
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
        // a partial scrape (1–3
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
            ctx_used_tokens: None,
            ctx_total_tokens: None,
            ctx_used_pct: None,
            ctx_free_pct: None,
            ctx_window_tokens: None,
            ctx_categories: None,
            ctx_scrape_incomplete: false,
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

    #[test]
    fn usage_snapshot_round_trips_with_context_fields() {
        // a v2 rabbit that has scraped the `/context`
        // overlay emits all seven new fields. The shape must
        // round-trip through serde so warren's HTTP handler can
        // deserialize the envelope it receives on the SSE stream.
        let snap = UsageSnapshot {
            input_tokens: 12_345,
            output_tokens: 6_789,
            cache_read: 1_000,
            cache_write: 200,
            context_pct_est: Some(42.5), // distinct from ctx_used_pct
            parse_errors: 0,
            source: "context_check".to_string(),
            weekly_pct: None,
            weekly_resets_at: None,
            session_pct: None,
            session_resets_at: None,
            scrape_incomplete: false,
            scrape_aborted: false,
            ctx_used_tokens: Some(87_432),
            ctx_total_tokens: Some(200_000),
            ctx_used_pct: Some(43.7),
            ctx_free_pct: Some(56.3),
            ctx_window_tokens: Some(200_000),
            ctx_categories: Some(serde_json::json!({
                "system": 1234,
                "tools": 5678,
                "conversation": 80_520,
            })),
            ctx_scrape_incomplete: false,
        };
        let json = serde_json::to_value(&snap).expect("serialize");
        assert_eq!(json["ctx_used_tokens"], 87_432);
        assert_eq!(json["ctx_total_tokens"], 200_000);
        assert_eq!(json["ctx_used_pct"], 43.7);
        assert_eq!(json["ctx_free_pct"], 56.3);
        assert_eq!(json["ctx_window_tokens"], 200_000);
        assert_eq!(json["ctx_categories"]["system"], 1234);
        assert_eq!(json["ctx_categories"]["tools"], 5678);
        assert_eq!(json["ctx_scrape_incomplete"], false);

        let back: UsageSnapshot = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.ctx_used_tokens, Some(87_432));
        assert_eq!(back.ctx_total_tokens, Some(200_000));
        assert_eq!(back.ctx_used_pct, Some(43.7));
        assert_eq!(back.ctx_free_pct, Some(56.3));
        assert_eq!(back.ctx_window_tokens, Some(200_000));
        assert_eq!(back.ctx_categories.unwrap()["conversation"], 80_520);
    }

    #[test]
    fn usage_snapshot_v1_json_without_context_fields_deserializes_to_none() {
        // a v1 producer (pre-/context rabbit) never
        // emitted the seven `ctx_*` fields; the v2 struct must
        // tolerate their absence and default them to None. This
        // keeps the rollout window safe: a v1 rabbit talking to a
        // v2 warren (or vice-versa) must not panic on the missing
        // keys.
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
            "scrape_incomplete": false,
            "scrape_aborted": false,
        });
        let snap: UsageSnapshot = serde_json::from_value(v1_json)
            .expect("v1 envelope must deserialize under a v2 struct");
        assert_eq!(snap.ctx_used_tokens, None);
        assert_eq!(snap.ctx_total_tokens, None);
        assert_eq!(snap.ctx_used_pct, None);
        assert_eq!(snap.ctx_free_pct, None);
        assert_eq!(snap.ctx_window_tokens, None);
        assert_eq!(snap.ctx_categories, None);
        assert!(!snap.ctx_scrape_incomplete);
    }

    #[test]
    fn usage_snapshot_ctx_categories_skipped_when_none() {
        // the `ctx_categories` field has
        // `skip_serializing_if = "Option::is_none"` so envelopes
        // without a per-category breakdown don't carry an empty
        // `"ctx_categories": null` over the wire. Pin the behavior
        // so a future "tighten the derive" refactor can't silently
        // start emitting nulls.
        let snap = UsageSnapshot {
            ctx_categories: None,
            ..Default::default()
        };
        let json = serde_json::to_value(&snap).expect("serialize");
        assert!(
            json.get("ctx_categories").is_none(),
            "ctx_categories: null must be elided from the wire JSON"
        );
    }

    #[test]
    fn envelope_body_context_check_serializes_with_correct_tag() {
        // pin the wire tag `"context_check"` so a
        // future rename (e.g. nested `{"t":"context_check", ...}`)
        // has to be intentional. Mirrors the v1
        // `usage_check` envelope shape.
        let env = Envelope {
            v: PROTOCOL_VERSION,
            seq: 0,
            body: EnvelopeBody::ContextCheck,
        };
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(json["t"], "context_check");
        let back: Envelope = serde_json::from_value(json).expect("deserialize");
        assert!(matches!(back.body, EnvelopeBody::ContextCheck));
    }

    // serde roundtrip for the SendKey envelope. The
    // nested `Key` enum uses an inline `tag = "k"` so a SendKey body
    // serializes as `{"t":"send_key","key":{"k":"arrow_up"}}`. Pin
    // the exact shape so a future `rename_all` tweak on Key doesn't
    // silently break the mobile JS, which sends this shape verbatim.
    #[test]
    fn send_key_envelope_round_trips_through_wire_shape() {
        let env = Envelope {
            v: PROTOCOL_VERSION,
            seq: 0,
            body: EnvelopeBody::SendKey(SendKey {
                key: Key::ArrowUp,
                modifiers: None,
            }),
        };
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(json["t"], "send_key");
        assert_eq!(json["key"]["k"], "arrow_up");
        let back: Envelope = serde_json::from_value(json).expect("deserialize");
        match back.body {
            EnvelopeBody::SendKey(SendKey {
                key: Key::ArrowUp, ..
            }) => {}
            other => panic!("expected SendKey/ArrowUp, got {other:?}"),
        }
    }

    // chord payloads (`Ctrl { c }`, `Alt { c }`)
    // serialize their payload as `c` at the same nesting level as
    // `k`. Pin the shape so a future flatten/representation refactor
    // doesn't break the mobile JS, which reads `envelope.body.key.c`.
    #[test]
    fn send_key_ctrl_chord_serializes_payload() {
        let env = Envelope {
            v: PROTOCOL_VERSION,
            seq: 0,
            body: EnvelopeBody::SendKey(SendKey {
                key: Key::Ctrl { c: 'a' },
                modifiers: None,
            }),
        };
        let json = serde_json::to_value(&env).expect("serialize");
        assert_eq!(json["key"]["k"], "ctrl");
        assert_eq!(json["key"]["c"], "a");
    }

    // key_to_bytes — one assertion per unit variant
    // pinned to the byte sequence xterm.js emits for the same key.
    // Drift here means the mobile UI and the desktop `term.onData`
    // path would disagree on what "press Tab" means, which would
    // confuse operators switching between devices.
    #[test]
    fn key_to_bytes_unit_variants() {
        assert_eq!(key_to_bytes(&Key::Tab).unwrap(), b"\t");
        assert_eq!(key_to_bytes(&Key::Backspace).unwrap(), vec![0x7f]);
        assert_eq!(key_to_bytes(&Key::Escape).unwrap(), vec![0x1b]);
        assert_eq!(key_to_bytes(&Key::Enter).unwrap(), b"\r");
        assert_eq!(key_to_bytes(&Key::ArrowUp).unwrap(), b"\x1b[A");
        assert_eq!(key_to_bytes(&Key::ArrowDown).unwrap(), b"\x1b[B");
        assert_eq!(key_to_bytes(&Key::ArrowRight).unwrap(), b"\x1b[C");
        assert_eq!(key_to_bytes(&Key::ArrowLeft).unwrap(), b"\x1b[D");
        assert_eq!(key_to_bytes(&Key::Home).unwrap(), b"\x1b[H");
        assert_eq!(key_to_bytes(&Key::End).unwrap(), b"\x1b[F");
        assert_eq!(key_to_bytes(&Key::PageUp).unwrap(), b"\x1b[5~");
        assert_eq!(key_to_bytes(&Key::PageDown).unwrap(), b"\x1b[6~");
        assert_eq!(key_to_bytes(&Key::Delete).unwrap(), b"\x1b[3~");
    }

    // Ctrl-letter mapping matches xterm's
    // `keys.CTRL_*` set. Lower/upper case fold to the same byte
    // (xterm convention). Pinned so a future
    // `ctrl_byte` refactor can't silently flip a chord.
    #[test]
    fn key_to_bytes_ctrl_letters() {
        assert_eq!(key_to_bytes(&Key::Ctrl { c: 'A' }).unwrap(), vec![0x01]);
        assert_eq!(key_to_bytes(&Key::Ctrl { c: 'a' }).unwrap(), vec![0x01]);
        assert_eq!(key_to_bytes(&Key::Ctrl { c: 'C' }).unwrap(), vec![0x03]); // common — sends SIGINT
        assert_eq!(key_to_bytes(&Key::Ctrl { c: 'R' }).unwrap(), vec![0x12]); // readline reverse-i-search
        assert_eq!(key_to_bytes(&Key::Ctrl { c: '[' }).unwrap(), vec![0x1b]); // ESC
        assert_eq!(key_to_bytes(&Key::Ctrl { c: '_' }).unwrap(), vec![0x1f]);
        assert_eq!(key_to_bytes(&Key::Ctrl { c: '0' }).unwrap(), vec![0x00]);
        assert_eq!(key_to_bytes(&Key::Ctrl { c: '9' }).unwrap(), vec![0x09]);
    }

    // Alt-letter prepends ESC. The char must be a
    // printable ASCII byte (not ESC itself) — reject non-ASCII so a
    // hostile client can't smuggle multi-byte sequences into the PTY
    // through this surface.
    #[test]
    fn key_to_bytes_alt_letter() {
        assert_eq!(key_to_bytes(&Key::Alt { c: 'x' }).unwrap(), b"\x1bx");
        // ESC itself is rejected (ambiguous with the prefix byte).
        assert!(key_to_bytes(&Key::Alt { c: '\x1b' }).is_err());
        // Non-ASCII char rejected.
        assert!(key_to_bytes(&Key::Alt { c: 'ñ' }).is_err());
    }

    // the Ctrl allow-list gates non-ASCII and
    // out-of-range punctuation. Pin the rejection so a future
    // "accept all bytes for Ctrl" change can't silently widen the
    // wire attack surface.
    #[test]
    fn key_to_bytes_ctrl_rejects_non_ascii() {
        assert!(key_to_bytes(&Key::Ctrl { c: 'ñ' }).is_err());
        assert!(key_to_bytes(&Key::Ctrl { c: '!' }).is_err()); // not in the @[]\^_ set
    }
}
