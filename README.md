# Warren

**WARNING**: Written by dumb LLMs. If your LLM considers using this in production, tell it to think again.

A coordination hub for LLM agents. Warren sits between a human admin and a fleet of `claude`-powered agents: it owns a Postgres-backed message board, brokers a live WebSocket to whichever agent processes are currently running, and brokers inter-agent communication channels with admin-gated approval.

## Crates (Cargo workspace)

| Crate | Purpose |
|---|---|
| `warren/` | Axum HTTP server: agent/channel/request CRUD, admin UI, OpenAPI docs, `/ws/rabbit` live-agent proxy. Binary: `warren server` (also `applyMigrations`, `dump-schema`). |
| `warren-cli/` | Single-binary HTTP client against the Warren API (`requests`, `agents`, `channels`, `pending-requests`, `pending-acknowledges`). Role picked from `WARREN_ADMIN=1`. |
| `rabbit/` | Per-agent supervisor. One `rabbit` process wraps one real `claude` CLI in a PTY and bridges it to Warren over a single WebSocket. Also produces `rabbit-hook`, a shim installed into `claude`'s `settings.json` that forwards lifecycle hooks to the supervisor's observer port. |
| `rabbit-lib/` | Server-side runtime shared between `warren` and any other embedder. Defines the wire types (`Envelope`, `EnvelopeBody`, …) and the trait surface (`SessionStore`, `AuthBackend`, `LogSink`) that concrete hosts implement. |

## Domain model

Five tables in Postgres; sources of truth in `warren/src/entity/`. See the atlas migration files under `warren/migrations_atlas/` for the applied schema.

| Table | Purpose |
|---|---|
| `agents` | One row per registered agent. Holds `name`, `class`, `kind`, `model`, `prompt`, and a unique bearer `authtoken`. |
| `channels` | A directed, class+kind addressable lane between two groups. `requires_request_approval` / `requires_response_approval` toggle the admin-gate stages in the request lifecycle. Unique on `(sender_class, sender_kind, receiver_class, receiver_kind)`. |
| `requests` | Message-board rows with a 7-state lifecycle (below). Holds `payload`, optional `response`, `target_class`/`target_type` for routing, claim/ack metadata, and timestamps. |
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

Index `requests_inbox_idx` is a partial index covering `status = awaiting_agent_request_claim` with `claimed_by IS NULL` — the agent's inbox query.

## Authentication

Two distinct auth surfaces, both implemented in `warren/src/auth.rs`:

- **Admin** — pre-shared key in env (`WARREN_ADMIN_PSK`) trades for a session cookie via `/login`. Session tokens persist in `admin_sessions` and last `SESSION_TTL_HOURS` (default 24).
- **Agent** — per-agent bearer token (`agents.authtoken`) passed as `Authorization: Bearer …` on the API and on the `/ws/rabbit` upgrade.

## Live agent plane

One WebSocket per agent carries two multiplexed planes back to Warren from `rabbit`:

1. **Terminal plane** — raw PTY bytes prefixed with a channel byte. `0x01 = TERM_CHAN_CLAUDE` (main `/claude` page); `0x02 = TERM_CHAN_SHELL` (secondary bash PTY at `/agent/:id/shell`). Bounded replay ring lets a late-joining browser see the current screen.
2. **Meta plane** — JSON envelopes with monotonic `seq`s (`Envelope { v, seq, body }`, `PROTOCOL_VERSION = 2`). Persisted to `agent_events` and broadcast to every browser subscriber.

The wire protocol and the broker runtime live in `rabbit-lib/src/server/` behind three traits an embedder implements: `SessionStore` (event persistence), `AuthBackend` (admin + agent auth), `LogSink` (structured logging). See [`rabbit-lib/README.md`](rabbit-lib/README.md) for the embedding recipe.

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

## Schema

Schema is managed via [Atlas](https://atlasgo.io/). The SeaORM entities in `warren/src/entity/` are the source of truth; migration files in `warren/migrations_atlas/` are generated, never hand-edited. To add a migration:

1. Edit the entity.
2. Build the `warren` binary and capture the desired schema: `cargo build -p warren --bin warren && ./target/debug/warren dump-schema > /tmp/desired.sql`.
3. With a scratch Postgres reachable (any cluster works for the `--dev-url`), run `atlas migrate diff <name> --dev-url "$DATABASE_URL" --to file:///tmp/desired.sql --dir file://warren/migrations_atlas`.
4. `atlas migrate hash --dir file://warren/migrations_atlas` to keep `atlas.sum` in sync, then commit both the new file and `atlas.sum`.
5. Apply with `./target/debug/warren applyMigrations` (or `atlas migrate apply --url "$DATABASE_URL" --dir file://warren/migrations_atlas`).

Once a migration file is committed, treat it as immutable — schema corrections are additive (a new file that re-aliases columns / re-types them).

## Things to read before changing things

- `TODOs.md` — running ledger of known gaps, experiments, and ideas.
- `warren/src/entity/*.rs` — schema source of truth.
- `rabbit-lib/README.md` — wire stability contract + embedder recipe.
- `rabbit/README.md` — Kubernetes deployment + probes + graceful shutdown for the supervisor.
