use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 1;
/// §D: primary terminal channel for the `/claude` endpoint. The byte
/// that prefixes every binary frame flowing through `ws_browser.rs`
/// and through rabbit's `LinkCmd::SendBinary { chan, data }`. `0x02`
/// (`TERM_CHAN_SHELL`) is the /shell equivalent.
pub const TERM_CHAN_CLAUDE: u8 = 0x01;
/// §D Milestone 5: secondary terminal channel for the `/shell` endpoint.
/// A `bash` PTY on the same rabbit, distinct binary-stream id so it can be
/// subscribed to (and written to) independently of the main Claude channel.
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
    Hello(HelloDown),
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
        id: Uuid,
        text: String,
        by: String,
    },
    /// §D reject-when-Running outcome: a `Prompt` arrived while the agent
    /// was already `Running`, so rabbit bounced it instead of injecting
    /// keystrokes into a live turn. Distinct from a generic
    /// `Log { level: "warn" }` so the warren UI can surface a dedicated
    /// affordance (e.g. an inline banner tied to the original prompt id).
    /// Mirrors `rabbit::wire::EnvelopeBody::PromptRejected`.
    PromptRejected {
        id: Uuid,
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
        prompt_id: Uuid,
        usage: Option<UsageSnapshot>,
        error: Option<String>,
    },
    /// §D Milestone 5 (Phase B): late-join screen dump pushed by rabbit in
    /// response to a `SnapshotRequest`. Mirrors `rabbit::wire::ScreenSnapshot`.
    ScreenSnapshot(ScreenSnapshotBody),
    /// §D Milestone 5 (Phase B): browser asks rabbit for a current snapshot
    /// of the named channel's VT after the bounded replay buffer has been
    /// pushed into xterm.js.
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloDown {
    pub agent_id: Uuid,
    pub protocol_v: u32,
    pub claude_version: String,
    pub session_id: Option<String>,
    pub state: AgentState,
    pub term_size: TermSize,
    /// §D Milestone 5: absolute base URL of the rabbit's recorder HTTP
    /// server (e.g. `http://10.0.0.42:7790`). `None` when the recorder is
    /// off; the history page routes must check rather than assume.
    #[serde(default)]
    pub recorder_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFrame {
    pub state: AgentState,
    pub session_id: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Starting,
    Idle,
    Running,
    Ended,
    Dead,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptEcho {
    pub prompt_id: Uuid,
    pub text: String,
    pub by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnDone {
    pub prompt_id: Uuid,
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
    /// Cumulative count of transcript JSONL lines that failed to parse in
    /// rabbit (§A.3). Surfaced for monitoring; never blocks the terminal.
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
    //! Round-trip serde tests for the wire envelope. Each new variant gets
    //! a positive (serialize → parse back → match) and a JSON-tag check
    //! (the `t` field uses snake_case so warren's serde derive resolves).

    use super::*;

    fn make_env(body: EnvelopeBody) -> Envelope {
        Envelope {
            v: PROTOCOL_VERSION,
            seq: 0,
            body,
        }
    }

    #[test]
    fn connection_assigned_round_trips_and_tag_is_snake_case() {
        let env = make_env(EnvelopeBody::ConnectionAssigned {
            connection_id: Uuid::nil(),
        });
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["t"], "connection_assigned");
        assert_eq!(json["connection_id"], "00000000-0000-0000-0000-000000000000");
        let parsed: Envelope = serde_json::from_value(json).unwrap();
        match parsed.body {
            EnvelopeBody::ConnectionAssigned { connection_id } => {
                assert_eq!(connection_id, Uuid::nil());
            }
            other => panic!("expected ConnectionAssigned, got {other:?}"),
        }
    }

    #[test]
    fn claim_leader_round_trips_and_tag_is_snake_case() {
        let env = make_env(EnvelopeBody::ClaimLeader {
            cols: 120,
            rows: 40,
        });
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["t"], "claim_leader");
        assert_eq!(json["cols"], 120);
        assert_eq!(json["rows"], 40);
        let parsed: Envelope = serde_json::from_value(json).unwrap();
        match parsed.body {
            EnvelopeBody::ClaimLeader { cols, rows } => {
                assert_eq!(cols, 120);
                assert_eq!(rows, 40);
            }
            other => panic!("expected ClaimLeader, got {other:?}"),
        }
    }

    #[test]
    fn release_leader_round_trips_and_tag_is_snake_case() {
        let env = make_env(EnvelopeBody::ReleaseLeader);
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["t"], "release_leader");
        let parsed: Envelope = serde_json::from_value(json).unwrap();
        assert!(matches!(parsed.body, EnvelopeBody::ReleaseLeader));
    }

    #[test]
    fn leader_changed_round_trips_with_some_and_none() {
        let env = make_env(EnvelopeBody::LeaderChanged {
            leader_id: Some(Uuid::nil()),
            cols: 80,
            rows: 24,
        });
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["t"], "leader_changed");
        assert_eq!(json["cols"], 80);
        assert_eq!(json["rows"], 24);
        let parsed: Envelope = serde_json::from_value(json).unwrap();
        match parsed.body {
            EnvelopeBody::LeaderChanged {
                leader_id,
                cols,
                rows,
            } => {
                assert_eq!(leader_id, Some(Uuid::nil()));
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            other => panic!("expected LeaderChanged, got {other:?}"),
        }

        // None variant: leader_id field serializes as JSON null and
        // round-trips back to None.
        let env_none = make_env(EnvelopeBody::LeaderChanged {
            leader_id: None,
            cols: 80,
            rows: 24,
        });
        let json_none = serde_json::to_value(&env_none).unwrap();
        assert!(json_none["leader_id"].is_null());
        let parsed_none: Envelope = serde_json::from_value(json_none).unwrap();
        assert!(matches!(
            parsed_none.body,
            EnvelopeBody::LeaderChanged {
                leader_id: None,
                ..
            }
        ));
    }
}
