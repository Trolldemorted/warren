//! §A.10 `integration` — milestone-1 dispatch discipline.
//!
//! Milestone-1's acceptance checklist (prompt/Stop roundtrip, `/clear`, ESC
//! interrupt, slash) has two layers:
//!
//!   * the *live* byte-level roundtrip against a real `claude` TUI — covered
//!     by the `#[ignore]`d `claude_smoke.rs` (hits the API, not run in CI); and
//!   * the *dispatch discipline* — how rabbit decides what to do with each
//!     inbound envelope given the agent's lifecycle state. That's the
//!     reject-when-Running policy (§D), and it IS deterministic and CI-safe.
//!
//! This file covers the second layer end-to-end: it drives the real
//! `ObserverHandle` state machine with the same hook events `claude` emits, and
//! checks the real `supervisor::should_reject_prompt` gate at each step.

use rabbit::observer::hooks::ObserverHandle;
use rabbit::observer::state::State;
use rabbit::supervisor::{prompt_rejected_for, should_reject_prompt};
use rabbit::wire::{Envelope, EnvelopeBody};

fn prompt() -> EnvelopeBody {
    EnvelopeBody::Prompt {
        id: uuid::Uuid::nil(),
        text: "do the thing".into(),
        by: "tester".into(),
    }
}
fn interrupt() -> EnvelopeBody {
    EnvelopeBody::Interrupt
}
fn slash() -> EnvelopeBody {
    EnvelopeBody::Slash {
        cmd: "usage".into(),
    }
}
fn clear() -> EnvelopeBody {
    EnvelopeBody::Clear { hard: false }
}

#[test]
fn milestone1_prompt_stop_roundtrip_gates_correctly() {
    let h = ObserverHandle::new();

    // Before any turn (Starting): a prompt is accepted.
    assert_eq!(h.latest_state(), State::Starting);
    assert!(
        !should_reject_prompt(h.latest_state(), &prompt()),
        "a prompt must be accepted when the agent is not Running"
    );

    // Turn starts (UserPromptSubmit → Running).
    h.ingest("UserPromptSubmit", &serde_json::json!({}));
    assert_eq!(h.latest_state(), State::Running);

    // A second prompt arriving mid-turn is rejected...
    assert!(
        should_reject_prompt(h.latest_state(), &prompt()),
        "a prompt arriving while Running must be rejected"
    );
    // ...but control frames are always allowed through, even mid-turn.
    assert!(
        !should_reject_prompt(h.latest_state(), &interrupt()),
        "ESC interrupt"
    );
    assert!(
        !should_reject_prompt(h.latest_state(), &slash()),
        "slash command"
    );
    assert!(!should_reject_prompt(h.latest_state(), &clear()), "/clear");

    // Turn ends (Stop → Idle): prompts are accepted again.
    h.ingest("Stop", &serde_json::json!({}));
    assert_eq!(h.latest_state(), State::Idle);
    assert!(
        !should_reject_prompt(h.latest_state(), &prompt()),
        "a prompt must be accepted again once the turn completes"
    );
}

#[test]
fn control_frames_never_rejected_in_any_state() {
    for state in [
        State::Starting,
        State::Idle,
        State::Running,
        State::Ended,
        State::Dead,
    ] {
        for body in [interrupt(), slash(), clear()] {
            assert!(
                !should_reject_prompt(state, &body),
                "control frame {body:?} must never be rejected (state {state:?})"
            );
        }
    }
}

#[test]
fn prompts_only_rejected_in_running_state() {
    for state in [State::Starting, State::Idle, State::Ended, State::Dead] {
        assert!(
            !should_reject_prompt(state, &prompt()),
            "prompt must be accepted in non-Running state {state:?}"
        );
    }
    assert!(should_reject_prompt(State::Running, &prompt()));
}

// §D reject-when-Running wire contract: the supervisor must emit a dedicated
// `PromptRejected` envelope (not a generic Log { warn }) so warren can render
// a targeted UI banner tied to the original prompt id. The gate is tested
// above; this test pins the *shape* of the wire payload.

#[test]
fn prompt_rejected_envelope_carries_original_prompt_id() {
    let id = uuid::Uuid::new_v4();
    let env = Envelope {
        v: 1,
        seq: 0,
        body: EnvelopeBody::Prompt {
            id,
            text: "do the thing".into(),
            by: "tester".into(),
        },
    };
    let rejected = prompt_rejected_for(&env);
    match rejected {
        EnvelopeBody::PromptRejected { id: rid, reason } => {
            assert_eq!(rid, id, "rejection envelope must echo the prompt id");
            assert!(
                !reason.is_empty(),
                "reason must be human-readable, not blank"
            );
        }
        other => panic!("expected PromptRejected, got {other:?}"),
    }
}

#[test]
fn prompt_rejected_envelope_is_not_a_log() {
    // Regression guard: previously rejections surfaced as
    // `Log { level: "warn", message: ... }`. The dedicated variant exists
    // specifically so warren can distinguish them; this test fails loudly
    // if anyone reverts the change.
    let id = uuid::Uuid::new_v4();
    let env = Envelope {
        v: 1,
        seq: 0,
        body: EnvelopeBody::Prompt {
            id,
            text: "x".into(),
            by: "t".into(),
        },
    };
    let rejected = prompt_rejected_for(&env);
    assert!(
        !matches!(rejected, EnvelopeBody::Log(_)),
        "prompt rejection must NOT emit a Log envelope (was the v1 stub)"
    );
    assert!(matches!(rejected, EnvelopeBody::PromptRejected { .. }));
}
