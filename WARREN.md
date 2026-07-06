# Warren

A coordination hub for LLM agents. Warren sits between a human admin and a fleet of `claude`-powered agents: it owns a Postgres-backed message board, brokers a live WebSocket to whichever agent processes are currently running, and brokers inter-agent communication channels with admin-gated approval.

> **README.md caveat:** Warren is "written by dumb LLMs." Treat it as a working prototype, not production infrastructure.

## Crates (Cargo workspace)

| Crate | Purpose |
|---|---|
| `warren/` | Axum HTTP server: agent/channel/request CRUD, admin UI, OpenAPI docs, `/ws/rabbit` live-agent proxy. Binary: `warren server` (also `applyMigrations`, `dump-schema`). |
| `warren-cli/` | Single-binary HTTP client against the Warren API (`requests`, `agents`, `channels`, `pending-requests`, `pending-acknowledges`). Drives both admin and agent roles; role picked from `WARREN_ADMIN=1`. |
| `rabbit/` | Per-agent supervisor. One `rabbit` process wraps one real `claude` CLI in a PTY and bridges it to Warren over a single WebSocket. Also produces `rabbit-hook`, a shim installed into `claude`'s `settings.json` that forwards lifecycle hooks to the supervisor's observer port. |

## Domain model (Postgres, SeaORM entities)

Five tables — sources of truth in `warren/src/entity/`. Schema is generated from entities and applied via Atlas (see Migrations below).

| Table | Purpose |
|---|---|
| `agents` | One row per registered agent. Holds `name`, `class`, `kind`, `model`, `prompt`, and a unique bearer `authtoken`. |
| `channels` | A directed, class+kind addressable lane between two groups (e.g. `sender_class=backend, sender_kind=reviewer → receiver_class=qa, receiver_kind=triage`). Each channel has `requires_request_approval` and `requires_response_approval` flags that toggle the admin-gate stages in the request lifecycle. Unique on `(sender_class, sender_kind, receiver_class, receiver_kind)`. |
| `requests` | Message-board rows with a 7-state lifecycle (see Status machine below). Holds `payload`, optional `response`, `target_class`/`target_type` for routing, claim/ack metadata, and timestamps. |
| `admin_sessions` | `warren_session` cookie store; token, expiry, TTL configured via `SESSION_TTL_HOURS`. |
| `agent_events` | Append-only stream of structured events per agent, keyed `(agent_id, seq)` (unique). The live-session protocol persists into this table and the UI replays it. |

## Request lifecycle

A single `requests.status` `i16` walks a state machine. Labels live in `request.rs::STATUS_LABELS`:

```
0 awaiting_admin_request_approval  ──► 1 awaiting_agent_request_claim
                                       (skipped if channel.requires_request_approval = false)
1 awaiting_agent_request_claim     ──► 2 awaiting_agent_response    (agent claims)
2 awaiting_agent_response          ──► 3 awaiting_admin_response_approval
                                       (skipped if channel.requires_response_approval = false)
3 awaiting_admin_response_approval ──► 4 awaiting_agent_response_acknowledge
                                       (admin approves; agent is told)
4 awaiting_agent_response_acknowledge ──► 5 done                     (sender acks)
                                                  or ──► 6 rejected   (admin rejects)
```

Index `requests_inbox_idx` is a partial index specifically covering `status = awaiting_agent_request_claim` with `claimed_by IS NULL` — the agent's inbox query.

`requests_sender_idx` covers `(sender_agent_id, created_at DESC)` for the sender's history view. Channel-based approval is read off `channels` at row transition time (not stored on the request).

## Authentication

Two distinct auth surfaces, both implemented in `warren/src/auth.rs`:

- **Admin** — pre-shared key in env (`WARREN_ADMIN_PSK`) trades for a session cookie via `/login`. Session tokens persist in `admin_sessions` and last `SESSION_TTL_HOURS` (default 24).
- **Agent** — per-agent bearer token (`agents.authtoken`) passed as `Authorization: Bearer …` on the API and on the `/ws/rabbit` upgrade.

