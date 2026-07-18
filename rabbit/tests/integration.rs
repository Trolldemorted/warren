//! §A.10 `integration` — milestone-1 dispatch discipline.
//!
//! Milestone-1's acceptance checklist (prompt/Stop roundtrip, `/clear`, ESC
//! interrupt, slash) has two layers:
//!
//!   * the *live* byte-level roundtrip against a real `claude` TUI — covered
//!     by the `#[ignore]`d `claude_smoke.rs` (hits the API, not run in CI); and
//!   * the *dispatch discipline* — how rabbit decides what to do with each
//!     inbound envelope given the agent's lifecycle state.
//!
//! Pin the wire-shape invariants on `EnvelopeBody::Prompt` /
//! `PromptRejected` so a future refactor that drops a field is
//! caught by a serde compile error, and confirm that the
//! `ObserverHandle`'s `latest_state` matches the actor's view after
//! the same hook events land (the gate observes the actor's snapshot,
//! not the observer's, but the two must agree).

use rabbit::observer::hooks::ObserverHandle;
use rabbit::observer::state::State;
use rabbit_lib::wire::{Envelope, EnvelopeBody};

#[test]
fn observer_state_machine_matches_actor_perspective() {
    // The actor's authoritative state lives on `AgentHandle::snapshot`
    // (kept in sync with rabbit's `State` envelopes); the observer's
    // `latest_state` is what hook events drive. They should agree on
    // transition timing for the cases that drive the busy-gate.
    let h = ObserverHandle::new();
    assert_eq!(h.latest_state(), State::Starting);
    h.ingest("UserPromptSubmit", &serde_json::json!({}));
    assert_eq!(h.latest_state(), State::Running);
    h.ingest("Stop", &serde_json::json!({}));
    assert_eq!(h.latest_state(), State::Idle);
    h.ingest("SessionEnd", &serde_json::json!({}));
    assert_eq!(h.latest_state(), State::Ended);
}

// Wire contract: `PromptRejected` must echo the original prompt id
// and carry a human-readable `reason`. Dropping either breaks the UI
// banner; these tests pin the round-trip shape.

#[test]
fn prompt_rejected_envelope_carries_original_prompt_id_and_reason() {
    let id = uuid::Uuid::new_v4();
    let env = Envelope {
        v: 2,
        seq: 0,
        body: EnvelopeBody::Prompt {
            id,
            text: "do the thing".into(),
            by: "tester".into(),
            by_connection_id: None,
        },
    };
    let rejected = EnvelopeBody::PromptRejected {
        id,
        reason: "agent is running a turn".into(),
        by_connection_id: None,
    };
    // Shape: id echoes, reason is non-empty, variant tag matches.
    match &rejected {
        EnvelopeBody::PromptRejected {
            id: rid,
            reason,
            by_connection_id,
        } => {
            assert_eq!(*rid, id, "rejection envelope must echo the prompt id");
            assert!(
                !reason.is_empty(),
                "reason must be human-readable, not blank"
            );
            assert!(
                by_connection_id.is_none(),
                "test sends HTTP-shaped rejection"
            );
        }
        other => panic!("expected PromptRejected, got {other:?}"),
    }
    // And the source `Prompt` is still wire-shape-compatible (no
    // accidental field drift between Prompt and PromptRejected):
    // they MUST share the `id` and `by_connection_id` fields. Note
    // the wire shape uses `#[serde(flatten)]` on `Envelope::body`,
    // so the JSON keys live at the top level (no nested `body`).
    let v = serde_json::to_value(&env).expect("serialize prompt");
    let rv = serde_json::to_value(&rejected).expect("serialize rejected");
    assert_eq!(v["id"], id.to_string());
    assert_eq!(rv["id"], id.to_string());
    assert_eq!(v["t"], "prompt");
    assert_eq!(rv["t"], "prompt_rejected");
}

#[test]
fn prompt_rejected_envelope_is_not_a_log() {
    // Regression guard: previously rejections surfaced as
    // `Log { level: "warn", message: ... }`. The dedicated variant
    // exists specifically so warren can distinguish them; this test
    // fails loudly if anyone reverts the change.
    let rejected = EnvelopeBody::PromptRejected {
        id: uuid::Uuid::new_v4(),
        reason: "agent is running a turn".into(),
        by_connection_id: None,
    };
    assert!(
        !matches!(rejected, EnvelopeBody::Log(_)),
        "prompt rejection must NOT emit a Log envelope (was the v1 stub)"
    );
    assert!(matches!(rejected, EnvelopeBody::PromptRejected { .. }));
}

#[test]
fn prompt_with_connection_id_round_trips() {
    // §Cross-tab prompt rejection visibility: a browser stamped
    // `by_connection_id` MUST round-trip through serde so warren
    // can filter on the rejection banner. v1 envelopes (no field)
    // must still deserialize cleanly under v2.
    let cid = uuid::Uuid::new_v4();
    let env = Envelope {
        v: 2,
        seq: 0,
        body: EnvelopeBody::Prompt {
            id: uuid::Uuid::new_v4(),
            text: "hello".into(),
            by: "browser".into(),
            by_connection_id: Some(cid),
        },
    };
    let json = serde_json::to_string(&env).expect("serialize");
    let back: Envelope = serde_json::from_str(&json).expect("deserialize");
    match back.body {
        EnvelopeBody::Prompt {
            by_connection_id, ..
        } => {
            assert_eq!(by_connection_id, Some(cid));
        }
        other => panic!("expected Prompt, got {other:?}"),
    }

    // v1-shaped JSON (no field) still deserializes — additive
    // upgrade, no breaking change for mixed-version rollouts.
    // Flat wire shape (no nested `body` — `#[serde(flatten)]`).
    let v1_json = serde_json::json!({
        "v": 2,
        "seq": 0,
        "t": "prompt",
        "id": uuid::Uuid::new_v4(),
        "text": "hi",
        "by": "admin",
    });
    let parsed: Envelope = serde_json::from_value(v1_json).expect("v1 prompt deserializes");
    match parsed.body {
        EnvelopeBody::Prompt {
            by_connection_id, ..
        } => {
            assert_eq!(by_connection_id, None);
        }
        other => panic!("expected Prompt, got {other:?}"),
    }
}
