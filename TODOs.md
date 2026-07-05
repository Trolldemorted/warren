# Remaining TODOs — rabbit + warren

Captured 2026-07-04, after the §1 stable-decisions audit and the real-`claude`
smoke test in `rabbit/src/claude_smoke.rs`. Source-of-truth for priorities
is `/input/rabbit.md` (sections §A.1–§A.11, §D milestones 1–5).

---

## Done this session (rejection UX + smoke test checklist)

- [x] **Leader-based resize (§A.6 follow-up)** — manual-claim model:
      every tab has a "Claim control" button; whoever clicks becomes
      leader, even if a prior leader is still connected (transfer,
      not rejection). New leader's `(cols, rows)` becomes the PTY
      grid via a `Resize` envelope to rabbit (no-op when equal); all
      browsers receive a `LeaderChanged` broadcast and any followers
      snap their xterm grid to the leader's. No auto-promotion on
      disconnect — a new leader must explicitly claim. State lives on
      the warren actor (`AgentHandle.leader`); rabbit is unchanged.
      See the §D Milestone 5 entry below for the full file list, the
      27 new tests, and the prior-term_size-read-order bug fix in the
      dispatch arm.
- [x] **Rejection UX stub → dedicated `PromptRejected` wire variant**
      (was the `warn` Log line in `supervisor.rs`). Added
      `EnvelopeBody::PromptRejected { id, reason }` to both `rabbit/src/wire.rs`
      and `warren/src/agents_live/wire.rs` (the two parallel enums must
      stay in sync). New `supervisor::prompt_rejected_for(env)` helper
      carries the original prompt id and human-readable reason. Supervisor
      loop (`supervisor.rs`) emits it via `LinkCmd::SendMeta` instead of a
      generic `Log { warn }`. `warren/src/agents_live/actor.rs::envelope_kind`
      extended to include `"prompt_rejected"`. UI: dedicated inline banner in
      `warren/templates/agent_claude.html` (`#prompt-rejected-banner`),
      driven by the JS `case 'prompt_rejected':` arm of `applyMeta`; banner
      auto-clears on the next non-`running` state frame. 2 new integration
      tests pin the wire contract (one positive, one regression guard
      against re-introducing the Log stub).