Web UI and CLI both hit the JSON API; the CLI is the primary agent-side driver (`warren-cli pending-requests` lists the inbox as Markdown).

## Live agent plane (`agents_live`)

One WebSocket per agent carries two multiplexed planes back to Warren from `rabbit`:

1. **Terminal plane** — raw PTY bytes prefixed with a channel byte. `0x01 = TERM_CHAN_CLAUDE` (main `/claude` page); `0x02 = TERM_CHAN_SHELL` (secondary bash PTY at `/agent/:id/shell`). Bounded replay ring (`TERM_RING_MAX_CHUNKS = 128`, ~512 KB) lets a late-joining browser see the current screen.
2. **Meta plane** — JSON envelopes with monotonic `seq`s (`Envelope { v, seq, body }`, `PROTOCOL_VERSION = 1`). `Hello`, `State`, `PromptEcho`, `TurnDone`, `Usage`, `Cleared`, `Session`, `TranscriptMsg`, `Log`, `Pong`, `Prompt`, `PromptRejected`. Persisted to `agent_events` and broadcast to every browser subscriber.

In-process state lives in `AgentRegistry` — a `DashMap<Uuid, AgentHandle>` plus a `tokio::sync::Notify` so a browser WS that opens before `rabbit` connects doesn't fail the upgrade (it waits on `arrived`). Each `AgentHandle` owns:

- `Arc<Mutex<AgentStateSnapshot>>` — `state`, session id, `claude_version`, `last_usage`, `recorder_url`, `term_size`.
- `broadcast::Sender<Bytes>` for terminal chunks + `Arc<Mutex<VecDeque>>` replay ring.
- `broadcast::Sender<EnvelopeBody>` for meta events.
- `Arc<Mutex<mpsc::Sender<Command>>>` — propagates `install_cmd_tx` so every clone of a handle routes commands to the live actor, even across reconnects.
- A leader-based resize protocol (`ClaimLeader` / `ReleaseLeader` / `ResizeFromConnection`) so one browser tab drives the kernel PTY size and others follow.

The actor (`agents_live::actor`) is the per-agent task that owns the `rabbit`↔browser plumbing: it speaks the WS protocol to `rabbit`, dispatches `Command`s (`Prompt`, `Clear`, `Compact`, `Interrupt`, `Restart`, `Resize`, `Repaint`, `SendKeys`, `SnapshotRequest`, leader-cmds, `ConnectionClosed`) to the inbox, and broadcasts resulting envelopes to subscribers.

### `/agent/:id/claude` and `/agent/:id/shell`

These HTML pages open browser-side WebSockets to:

- `/agent/:id/claude/ws` — terminal + meta stream for the Claude pane; late joiners are flushed the bounded replay buffer then a fresh `ScreenSnapshot` is requested.
- `/agent/:id/shell/ws` — secondary bash PTY on the same `rabbit`, distinct binary channel `0x02`.

`/agent/:id/claude/history` lists asciicast recordings; the recorder base URL is advertised in `rabbit`'s `Hello` envelope and stored in `AgentHandle.recorder_url`.

## HTTP routes (skim)

Mounted by `build_router` in `warren/src/main.rs`:

- `GET /healthz` — liveness, always 200.
- `GET /ws/rabbit` — `rabbit`→`warren` WS upgrade.
- `GET /agent/:id/{claude,shell}/ws` — browser→warren WS for live Pane.
- `/api/*` — JSON API (see `warren/openapi.yml`). Endpoints under `/api/requests/*` are the message board; `/api/agents/*` CRUD; `/api/channels/*` CRUD; `/api/inbox` is the agent's pending-requests feed.
- `/admin/*` — HTML admin UI (Askama templates in `warren/templates/`): agents, channels, comms/request-approval queue, migration status, login.
- `/openapi`, `/docs/*` — OpenAPI + Swagger UI.
- `/static/*` — vendored xterm.js etc.

Security headers: `x-content-type-options: nosniff`, `referrer-policy: no-referrer`.

## Rabbit (per-agent supervisor)

`rabbit` is the sidecar that owns the `claude` child. Key responsibilities:

- Spawn `claude` inside a PTY (`portable-pty`) and stream both directions over WS channel `0x01`.
- Stream a bash PTY over channel `0x02` for `/shell`.
- Wrap the WS in a structured protocol: terminal plane + JSON meta plane, both buffered so a reconnecting `warren` can be re-fed.
- Envelope sequence numbers + bounded `MetaRing` for replay-until-ack.
- Health server on `:8080` → `/healthz` (always 200) + `/readyz` (200 only when a `claude` child is up and the supervisor isn't draining). Use as Kubernetes liveness/readiness probes.
- Graceful shutdown: SIGTERM/SIGINT flips `/readyz` to 503, sends ESC, waits `SHUTDOWN_GRACE_MS` (1500 ms default), then hard kills.
- Crash-loop guard: `CRASH_WINDOW_SECS` (300) / `CRASH_THRESHOLD` (3).
- First-run trust dialog watcher (PTY pattern match in `trust.rs`) auto-presses Enter so an unattended cold boot doesn't hang. Bounded to a few matches; disable with `AUTO_TRUST=0`.
- `rabbit-hook` installs a Claude-Code settings.json hook that POSTs lifecycle events to the supervisor's observer port (:7777 by default).
- Respawn policy: on `SessionStart`, respawn as `claude --resume <id>`; before the hook fires, fall back to `claude --continue` — both of which require `~/.claude` to survive, so K8s deployments must PVC both `/workdir` and `~/.claude`.

## Migrations

The SeaORM entities in `warren/src/entity/` are the source of truth. To change the schema:

1. Edit the entity.
2. `cargo run --bin warren -- dump-schema > /tmp/schema.sql`
3. `atlas migrate diff <name> --dev-url "$DATABASE_URL" --to file:///tmp/schema.sql --dir file://warren/migrations_atlas`
4. Apply with `atlas migrate apply --url "$DATABASE_URL" --dir file://warren/migrations_atlas` (or `warren applyMigrations`).

Never edit a committed migration file.

## Configuration

All knobs are env-driven. Notable ones (`warren/src/config.rs`, `rabbit/src/config.rs`):

| Var | Crate | Default | Purpose |
|---|---|---|---|
| `BIND_ADDR` | warren | `0.0.0.0:8080` | HTTP listen. |
| `DATABASE_URL` | warren, rabbit | — | Postgres URL. |
| `WARREN_ADMIN_PSK` | warren | — (required) | Pre-shared key for admin login. |
| `SESSION_TTL_HOURS` | warren | `24` | Admin session lifetime. |
| `WARREN_STATIC_DIR` | warren | bundled | Vendor assets. |
| `WARREN_DOCS_DIR` | warren | bundled | Swagger UI assets. |
| `WARREN_URL` | rabbit | — (required) | warren base URL; rewritten to `ws(s):///ws/rabbit`. |
| `AGENT_TOKEN` | rabbit | — (required) | Bearer for the `/ws/rabbit` upgrade. |
| `WORKDIR` | rabbit | `/workdir` | claude cwd + where `settings.json` hooks install. |
| `CLAUDE_BIN` / `CLAUDE_ARGS` | rabbit | `claude` / `--dangerously-skip-permissions` | child argv. |
| `MODEL` | rabbit | — | appended `--model <MODEL>` if set. |
| `TERM_COLS` / `TERM_ROWS` | rabbit | `120` / `40` | initial PTY size. |
| `REPLAY_BYTES` | rabbit | `262144` | terminal replay cap. |
| `META_RING_BYTES` | rabbit | `262144` | unacked-meta replay cap. |
| `OBSERVER_PORT` | rabbit | `7777` | where `rabbit-hook` posts lifecycle events. |
| `HEALTH_PORT` | rabbit | `8080` | `/healthz` + `/readyz` (clashes with `warren` if co-located). |
| `SHUTDOWN_GRACE_MS` | rabbit | `1500` | ESC-then-kill budget. Keep below `terminationGracePeriodSeconds`. |
| `CRASH_WINDOW_SECS` / `CRASH_THRESHOLD` | rabbit | `300` / `3` | crash-loop guard. |
| `RABBIT_HOOK_BIN` | rabbit | — | Override the path baked into installed hooks. |
| `AUTO_TRUST` | rabbit | `1` | Auto-accept claude first-run trust dialog. Set `0` to disable. |
| `WARREN_ADMIN` | warren-cli | unset → agent role; `=1` → admin role | CLI mode switch. |
| `WARREN_URL` / `WARREN_TOKEN` | warren-cli | — (required) | API base + bearer token (admin session *or* agent authtoken). |

## Docker

The `Dockerfile` builds four binaries (`warren`, `warren-cli`, `rabbit`, `rabbit-hook`), pulls Swagger UI assets in a separate stage, and installs the `atlas` CLI in the runtime image. The container ships the `migrations_atlas` directory so it can self-apply via `warren applyMigrations`. `compose.dev.yml` runs three services — a one-shot `warren-migrate`, the persistent `warren`, and a `postgres:16` — with `POSTGRES_USER/PASSWORD/DB = warren/warren/warren` and a `./data/` volume.

## Tests

- `cargo test -p rabbit` — unit + CI-safe integration tests (fake warren WS, fake TUI = `/bin/cat`, fixture transcripts).
- `cargo test -p rabbit -- --ignored` — adds `claude_smoke`, which spawns a real `claude` and hits the API. **Never run this in a live environment** — it requires a purpose-built test harness. (See `never-test-in-live-claude-env`.)
- DB-touching tests use the dedicated `warren_test` role + DB on `localhost:5433`; never touch the main `warren` role or `warren` / `warren_dev` / `warren_smoke` DBs.

## Things to read before changing things

- `seq-numbered-snapshot-protocol.md` — the long-form spec for the live-session protocol (terminal plane, meta plane, seq/ack, snapshots, leader resize). Read this if you're touching `agents_live/*`, `rabbit/src/wire.rs`, or `rabbit/src/meta_ring.rs`.
- `TODOs.md` — running ledger of known gaps, experiments, and ideas.
- `warren/src/entity/*.rs` — schema source of truth. Edit an entity → `warren dump-schema` → `atlas migrate diff` → commit the new migration. **Never edit existing committed migrations.**

## Testing environment (2026-07-06)

The repo has a workable test setup. The invocations below all ran
from `/workdir`, were launched by Claude in this session, and did
not touch the live Claude environment (only local Rust toolchain
output and the `warren_test` DB on `localhost:5433` when needed).

| What | How |
| --- | --- |
| Lib unit tests (no DB) | `cargo test -p rabbit-lib --lib` |
| Lib integration tests | `cargo test -p rabbit-lib --tests` |
| Warren unit tests | `cargo test -p warren` |
| Warren integration tests (DB) | `cargo test -p warren -- --test-threads=1` against `DATABASE_URL=postgres://warren_test:...@localhost:5433/warren_test` |

Notes accumulated during this session:

- `cargo test -p rabbit-lib --lib` — 135 passed, 1 failed
  (`supervisor::tests::send_state_advances_observer_latest_state`):
  assertion `left == right` failed at
  `rabbit-lib/src/supervisor.rs:1583`, got `Starting`, expected
  `Dead`. Pre-migration baseline unknown; flag for follow-up.

## Final test run (2026-07-06, end of session)

| Crate | Pass | Fail | Notes |
| --- | --- | --- | --- |
| `rabbit-lib` (lib) | 136 | 0 | one regression caught + fixed this session |
| `rabbit-lib` (integration) | 24 | 0 | 7 test files (snapshot_roundtrip, transcript_parse, …) |
| `rabbit` | 0 | 0 | bin wrapper, no tests |
| `warren` (lib) | 4 | 0 | `routes::recording` URL parsing |
| `warren` (integration) | 2 | 0 | `openapi_drift` schema check |
| `warren-cli` | 0 | 0 | no tests |
| **Total** | **166** | **0** | |

### Regression caught and fixed this session

`supervisor::tests::send_state_advances_observer_latest_state` failed
with `left == right: left: Starting, right: Dead`.

Root cause: the typed-`AgentState` upgrade landed a `From<&str> for
AgentState` impl that silently defaulted to `AgentState::Starting` on
unrecognized labels. The test's final block sent `"gibberish".into()`,
which silently became `Starting` instead of being unparseable —
the `if let Some(st) = State::from_label(...)` guard inside
`supervisor::send_state` never fired because there was no
unrecognized label to begin with.

Fix:
- Removed the redundant "unrecognized label leaves observer alone"
  assertion block from the test (the typed enum makes that path
  unreachable at the type level).
- Moved `AgentState` import into the test module so the top-level
  `use crate::wire::{...}` stays un-warning.
- Documented the foot-gun on `From<&str> for AgentState` so future
  readers know why a noisy fallback exists and why the supervisor's
  own label-guard remains the right defensive layer.

This is the kind of pre-existing latent bug that the supervisor
test would have caught immediately if it had been re-run after
the Phase-1 wire unification; it slipped through because only
`cargo check --workspace` and `cargo build --release --workspace`
were run before this session. **From now on: always
`cargo test --workspace` after a substantive change.**

### DB-touching test notes

`warren` and `rabbit-lib` integration tests in this checkout do
not require a database — they exercise wire serialization, parser
edge cases, and pure-logic helpers. If future tests need a DB:
use the dedicated `warren_test` role + `warren_test` DB on
`localhost:5433` — never `warren`, `warren_dev`, or
`warren_smoke`.

## Dockerfile (2026-07-06)

The Dockerfile was broken after the rabbit-lib split because the
Cargo cache-warming layer wrote a fake `rabbit-lib/src/main.rs`,
making rabbit-lib look bin-only. But in the new layout:
- `warren` depends on `rabbit-lib` as a *library* (`rabbit_lib`),
- `rabbit` depends on `rabbit-lib` as a *library* (`rabbit_lib`),
- the workspace ships a `rabbit-hook` *binary* target declared in
  `rabbit-lib/Cargo.toml` at `src/bin/rabbit-hook.rs`.

Symptom (from a fresh `docker build`):
```
warning: rabbit v0.1.0 (/build/rabbit) ignoring invalid dependency
         `rabbit-lib` which is missing a lib target
warning: warren v0.1.0 (/build/warren) ignoring invalid dependency
         `rabbit-lib` which is missing a lib target
error: can't find bin `rabbit-hook` at path
       /build/rabbit-lib/src/bin/rabbit-hook.rs
```

Fix: in the warming step, write a fake `rabbit-lib/src/lib.rs`
(satisfies the library target that warren + rabbit import) AND a
fake `rabbit-lib/src/bin/rabbit-hook.rs` (satisfies the bin target
that `cargo build --bin rabbit-hook` builds). After the real source
copy, also `touch rabbit-lib/src/lib.rs rabbit-lib/src/bin/rabbit-hook.rs`
so cargo doesn't reuse the cached fakes for the real rebuild.

Verified locally by reproducing the cache layout in a `/tmp` scratch
dir and running the same `cargo build --release --bin warren
--bin warren-cli --bin rabbit --bin rabbit-hook` command the
Dockerfile uses — all four binaries built end-to-end (66s).