- [x] **`claude_smoke.rs` §A.10 milestone-1 checklist** (was the third
      "Smaller items noticed but not addressed"). Three new `#[ignore]`d
      tests added alongside the existing `claude_pt_roundtrip`, sharing a
      new `SmokeHarness` helper that wraps spawn + boot-drain + trust
      accept:
        * `clear_resets_terminal_and_rearms_prompt` — sends a prompt,
          asserts response, sends `/clear`, asserts a follow-up prompt
          still reaches the model.
        * `esc_interrupt_does_not_crash_session` — sends a longer prompt,
          gives claude a head-start, sends ESC, asserts the agent is
          still alive and accepts a follow-up.
        * `slash_usage_returns_context_readout` — sends `/usage`, asserts
          some usage/context/token signal in the output (loose match so
          future UI label changes don't break the test).
- [x] **Per-agent read-only viewer mode** (was Milestone 5 / §A.10).
      Activated by `?viewer=true` on `/agent/:id/claude` (and on the WS
      URL). Server-side enforcement in
      `warren/src/agents_live/ws_browser.rs`:
        * new `WsBrowserQuery { viewer: Option<bool> }` axum extractor;
        * new `should_drop_for_viewer(&EnvelopeBody) -> bool` helper
          extracted for testability — matches the full input-side
          surface (Prompt / Interrupt / Slash / Clear / Resize / Repaint /
          Restart);
        * `forward_browser_message` and the binary handler short-circuit
          on viewer mode;
        * the page's `term.onData` handler also skips sending typed bytes
          in viewer mode (saves a round trip + means a misconfigured proxy
          can't log viewer keystrokes).
      UI: `body.viewer-mode` CSS class hides the right-hand aside
      (Interrupt/Clear/Compact/Restart) and the `prompt-rejected-banner`
      styling is untouched; new `#viewer-mode-banner` ("read-only viewer
      mode — input disabled") pinned to the bottom of the term column.
      4 new unit tests in `ws_browser::tests` pin the drop-list:
        * `viewer_drops_prompt_frame`
        * `viewer_drops_all_control_input_frames`
        * `viewer_does_not_drop_rejection_outcomes` (output frames must
          always pass — guards against future shape changes silencing
          the new `PromptRejected` envelope for viewers)
        * `viewer_does_not_drop_meta_state_or_usage`
      POST endpoints (`/api/agents/:id/claude/*`) remain admin-gated;
      viewer-mode is per-WS and doesn't bypass that auth layer (see §D
      open question on auth granularity for the next step).
- Verified: `cargo check`/`clippy --all-targets`/`test -p rabbit` and
      `cargo test -p warren --bin warren` all clean (61 unit + 4 smoke
      ignored on rabbit; 4 new viewer-mode tests on warren).

---

## DONE — `/agent/:id/shell` (warren + rabbit both shipped)

The shell endpoint ships in two halves. A prior session landed the
**warren side**; this session landed the **rabbit side** (spawning +
managing the bash PTY) plus a warren fix the earlier pass missed.

**Warren side — DONE (prior session):**

- [x] `TERM_CHAN_SHELL: u8 = 0x02` added to both `rabbit/src/wire.rs` and
      `warren/src/agents_live/wire.rs` (mirrors `TERM_CHAN_CLAUDE`).
- [x] Multi-channel aware actor: `warren/src/agents_live/actor.rs`
      no longer strips the channel byte on inbound binary frames —
      passes it through so subscribers can filter. The actor used to
      silently collapse everything onto a single terminal stream,
      which would have broken `/shell` even after the rest of the
      plumbing landed.
- [x] `warren/src/agents_live/ws_browser.rs` filters inbound term frames
      by `TERM_CHAN_CLAUDE` (other channels dropped on the floor for
      this WS).
- [x] New `warren/src/agents_live/ws_shell.rs` (`ws_shell` handler):
      pure byte-pump between browser and rabbit's shell channel;
      `?viewer=true` query mirrors ws_browser's read-only contract;
      drops typed bytes server-side in viewer mode.
- [x] New template `warren/templates/agent_shell.html` (xterm.js pane,
      `?viewer=true` URL flag → `#viewer-mode-banner`, channel byte
      `0x02` for outbound, drop other channels on inbound).
- [x] Routes registered in `warren/src/main.rs`
      (`/agent/:id/shell/ws`) and `warren/src/routes/ui.rs`
      (`/agent/:id/shell` HTML page + `AgentShellTemplate`).
- [x] `cargo check --all-targets`/`clippy -- -D warnings` clean on
      both crates; existing 4 viewer-mode tests still pass.

**Rabbit side — DONE this session:**

The supervisor used to manage a single PTY (claude). It now runs an
optional second PTY (bash) on its own channel:

- [x] Channel-aware `LinkCmd::SendBinary { chan, data }` and
      `LinkEvent::Binary { chan, data }` in `rabbit/src/link.rs`. Outbound
      frames carry whatever channel the producer tags; inbound frames now
      accept **both** `TERM_CHAN_CLAUDE` and `TERM_CHAN_SHELL` (still drops
      unknown channels) and forward the channel byte through to the
      supervisor instead of stripping it.
- [x] Inbound routing in `rabbit/src/supervisor.rs`: the
      `LinkEvent::Binary { chan, .. }` arm routes shell-channel bytes to the
      shell PTY and everything else to the active claude PTY.
- [x] Output tagging: the claude PTY read loop now emits
      `LinkCmd::SendBinary { chan: TERM_CHAN_CLAUDE, .. }`; the shell task
      emits `TERM_CHAN_SHELL`.
- [x] New `rabbit/src/shell.rs` — a self-contained second-PTY manager.
      It deliberately does **not** reuse claude's `spawn_run_one`
      machinery (no crash-window, no session resume, no graceful-ESC, no
      trust dialog): on exit it just respawns after a 250ms delay until
      shutdown. Reader and writer live on **separate OS threads** so an
      idle `bash` (zero output) can't starve typed input the way a shared
      read/try-write loop would. Spawned from the supervisor only when
      `config.enable_shell` is set (`ENABLE_SHELL=1`), registered as
      `pub mod shell` in `rabbit/src/lib.rs`. 1 unit test drives a real
      `/bin/cat` generation and asserts a write round-trips back out tagged
      `TERM_CHAN_SHELL`.
- [x] **Warren gap fixed** (the earlier "warren side done" pass missed
      it): `actor.rs::Command::SendKeys` hardcoded `TERM_CHAN_CLAUDE`, so
      shell keystrokes from `ws_shell` would have landed on **claude's**
      PTY. `SendKeys` and `AgentHandle::send_terminal_bytes` now carry a
      `chan` byte; `ws_shell` sends `TERM_CHAN_SHELL`, `ws_browser` sends
      `TERM_CHAN_CLAUDE`. Without this the `/shell` page would silently
      steer claude.
- Config knobs (`ENABLE_SHELL`, `SHELL_BIN` default `/bin/bash`,
      `SHELL_ARGS` default `-i`) were already added in a prior session.
- Verified: `cargo check --all-targets`/`clippy -- -D warnings`/`test`
      clean on both crates (62 rabbit lib tests incl. the new shell test,
      +14 rabbit integration, 4 warren viewer-mode tests).

---

## Done in a prior session (cleanup + prompt policy)

- [x] **Reject-when-Running prompt policy** (§D highest-leverage decision).
      New `ObserverHandle::latest_state()` tracks lifecycle state, folded in
      via a testable `ingest()` method. Supervisor loop gates `Prompt` frames:
      a prompt arriving while `Running` is bounced with a `warn` `Log` meta
      event; control frames still pass. 4 new unit tests in
      `observer/hooks.rs` pin the transitions.
- [x] **Dropped unused deps** `notify` + `clap` from `rabbit/Cargo.toml`.
- [x] **Removed dead `LinkCmd::SendTextRaw`** arm + match arm (was the stale
      `SendText(Envelope)` item).
- [x] **Removed `_unused_*` stubs** in `supervisor.rs` + orphaned
      `TurnDone`/`UsageSnapshot` imports.
- [x] **§A.10 `src/lib.rs` refactor** — modules are now `pub mod` in a
      library crate; `main.rs` is a thin wrapper over `rabbit::run()`;
      `claude_smoke.rs` moved to `tests/claude_smoke.rs`. Unblocks the 4
      planned integration-test files + downstream embedders. Fixed 3 clippy
      lints the new public surface exposed.
- [x] **All 4 §A.10 integration-test files** written (`pty_echo`,
      `input_discipline`, `transcript_parse`, `integration`) + shared
      `tests/common/mod.rs`. CI-runnable; +13 tests.
- [x] **Fixed transcript-tailer panic** — `scan_once` used `blocking_send`
      inside an async task (panics); now collects into a `Vec` and `run`
      awaits `tx.send`. Extracted `supervisor::should_reject_prompt` as a
      `pub fn` for testability.
- Verified: `cargo check`/`clippy --all-targets`/`test -p rabbit` all clean
      (60 passed across 9 binaries, 1 ignored smoke test).

### Discovered / notes for next time

- **Rejection UX is a stub**: rejections surface as a generic `warn` `Log`
  line. A dedicated wire variant (e.g. `PromptRejected { reason }`) + warren
  UI affordance would be cleaner, but touches warren (`http.rs`,
  `agent_claude.html`) — deferred.
- **State-tracking gap** — RESOLVED. `latest_state` now advances on both hook
  events *and* the supervisor's own transitions: `send_state` takes the
  `ObserverHandle` and calls `observer.set_state(State::from_label(..))`, so
  initial `idle`, clean-exit `ended`, and crash/shutdown `dead` all update
  `latest_state()`. Added `State::from_label` (inverse of `as_str`) +
  `ObserverHandle::set_state`; 3 tests pin the roundtrip and the send_state
  advance (incl. unknown-label no-op). `latest_state()` is now authoritative
  for the whole lifecycle, not just the hook subset.
- **`claude_smoke.rs` lives inside the bin** because modules are private in
  `main.rs` — this is exactly what the §A.10 `lib.rs` refactor unblocks.
  That refactor is the next highest-leverage item.

---

## Done in a prior session

- [x] **`--model`** wired into `claude_args` (`rabbit/src/supervisor.rs:108`).
- [x] **Bracketed paste** in `input.rs::paste()` — `Ctrl-U + ESC[200~ + text
      + ESC[201~ + \r` per §A.2; 5 unit tests pin the byte sequence.
- [x] **Transcript path** now sourced from the `SessionStart` hook payload
      (`observer/hooks.rs::parse`), with `TranscriptTail::with_observer`
      following it live. Fallback `workdir/.claude/transcript.jsonl` until
      the hook fires.
- [x] **`parse_errors` counter** on every `Usage` envelope
      (`UsageSnapshot.parse_errors: u64`, both sides, `#[serde(default)]`
      for back-compat).
- [x] **`context_pct_est`** computed from `input + cache_read` vs. a model
      window table (Sonnet 4 = 1M, everything else = 200k).
- [x] **`--continue` fallback** in `respawn::effective_args` when no session
      id is known and `fresh=false`.
- [x] **Browser `Resize` envelope** now routes through `handle.resize()`
      instead of typing a private xterm escape (`ws_browser.rs`, plus the
      new `AgentHandle::resize`).
- [x] **Smoke test** (`rabbit/src/claude_smoke.rs`, `#[ignore]`) spawns real
      `claude` in a 120×40 PTY, sends a small prompt via §A.2 bracketed
      paste, detects-and-responds to the trust dialog, and asserts the
      model responds with the requested token.

---

## §D Milestone 4 — Hardening + HTTP API (mostly done, gaps below)

- [x] **Verify seq/ack meta replay** end-to-end. Done: `rabbit/tests/meta_replay.rs`
      stands up a real local WS server, drives meta events through the live
      `Link`, drops the connection mid-stream, and asserts the buffered events
      replay on reconnect **with their original seq**. A second test proves
      `EnvelopeBody::Ack { ack_seq }` trims the ring so acked events are not
      replayed (using an event-channel barrier for deterministic ordering).
      `tokio-tungstenite`/`futures-util` added to `[dev-dependencies]`.
- [x] **PVC story** (§A.7): operator-side. Documented in `rabbit/README.md`
      ("Kubernetes deployment"): mount **both** `/work` and `~/.claude` on a
      PVC so `--resume`/`--continue` survive pod restarts, with the respawn
      policy spelled out (why an `emptyDir` silently loses the session).
- [x] **k8s probes** — `/healthz` and `/readyz` exist (`health.rs`); the
      `terminationGracePeriodSeconds: 30` is operator-side. The supervisor's
      shutdown budget is now an explicit, unit-tested contract:
      `supervisor::graceful_expired(elapsed, grace_period, child_alive)` bounds
      shutdown latency at `SHUTDOWN_GRACE_MS` (default 1500ms « 30s), and
      `rabbit/README.md` documents the probe + grace-period wiring. (Cold-start
      `claude --resume` latency is a *startup*, not *shutdown*, concern —
      absorbed by readiness, not the termination grace period.)
- [x] **Prompt queue policy** (§D open question) — **decided: reject-when-Running**.
      `dispatch_to_pty` is now gated in the supervisor loop
      (`supervisor.rs`): a `Prompt` arriving while `observer.latest_state()
      == Running` is bounced with a `warn` `Log` meta event instead of
      injected into the PTY. Control frames (Interrupt/Slash/Clear/Resize/
      Repaint) still pass unconditionally. State is tracked via the new
      `ObserverHandle::latest_state()` (updated in `ingest`); 4 unit tests
      in `observer/hooks.rs` pin the transitions.

## §D Milestone 5 — Extras (not started)

- [x] **Server-side VT state** via `avt` (asciinema's terminal) — feeds
      late joiners a precise screen dump + cursor state, replaces the
      SIGWINCH jiggle. The jiggle works but is the *v1* hack. **DONE**
      — Phases A/B/C all in: passive `TermTracker` in the claude PTY
      loop (`rabbit/src/vt.rs` + `Cargo.toml` dep + `supervisor.rs`
      feed/resize); `ScreenSnapshot` + `SnapshotRequest` wire envelopes
      (mirrored in rabbit + warren); `PtyCmd::Snapshot` computes the
      dump in the blocking thread (owns `vt`, no mutex); `ws_browser.rs`
      requests a snapshot after the replay buffer and `applyMeta` paints
      it; the 250 ms SIGWINCH jiggle is retired (operator-initiated
      `Repaint` path preserved). 6 vt unit tests + 3 wire-roundtrip
      tests cover it.
- [x] **Asciicast recording** + `/agent/:id/claude/history` page with
      `asciinema-player`. **DONE** — recorder sidecar in
      `rabbit/src/recorder.rs` (v2 header + UTF-8 lossy + per-session
      rotation up to `MAX_ROTATION_DEPTH=64`); HTTP endpoint in
      `rabbit/src/http_server.rs` (`/sessions`, `/casts/:filename`,
      `/healthz`, Bearer auth + strict filename allow-list +
      canonicalize-escape check); wired into the supervisor driver task
      on `enable_asciicast` (`PtyEvt::Read` → `feed`,
      observer `session` → `start_session`, `PtyEvt::Exited` → `close`).
      URL discovered via the Hello envelope (no DB migration).
      Warren side: `routes/recording.rs` (Bearer-authed `reqwest` proxy
      with `is_safe_recorder_url` SSRF guard), `/agent/:id/claude/history`
      list page (eager list, most-recent first, total size + segment
      count per session), `/agent/:id/claude/history/:session` play page
      embedding vendored `asciinema-player@3.17.0` (Apache-2.0;
      `warren/static/vendor/asciinema-player/` with LICENSE + NOTICE).
      "→ history" link gated on `recorder_enabled` in
      `agent_claude.html`. Tests: 7 recorder unit tests (header/frame
      shape, rotation, UTF-8 replacement, multi-session, broken-after-
      error, empty-chunk noop, JSON-escape control chars) + 8
      http_server unit tests (filename allow-list accept/reject,
      segment ordering, healthz, 401, 400, 200 + correct content-type,
      grouping + mtime order). `cargo build --workspace` +
      `cargo clippy --workspace --all-targets -- -D warnings` +
      `cargo test --workspace` all clean (83 rabbit lib + 8 warren bin +
      integration suites green).
- [x] **`/agent/:id/shell`** — extra PTY (`bash`) on the same rabbit, new
      binary channel ids. Debug access for free. **DONE** — see the
      `/agent/:id/shell` section above.
- [x] **Leader-based resize** — single client controls size; others see
      the resulting screen state. **DONE** — manual-claim model (claims
      always succeed; transfers from a prior leader are allowed even when
      still connected; no auto-promotion on disconnect). Wiring:
        * 4 new envelope variants in `warren/src/agents_live/wire.rs`:
          `ConnectionAssigned { connection_id }` (server → browser on WS
          open, sent directly on the WS not via meta broadcast),
          `ClaimLeader { cols, rows }`, `ReleaseLeader`,
          `LeaderChanged { leader_id: Option<Uuid>, cols, rows }`.
          snake_case serde tags.
        * `AgentHandle::leader` field (`Arc<Mutex<Option<LeaderInfo>>>`)
          with sync mutator accessors `claim_leader`, `release_leader`,
          `clear_leader_if`, `current_leader`, `is_leader`,
          `update_leader_size`. Public `cmd_tx()` accessor so the
          browser side can dispatch fire-and-forget commands.
        * 4 new `actor::Command` variants — `ClaimLeader`,
          `ReleaseLeader`, `ResizeFromConnection`, `ConnectionClosed` —
          with the dispatch logic reading the pre-update term_size
          first so the inherit-then-resize comparison isn't shadowed
          (a bug-fix detail; see actor.rs comments).
        * `AgentStateSnapshot.term_size` is now refreshed on Hello and
          on each leader-driven Resize; sticky on plain State updates.
        * `ws_browser.rs` generates a per-tab `Uuid` connection_id, sends
          `ConnectionAssigned` directly on the WS, routes the 4 new
          text envelopes to commands, drops non-leader `Resize` at the
          ws_browser boundary (defense in depth: actor also checks
          `is_leader` in `Command::ResizeFromConnection`), sends
          `Command::ConnectionClosed` on every WS break.
        * `agent_claude.html` adds Claim/Release buttons + leader
          badge in the actions aside (hidden in viewer mode via
          existing CSS), new `connection_assigned` and
          `leader_changed` cases in `applyMeta`, `setLeaderUi` snap
          logic that hides Claim/Release based on `myConnectionId`,
          clamps incoming `(cols, rows)` to a safe range before
          follower `term.resize()`, calls `fit.fit()` before reporting
          the leader's grid on Claim (handles "user dragged window
          before claiming"), and `term.onResize` listener gated on
          `isLeader` so followers never echo a synced grid back as a
          Resize frame. Window-resize coalescing via
          `requestAnimationFrame`.
        * Recording shows the leader's grid only (no `"r"` events on
          mid-session resize — documented v1 limitation; asciicast v2
          header + text are still faithful at the new layout).
      Test coverage:
        * 4 wire serde roundtrips (`connection_assigned`,
          `claim_leader`, `release_leader`, `leader_changed` with
          both Some/None).
        * 11 handle tests (claim/release/clear_leader_if permutations,
          is_leader, update_leader_size, full_state_machine_walk, and
          term_size sticky/refreshed semantics).
        * 5 actor tests (broadcasts on claim/release, connection-
          closed behavior, handle-level resize-gate mirror).
        * 7 ws_browser tests (claim/release text → command routing,
          leader-accepted vs. non-leader-dropped resize, viewer-mode
          drop for the 4 new frames, inbound ConnectionAssigned /
          LeaderChanged silently ignored).
      `cargo test --workspace` (35 warren + 85 rabbit lib + 4 smoke +
      integration suites) and `cargo clippy --workspace --all-targets
      -- -D warnings` both clean. `ActorHandle` gained `Debug`
      (test-support derivable). Rabbit side is unchanged. Manual
      smoke checklist goes in the commit message.
- [x] **Per-agent read-only viewer mode** — admin-only input v1 is the
      current default; read-only viewer is the explicit toggle. **DONE**
      — see the dedicated entry at the top of this list (`?viewer=true`
      query param + server-side enforcement + UI banner).

## §A.10 Test structure — DONE this session

All four integration-test files now exist alongside `claude_smoke.rs`, sharing
`tests/common/mod.rs` (PTY reader + `/bin/cat` fake-TUI helpers). CI-runnable
(no API, no live `claude`); the live TUI roundtrip stays in the `#[ignore]`d
`claude_smoke.rs`.

- [x] `rabbit/tests/pty_echo.rs` — byte-pump round-trip through a real PTY
      against a `/bin/cat` fake TUI + replay-buffer contract (4 tests).
- [x] `rabbit/tests/input_discipline.rs` — `input::paste`/`slash`/`interrupt`
      driven into a real PTY master; asserts a real tty consumer receives the
      payload (3 tests).
- [x] `rabbit/tests/transcript_parse.rs` — fixture-driven `TranscriptTail::run`
      over a temp `.jsonl`: token counts, `context_pct_est` (200k + 1M
      windows), and `parse_errors` increment on malformed lines (3 tests).
      **Surfaced + fixed a real bug** (see below).
- [x] `rabbit/tests/integration.rs` — milestone-1 dispatch discipline: drives
      the real `ObserverHandle` state machine (prompt→Running→Stop→Idle) and
      the extracted `supervisor::should_reject_prompt` gate; verifies control
      frames (ESC/slash/`/clear`) are never gated (3 tests).

### Bug fixed while writing `transcript_parse.rs`

`TranscriptTail::scan_once` sent usage via `tx.blocking_send()`, but `run` is
spawned as a Tokio task — and `blocking_send` **panics in an async context**.
The tailer's task therefore panicked on the *first* usage line and silently
died, so context/usage reporting never worked past line one. Fixed: `scan_once`
now collects `UsageUpdate`s into a `Vec` and the async `run` loop awaits
`tx.send()`. `transcript_parse.rs` is the regression guard. Also extracted the
prompt-gate decision into `pub fn supervisor::should_reject_prompt(state, body)`
so it's testable from `tests/`.

To make these tests linkable without restructuring, either:
- [ ] ~~Promote the crate's `pub mod`-ness for `pty`, `input`, …~~ (rejected)
- [x] **Add a `src/lib.rs`** that re-exports the modules and have `main.rs`
      become a thin wrapper around `rabbit::run`. **DONE this session.**
      `src/lib.rs` now `pub mod`-exports every module + a `pub fn run()`
      carrying the supervisor entrypoint; `main.rs` is a 3-line wrapper.
      `claude_smoke.rs` moved to `tests/claude_smoke.rs` (uses
      `rabbit::input::paste`), proving external integration tests link.
      Surfaced + fixed 3 clippy lints exposed by the new public surface
      (`Default` for `HealthState`/`ObserverHandle`, `MetaRing::is_empty`).
      `cargo clippy --all-targets` + `cargo test -p rabbit` clean (47 pass).

The second is the cleaner long-term fix and unblocks external integration
tests + downstream crates that want to embed rabbit's PTY/observer pieces.

**The 4 §A.10 test files above are now unblocked** — they can be written as
`tests/*.rs` files using `rabbit::{pty, input, observer, wire, respawn}`.

## §D Open questions (need a decision before milestones 4–5 land)

- [x] **warren horizontal scaling** — **decided: sticky-by-agent-id
      for v1, broker hop deferred.** Single-instance warren remains the
      default deployment; if we scale out, route by agent id (the hub
      actor's owning warren) using a sticky load-balancer rule keyed on
      `agent_id` in the WS upgrade path. The broker hop (warren as a
      thin client to a central broker) is a v2 concern — its DB
      implications are limited because no schema today has a warren FK.
      Cutover gate: >50 concurrent active agents on a single warren, or
      >3 warren instances before we build the broker. Tracked under
      Milestone 5 backlog but not blocking any current milestone.
- [x] **Prompt policy while `Running`** — **decided & implemented: reject**.
      See Milestone 4 above.
- [x] **Auth granularity for webshell** — **decided: admin-only input v1,
      viewer-mode for any authenticated user v1.5.** Now that
      viewer-mode is implemented (`?viewer=true` on
      `/agent/:id/claude[/shell]?viewer=true`), the server-side drop-list
      already enforces read-only regardless of client. v1 keeps the
      existing `auth::validate_admin_session` gate on the WS upgrade
      path. v1.5 will split the gate:
        * `validate_admin_session` → input WS (`/claude/ws` no flag,
          `/shell/ws` no flag) + all POST endpoints
          (`/api/agents/:id/claude/*`).
        * `validate_any_session` → viewer WS
          (`/claude/ws?viewer=true`, `/shell/ws?viewer=true`).
      The split is a 4-line change in `ws_browser.rs` + `ws_shell.rs`
      (`auth::validate_admin_session` → `auth::validate_any_session`,
      gated on the `viewer` flag). No UI affordance yet — it's URL-only.
      Plan to add a "Share view-only link" button on the agent page in
      v1.5 once we have at least one non-admin user story.
- [x] **Retention** — **decided (deferred to asciicast work).**
      Defaults, pending Task #6 implementation:
        * asciicast files: 10 MiB per session, rotate on size or session
          boundary (whichever first). Older segments kept in a
          `<session-id>.cast.0`, `.cast.1`, ... series; oldest evicted.
        * `agent_events` JSON payload: 256 KiB truncation ceiling, with
          `full_size` storing the original byte count for non-destructive
          inspection via direct DB query. Tool outputs (the most common
          culprit) get truncated first; `transcript_msg` payloads stay
          full since they're line-oriented JSONL we already parse.
        * `usage` rows: never truncated (small, structured).
      Concrete code change is a single `truncate_payload` helper in
      `warren/src/db_ops.rs` called from the `persist_event` path.
- [x] **Cost display** — **decided: in-terminal `/cost` slash command
      for v1.** Reason: a price table is its own schema concern (which
      provider, which model, what unit — Anthropic's per-MTok pricing
      changes quarterly), and adding it to the `agents` table is a
      migration that needs a maintainer commitment to keep it fresh.
      The `/cost` command reads the latest `UsageSnapshot` from the
      transcript tail and renders a single-shot readout using a small
      built-in price table (Sonnet 4 = $3/$15 per MTok in/out, Opus 4.8
      = $15/$75, Haiku 4.5 = $0.80/$4). The slash command is gated on
      `Running` like other input frames — see Task #1's rejection UX
      (a `/cost` mid-turn would not be useful anyway, claude is busy).
      UI cost display is a v1.5 task once the price table schema lands.

## Smaller items noticed but not addressed

- [x] **`rabbit/src/link.rs`** dead `LinkCmd::SendText(Envelope)` arm —
      resolved. It had already been renamed to `SendTextRaw(String)` (an
      unused escape hatch); removed it and its match arm. `LinkCmd::Shutdown`
      remains as a still-`dead_code` teardown primitive.
- [x] **`notify` and `clap` deps** — confirmed unused in `rabbit/src` and
      dropped from `rabbit/Cargo.toml`. `cargo tree` is now honest.
- [x] **Smoke test** could be tightened: it currently exercises one prompt
      with the trust dialog. Worth adding `/clear` (which triggers the
      transcript reset), `ESC` interrupt (mid-response), and a slash
      command (`/usage`) — these are the §A.10 milestone-1 checklist.
      **Done this session** — see "Done this session" above.
- [x] **`MetaRing`** (`rabbit/src/meta_ring.rs`) — bounded byte ring for
      unacked meta events on WS reconnect. **Verified: all meta events feed the
      ring.** Every structured event routes through `LinkCmd::SendMeta`
      (`send_state`→State, transcript relay→Usage, `forward_observer_event`→
      Session/State/PromptEcho/StopHook/Log, prompt-rejection→Log), and
      `link.rs` unconditionally `meta_ring.push()`es every `SendMeta` frame
      before sending. Only raw PTY bytes (`SendBinary`) bypass the ring — by
      design, they use the separate bounded replay buffer (§A.6). End-to-end
      replay + Ack-trim now covered by `tests/meta_replay.rs`.
- [x] **Trust-dialog detection in the supervisor itself.** Done. Promoted the
      smoke test's marker logic into `rabbit/src/trust.rs` (`TrustWatcher` +
      `has_trust_marker`, the shared source of truth the smoke test now imports)
      and wired it into `spawn_run_one`'s PTY read loop: on a fresh workdir the
      supervisor detects claude's "trust this folder?" dialog and auto-accepts
      with Enter, bounded to 3 accepts so a false positive can't storm the PTY.
      Gated by `AUTO_TRUST` (default on; `README.md` documents it). 7 unit tests
      pin detection, chunk-boundary reassembly, budget, and window bounding.

## Verification gates for each milestone

- Milestone 4 close-out: `cargo test -p rabbit --bins` clean +
  `tests/integration.rs` (above) green against a pinned `claude` version
  + k8s `terminationGracePeriodSeconds=30` respected under SIGTERM during
  an active turn.
- Milestone 5 close-out: `avt`-backed `/agent/:id/claude/history` page
  renders identically to a live xterm.js session; asciicast files
  rotatable; `/shell` works against a fresh pod. **DONE this session**
  — see the asciicast entry under §D Milestone 5 below.

---

## Open question I'd resolve first — RESOLVED 2026-07-04

The single highest-leverage decision was the **prompt policy while `Running`**
(§D open question). **Resolved: reject-when-Running**, and implemented at the
supervisor dispatch layer (`supervisor.rs`) plus `ObserverHandle::latest_state()`
(`observer/hooks.rs`). Rejections surface as a `warn` `Log` meta event; wiring
that into a dedicated warren/UI affordance (`http.rs`, `agent_claude.html`)
instead of a generic log line is the natural follow-up.

---

## §D Milestone 5 — Server-side VT state via `avt` (design only)

**Goal:** Replace the SIGWINCH jiggle (`rabbit/src/supervisor.rs:449`)
with a precise server-side terminal state so late joiners receive an
authoritative screen dump + cursor state, not a heuristic redraw.

**Library:** [`avt`](https://docs.rs/avt/latest/avt/) (asciinema virtual
terminal) v0.18.0 — Apache-2.0. Parses a PTY byte stream into a
virtual-terminal state machine and exposes screen contents, cursor
position, dirty-line tracking, and resize.

### avt API surface we'll use

```
let mut vt = avt::Terminal::new((cols, rows), scrollback_limit);
let mut parser = avt::Parser::new();

for chunk in pty_read_chunks {
    for func in parser.feed(&chunk) {       // returns Vec<Function>
        vt.execute(func);
    }
    // (send chunk to warren as today)
}

// On late-joiner request:
let text = vt.text();              // Vec<String>, one per visible row
let cursor = vt.cursor();          // (col, row)
let dirty = vt.changes();          // Vec<usize>, line indices since last call
let size = vt.size();              // (cols, rows)
vt.resize(new_cols, new_rows);     // bool, changed?
```

Notable gap: only ~3% of the crate's API surface is documented in the
docs.rs index. We'll pin to `avt = "0.18"` and pin a specific minor if
0.x churns. Property tests will catch silent breakage (see Test strategy
below).

### Where `Vt` lives — rabbit side

The supervisor's blocking task is the single owner of the PTY read loop,
so it's the single owner of `Vt`:

```
// rabbit/src/supervisor.rs (inside spawn_run_one's blocking task)
let mut vt = avt::Terminal::new((cols, rows), 5_000);   // scrollback limit
let mut parser = avt::Parser::new();

loop {
    let n = reader.read(&mut io_buf)?;
    // 1. Parse + execute on the VT (server-side ground truth).
    for func in parser.feed(&io_buf[..n]) {
        vt.execute(func);
    }
    // 2. Forward raw bytes to warren (live view stays byte-for-byte).
    pty_evt_tx.blocking_send(PtyEvt::Read(io_buf[..n].to_vec()))?;
    // (snapshot path TBD — see "On-demand snapshot" below)
}
```

The existing PtyEvt::Read → LinkCmd::SendBinary path is unchanged for
the live case. avt is a passive observer on the same byte stream; it
doesn't add latency.

### Replacing the SIGWINCH jiggle — on-demand snapshot

Today, `ws_browser.rs:51-64` spawns a task that sleeps 250ms then sends
`Repaint` (a SIGWINCH jiggle) to coerce claude into redrawing. The
jiggle is the v1 hack; the SIGWINCH might not land if the TUI has
already settled, or it might double-paint if the timing is off.

**Replacement flow:**

1. Late joiner connects to `/agent/:id/claude/ws`.
2. `ws_browser.rs` sends the existing replay buffer (raw bytes) so
   xterm.js has the recent visual history.
3. After the replay flushes, `ws_browser.rs` sends a new wire envelope
   `ScreenSnapshot { cols, rows, cursor_row, cursor_col, text: Vec<String> }`
   to the browser.
4. The browser's `applyMeta` clears xterm.js (`term.reset()`) and writes
   the snapshot directly: `term.write(text.join('\n'))` + reposition
   the cursor. No SIGWINCH; no jiggle.

For the snapshot to be available, rabbit needs a way to *request* one
on demand. Two options:

- **A. warren requests, rabbit responds.** New envelope
  `SnapshotRequest { chan: u8 }` from warren → rabbit →
  `ScreenSnapshot` response. One round-trip per join.
- **B. rabbit pushes proactively.** The supervisor sends a snapshot
  every N seconds OR on every state transition. Cheaper, but wasteful
  for a stable screen.

**Pick A.** A request/response model is simpler — the request is
implicit in the WS upgrade, the response is a single envelope, and
there's no idle traffic.

### Wire protocol additions

```
// New envelope body (rabbit → warren, on snapshot request):
ScreenSnapshot {
    chan: u8,           // TERM_CHAN_CLAUDE (0x01) or TERM_CHAN_SHELL (0x02)
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    text: Vec<String>,  // rows.length == rows, each String length == cols
}

// New envelope body (warren → rabbit, on WS join):
SnapshotRequest {
    chan: u8,           // which terminal to snapshot
}
```

Add to both `rabbit/src/wire.rs` and `warren/src/agents_live/wire.rs`,
mirror the `PromptRejected` pattern. Update
`warren/src/agents_live/actor.rs::envelope_kind` for the
`"screen_snapshot"` / `"snapshot_request"` strings.

### Implementation phases

1. **Phase A — add `avt` to `rabbit/Cargo.toml`** (one-line dep),
   instantiate `Terminal` + `Parser` in the supervisor's blocking task,
   feed every read chunk through. No wire change yet. Verify with a
   property test that a known byte stream reconstructs to the expected
   `Terminal::text()`. **[x] DONE** — `rabbit/src/vt.rs::TermTracker`
   wraps `avt::Vt` (asciinema virtual terminal); instantiated in
   `supervisor::run` after `graceful_since`, fed on every PTY read, and
   resized alongside `PtyCmd::Resize`. `ScreenSnapshot::snapshot()` is
   wired (cursor + grid + dimensions) but stays `dead_code` until Phase
   B. UTF-8 is buffered across chunk boundaries so a multibyte codepoint
   split across two 4 KiB reads still assembles cleanly (6 unit tests,
   including the split-across-three-feeds emoji case). No wire change.
2. **Phase B — wire `ScreenSnapshot` + `SnapshotRequest` envelopes.**
   rabbit responds to `SnapshotRequest { chan: 0x01 }` by serializing
   `vt.text() + vt.cursor()` into the envelope. warren's
   `ws_browser.rs` sends `SnapshotRequest` immediately after sending
   the replay buffer. warren's `applyMeta` learns `case 'screen_snapshot':`.
   **[x] DONE** — added `ScreenSnapshot` + `SnapshotRequest` to
   `rabbit/src/wire.rs` and `warren/src/agents_live/wire.rs`;
   `envelope_kind` returns `"screen_snapshot"` / `"snapshot_request"`.
   New `PtyCmd::Snapshot { chan }` in the rabbit supervisor; the blocking
   PTY thread (which owns the `TermTracker`) computes `vt.snapshot()`,
   pushes the result back via `PtyEvt::Meta(EnvelopeBody::ScreenSnapshot)`,
   and the driver loop forwards it to warren over `LinkCmd::SendMeta`.
   `ws_browser.rs` sends `SnapshotRequest` alongside the existing repaint
   jitter (Phase C will retire the jitter). `agent_claude.html`'s
   `applyMeta` resets xterm.js, writes the rows joined by CRLF, and
   repositions the cursor with `CSI H`. `Command::SnapshotRequest` +
   `AgentHandle::snapshot_request(chan)` added to the actor. Shell channel
   is a no-op for now (only claude has a `TermTracker`). 3 new tests in
   `rabbit/tests/snapshot_roundtrip.rs` cover the inbound request, the
   outbound snapshot envelope, and the wire tag shape.
3. **Phase C — delete the SIGWINCH jiggle.** The 250ms sleep +
   `Repaint` jiggle in `ws_browser.rs:51-64` becomes a no-op (or is
   removed). `supervisor.rs:449` `PtyCmd::Repaint` path stays for
   operator-initiated repaints but no longer fires on browser joins.
   **[x] DONE** — the spawned `tokio::time::sleep(250ms)` + `repaint()`
   task in `ws_browser.rs` is gone; the browser now relies on the
   `ScreenSnapshot` from Phase B for late joins. The `Repaint` wire
   envelope, `AgentHandle::repaint()`, `Command::Repaint`, and
   `PtyCmd::Repaint { cols, rows }` all stay intact for operator-facing
   tools that want to force a SIGWINCH jiggle on demand — the wire
   path is unchanged, only the browser-join call site is removed.

### Test strategy

- **Property test** in `rabbit/tests/avt_parse.rs` (new file):
  feed `avt::Terminal` a stream of `clear_screen + move_cursor(0,0) +
  "hello\nworld\n"`, assert `text()` returns
  `["hello", "world", "", "", ...]` for the right dimensions. Proptest
  with random ANSI sequences generated from `avt::parser`'s grammar
  → round-trip back through `text()` to assert cursor + cells are
  consistent.
- **Integration test:** spawn a real `/bin/sh -c "echo hello; sleep 1; echo world"`
  PTY, capture the bytes, feed them to a `Terminal`, assert the final
  `text()` contains `"hello"` on row 0 and `"world"` on a later row.
- **Determinism test** (regression guard): record a byte stream from a
  real claude session into a fixture file, assert the resulting
  `ScreenSnapshot` JSON matches a golden file. Locks the wire format.
- **WS roundtrip test:** extend `rabbit/tests/meta_replay.rs` pattern
  to stand up a real local WS server, drive a `SnapshotRequest` →
  `ScreenSnapshot` round-trip, assert the JSON matches.

### Risks

- **API churn.** avt is 0.x; pin a minor. If 0.19 breaks the surface,
  hold the upgrade behind a feature flag.
- **Parser state boundaries.** `avt::Parser::feed` may buffer an
  incomplete escape sequence across chunks. We're already reading in
  4 KiB chunks and feeding the whole chunk — incomplete sequences are
  handled by the parser's internal buffer. Test: a sequence split
  mid-escape must round-trip correctly.
- **Snapshot size.** 120×40 = 4800 cells, mostly ASCII ≈ 5 KiB JSON.
  Plus existing replay buffer (256 KiB cap). Total ≈ 261 KiB per
  join — fine for a WS frame, no chunking needed.
- **Multi-channel.** `ScreenSnapshot { chan }` already carries the
  channel byte; Phase B lights up shell snapshots for free once claude
  works.

### Open questions for review

- Should `ScreenSnapshot` be **one envelope per row** (smaller frames,
  streamable) or **one envelope** with `text: Vec<String>` (simpler)?
  *Default: one envelope.* xterm.js can render a 5 KiB text array in
  one frame, and a single request/response is cleaner than N.
- Should `vt.changes()` (incremental dirty lines) be used instead of
  `vt.text()` (full snapshot)? *Default: full snapshot for the
  late-joiner case* — dirty lines are an optimization for the
  live-stream case, which we already have via raw bytes. Incremental
  snapshots could be a v1.5 optimization if profiling shows the full
  snapshot is too heavy.
- Where does the snapshot live long-term? For Milestone 5's asciicast
  work (Task #6), snapshots become asciicast frames; the `Vt`
  becomes the producer side of the recorder. So this design feeds
  directly into Task #6 with no rework.