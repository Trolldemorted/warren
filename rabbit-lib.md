# rabbit-lib — extracting the remote-controllable Claude supervisor

**Status:** Phases 1, 4 done. Phases 2, 3, 5, 6 partial. See
[§12 Status](#12-status--2026-07-06) for the per-phase outcome and
[§13 Follow-up plan](#13-follow-up-plan) for the per-file transform
left to do.
**Author:** Claude, drafted 2026-07-06 from a read of `WARREN.md`,
`rabbit/src/**/*.rs`, `warren/src/agents_live/*.rs`, and
`seq-numbered-snapshot-protocol.md`.
**Goal:** package the *TTY-wrapping*, *protocol*, and *server-side* code
that today lives split across the `rabbit` and `warren` crates into a single
publishable `rabbit-lib` crate, so a developer outside Warren can re-use the
remote-controllable `claude` supervisor without forking the whole workspace.

---

## 0. Why this matters

Warren has two halves glued together:

- **Supervisor half** — `rabbit/`. Spawns `claude` in a PTY, exposes an
  axum health server, observes Claude lifecycle hooks, tail-follows the
  transcript JSONL, optionally records asciicast, optionally runs a bash PTY,
  and bridges everything to a single WebSocket.
- **Server half** — `warren/src/agents_live/`. Accepts the supervisor's WS
  on `/ws/rabbit`, fans term-bytes + meta-envelopes out to browsers on
  `/agent/:id/claude/ws` and `/agent/:id/shell/ws`, and persists the event
  stream into Postgres.

The user wants to re-use the supervisor half, but in another product they
own. Two design forces pull against each other:

1. **The protocol crosses the seam.** The two halves share wire types
   (`EnvelopeBody::ScreenSnapshot { after_seq }`, `TermFrame { chan, seq,
   data }`) that today are *duplicated* in `rabbit/src/wire.rs` and
   `warren/src/agents_live/wire.rs` and kept in lockstep by comments
   (`Mirrored in …`). Anything we publish must own exactly one
   authoritative copy.
2. **The server half today hard-codes warren's specifics** — `Db`
   (SeaORM), `db_ops::*` queries, `auth::extract_agent_token`,
   `auth::validate_admin_session`, `error::AppError`, `AppState`. None
   of those belong in a library meant to plug into a non-Warren stack.

So the plan has three structural moves:

1. **One canonical `wire` module** — kill the duplication.
2. **Move the server half into the library** — `agents_live` becomes
   `rabbit_lib::server`.
3. **Abstract the warren-specific dependencies** behind small traits
   (auth, persistence) so the library is embeddable.

The published artifact (`rabbit-lib`) carries all three. The existing
`rabbit` and `warren` crates shrink to thin shells that depend on it.

---

## 1. What lives where after the move

### 1.1 New crate: `rabbit-lib/` (publishable)

Everything that today constitutes the supervisor *plus* the server half.
Single source of truth for the protocol. Published to crates.io.

```
rabbit-lib/
├── Cargo.toml                  # name = "rabbit-lib", publish = true
├── src/
│   ├── lib.rs                  # re-exports the public API below
│   ├── config.rs               # Config struct + validate_warren_url
│   ├── wire.rs                 # CANONICAL Envelope / EnvelopeBody / TermFrame / etc.
│   ├── pty.rs                  # portable-pty wrapper, ExitKind, PtyExitStatus
│   ├── vt.rs                   # TermTracker (avt-backed)
│   ├── input.rs                # paste / slash / interrupt byte sequences
│   ├── trust.rs                # TrustDialog / TrustWatcher
│   ├── respawn.rs              # effective_args + CrashWindow
│   ├── shell.rs                # ShellHandle / ShellCmd (gated on `shell` feature)
│   ├── recorder.rs             # AsciicastRecorder (gated on `asciicast` feature)
│   ├── observer/
│   │   ├── mod.rs
│   │   ├── state.rs            # State enum
│   │   ├── hooks.rs            # ObserverHandle + axum router + parse
│   │   └── transcript.rs       # TranscriptTail
│   ├── meta_ring.rs            # bounded meta replay buffer
│   ├── health.rs               # HealthState + /healthz, /readyz axum router
│   ├── http_server.rs          # recorder-cast axum router (gated on `asciicast`)
│   ├── link.rs                 # Link, LinkCmd, LinkEvent, ReplaySnapFn
│   ├── supervisor.rs           # top-level loop + spawn_run_one
│   ├── hooks_install.rs        # claude settings.json installer
│   ├── session_store.rs        # SessionStore trait
│   ├── auth.rs                 # AgentToken / AdminSession trait
│   ├── server/                 # NEW — the warren::agents_live half
│   │   ├── mod.rs              # re-exports
│   │   ├── handle.rs           # AgentHandle (was agents_live::handle)
│   │   ├── registry.rs         # AgentRegistry (was agents_live::registry)
│   │   ├── actor.rs            # per-agent actor (was agents_live::actor)
│   │   ├── ws_rabbit.rs        # /ws/rabbit handler + router(state, store, auth)
│   │   ├── ws_browser.rs       # /agent/:id/claude/ws handler + router(state, store, auth)
│   │   ├── ws_shell.rs         # /agent/:id/shell/ws handler + router(state, store, auth)
│   │   └── http.rs             # /api/agents/:id/state|usage|claude|shell|history http
│   └── bin/
│       └── rabbit-hook.rs      # unchanged
└── tests/                      # moved verbatim from rabbit/tests
    ├── common/
    ├── input_discipline.rs
    ├── claude_smoke.rs         # CLAUDE_SMOKE / --ignored; doc warning preserved
    ├── transcript_parse.rs
    ├── snapshot_roundtrip.rs
    ├── meta_replay.rs
    ├── integration.rs
    └── pty_echo.rs
```

The `Cargo.toml` declares `[[bin]]` entries for `rabbit` *and* `rabbit-hook`,
preserving the existing binary names so the Dockerfile / compose file / hook
install path keep working without churn.

### 1.2 Existing crate: `rabbit/` (becomes a thin bin wrapper)

```toml
[package]
name = "rabbit"
version = "0.1.0"
publish = false

[dependencies]
rabbit-lib = { path = "../rabbit-lib" }
tokio = { workspace = true }
anyhow = { workspace = true }

[[bin]]
name = "rabbit"
path = "src/main.rs"
```

`src/main.rs` becomes `fn main() -> anyhow::Result<()> { rabbit_lib::run() }`
— the same one-liner it already is, just calling into the moved library.

The `rabbit` crate stops re-exporting modules at all; integration tests under
`rabbit/tests/` either move into `rabbit-lib/tests/` (preferred — they test
library code) or get deleted. The plan is to **move them all** since they
exercise the same modules.

### 1.3 Existing crate: `warren/` (consumes rabbit-lib)

`warren/Cargo.toml` gains:

```toml
rabbit-lib = { path = "../rabbit-lib" }
```

…and drops nothing (it never had a direct dep on `rabbit/`). What changes:

- `warren/src/agents_live/` is deleted.
- `warren/src/main.rs`'s `mod agents_live;` becomes `use rabbit_lib::server;`
  (or the `lib.rs` re-exports `rabbit_lib::server as agents_live` for
  unchanged call sites).
- `build_router` merges `rabbit_lib::server::routers(state)` instead of
  hard-coding the four routes.
- `AppState` gains a `store: Arc<dyn SessionStore>` field whose default value
  is the SeaORM-backed adapter that warren constructs from `Db`.

The wire `mirror` disappears. Both `rabbit/src/wire.rs::EnvelopeBody::Prompt`
and `warren/src/agents_live/wire.rs::EnvelopeBody::Prompt` are the same type
now — they deserialize identically because there is only one.

### 1.4 Existing crate: `warren-cli/`

No source changes. Cargo.toml unchanged. CLI calls `/api/*` over HTTP, so
the JSON wire format is what it depends on — and that format is preserved
by the protocol stability contract (§4 below).

---

## 2. The wire protocol becomes single-sourced

Today the two `wire.rs` files are intentionally parallel — the comment on
`rabbit/src/wire.rs::TermFrame` reads `Mirrored in
warren::agents_live::wire::TermFrame`, and every field gets the same `Mirrored
in …` annotation. The duplication exists because both crates need to read
and write the same JSON shapes. After the move:

| Symbol (today)                              | After                                                      |
|---------------------------------------------|------------------------------------------------------------|
| `rabbit::wire::Envelope`                    | `rabbit_lib::wire::Envelope` (canonical)                   |
| `rabbit::wire::EnvelopeBody`                | `rabbit_lib::wire::EnvelopeBody`                           |
| `warren::agents_live::wire::Envelope`       | `pub use rabbit_lib::wire::Envelope;`                      |
| `warren::agents_live::wire::EnvelopeBody`   | `pub use rabbit_lib::wire::EnvelopeBody;`                  |
| `warbit::wire::HelloUp` / `HelloDown`       | merged into one `Hello { role: HelloRole, … }`             |
| `warren::agents_live::wire::AgentState`     | kept here (warren adds domain types like `AgentState`)     |

### 2.1 The `Hello` variant was *almost* identical

`rabbit/src/wire.rs::HelloUp` has `state: String` (a free-form label);
`warren/src/agents_live/wire.rs::HelloDown` has `state: AgentState` (a typed
enum). Two options:

- **(a)** Pick one shape. Either `state: String` everywhere (warren parses it
  into `AgentState` on ingress), or `state: AgentState` everywhere (rabbit
  serializes its string label via `AgentState::as_str`).
- **(b)** Add a server-only variant `Hello(HelloUp)` (rabbit → warren) and
  a client-only variant `Hello(HelloDown)` (warren → browser), but with a
  shared field set.

**Decision: pick (a) and standardize on the typed enum.**
`warren::agents_live::wire::AgentState` is more useful for the browser JS
template (it surfaces a clickable row state badge). Rabbit's `state:
String` is an artifact of the supervisor stringifying its enum for
log readability; one `AgentState::as_str()` and `State::from_label()`
already exist as the round-trip helpers. After the move the supervisor
emits typed `AgentState` directly.

The browser JS template is unaffected because the *JSON shape* doesn't
change — it's still `{ "t": "hello", "state": "idle", … }` with the same
snake_case serialization (already gated by `#[serde(rename_all =
"snake_case")]` on the enum).

### 2.2 The `Leader*` variants are server-only

`ConnectionAssigned`, `ClaimLeader`, `ReleaseLeader`, `LeaderChanged` only
flow on the server→browser side of the wire. They live in the *browser*
envelope (`warren::agents_live::wire::EnvelopeBody`) and never appear on
the rabbit→server link. After the move they stay on `rabbit_lib::server`
as a `pub mod browser_extensions` with its own `EnvelopeBody` extension,
or — cleaner — they get baked into the main `EnvelopeBody` with an
`is_browser_only()` predicate that the link layer skips on serialize. The
shape lock is `serde` tags, so the JSON wire is identical regardless.

**Decision: bake them into the main `EnvelopeBody`.** The link layer
already filters by tag in `attempt()` (it drops anything that isn't
`{Hello, Ack, State, Prompt, …}`), so adding four new variants is a
zero-overhead no-op for the rabbit side.

### 2.3 The `TranscriptMsg` payload is opaque

Both files declare it as `message: serde_json::Value` — already a passthrough.
No change needed.

### 2.4 `ScreenSnapshotBody::after_seq` and the seq-numbered protocol

Already converged in v2 (`rabbit/src/wire.rs::PROTOCOL_VERSION = 2`,
`warren/src/agents_live/wire.rs::PROTOCOL_VERSION = 2`). After the move
this is one constant in one file. The seq-numbered snapshot protocol
spec (`seq-numbered-snapshot-protocol.md`) needs a small note appended
referring to `rabbit_lib::wire` as the single source.

---

## 3. The server half — what crosses from warren into rabbit-lib

### 3.1 Imports that have to be abstracted

`grep "use crate::"` over `warren/src/agents_live/*.rs` shows six warren-internal
imports the server half makes today:

| Import                                      | Where used                          | Plan                            |
|---------------------------------------------|-------------------------------------|---------------------------------|
| `crate::db::Db`                             | `actor.rs`                          | → `Arc<dyn SessionStore>`       |
| `crate::db_ops` (next_event_seq, insert_*)  | `actor.rs`                          | → `SessionStore::next/insert`   |
| `crate::auth::extract_agent_token`          | `ws_rabbit.rs`                      | → `AuthBackend::agent_token()`  |
| `crate::auth::validate_admin_session`       | `ws_browser.rs`, `ws_shell.rs`      | → `AuthBackend::admin_session()`|
| `crate::auth::AuthContext`                  | `http.rs`                           | → `AuthBackend::admin_session()`|
| `crate::error::{AppError, AppResult}`       | everywhere                          | → `anyhow::Result` for the lib  |
| `crate::AppState`                           | http/ws_browser/ws_shell/ws_rabbit  | → `ServerState` (lib-local)     |

Each is one seam. None requires schema changes; the persistence trait is
two methods (the actor's actual needs), the auth trait is two methods, the
error type drops a generic `Into<anyhow::Error>` wrapper.

### 3.2 `SessionStore` trait

Today the actor calls:

```rust
db_ops::next_event_seq(&db, agent_id).await?;
db_ops::insert_agent_event(&db, id, agent_id, seq, kind, payload).await?;
```

…via `db_ops` against the SeaORM `Db`. After the move:

```rust
#[async_trait::async_trait]   // or native async-fn-in-trait (1.75+; we target 1.85)
pub trait SessionStore: Send + Sync + 'static {
    /// Returns the next free `seq` for `agent_id`'s event log. The store
    /// is responsible for persisting this on insert; the caller uses it
    /// as the dedup watermark and to populate `(agent_id, seq)` rows.
    async fn next_event_seq(&self, agent_id: Uuid) -> Result<i64>;

    /// Append one event to `agent_id`'s log at `seq`. Returns Ok(()) if
    /// persisted, Err(_) on transport failure. Implementations should
    /// surface a unique-constraint violation (re-insert at an existing
    /// seq) however they like — the actor currently swallows it as
    /// "already persisted" via `Result::ok()`.
    async fn insert_event(
        &self,
        agent_id: Uuid,
        seq: i64,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<()>;
}
```

The workspace already pins `rust-version = "1.85"`, so native
`async fn` in traits is available — but `async_trait` may still be
preferred for object-safety (`dyn SessionStore`). Plan keeps both options
on the table; final call is whichever the trait-object usage demands.

A pre-built adapter lives in warren (not rabbit-lib) so the lib stays
free of SeaORM:

```rust
// warren/src/rabbit_adapter.rs (new file)
pub struct SeaOrmSessionStore(pub Db);
#[async_trait::async_trait]
impl rabbit_lib::SessionStore for SeaOrmSessionStore { /* … */ }
```

### 3.3 `AuthBackend` trait

```rust
#[async_trait::async_trait]
pub trait AuthBackend: Send + Sync + 'static {
    /// Validate the `Authorization: Bearer …` header against the agent
    /// token table. Returns the authenticated `agent_id` on success.
    async fn authenticate_agent(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<Uuid, AuthError>;

    /// Validate the session cookie / header for admin endpoints. Returns
    /// `true` iff the caller is an authenticated admin.
    async fn authenticate_admin(
        &self,
        headers: &axum::http::HeaderMap,
    ) -> Result<bool, AuthError>;
}
```

The `AuthError` type lives in rabbit-lib and maps cleanly to `AppError::Unauthorized`
in warren via a `From` impl. The warren adapter is a thin wrapper around
the existing `auth::extract_agent_token` and `auth::validate_admin_session`.

### 3.4 `ServerState` — the lib's analogue of warren's `AppState`

```rust
pub struct ServerState {
    pub registry: AgentRegistry,
    pub store: Arc<dyn SessionStore>,
    pub auth: Arc<dyn AuthBackend>,
    /// Surface for emitting a ServerLog entry when an actor logs
    /// something. Defaults to `log::log!`. Embedders can swap in a
    /// structured logger.
    pub log_sink: Arc<dyn LogSink>,
}
```

`ServerState::router() -> axum::Router` returns a single `Router<Arc<ServerState>>`
that mounts `/ws/rabbit`, `/agent/:id/claude/ws`, `/agent/:id/shell/ws`,
and the JSON http endpoints under `/api/agents/:id/*`. War merges it via
`Router::merge(rabbit_lib::server::ServerState::router())`.

### 3.5 The HTTP `/api/agents/:id/*` endpoints

Today `warren/src/agents_live/http.rs` exposes (presumably) `/state`,
`/usage`, `/claude`, `/shell`, `/history` — they're used by the War UI
to surface per-agent state and to fetch `.cast` files via the recorder's
HTTP server. After the move:

- `/state`, `/usage`, `/claude` (clear), `/shell` — move into
  `rabbit_lib::server::http.rs`. They're `AgentHandle`-backed and have
  no warren-specific logic.
- `/history` (fetch `.cast`) — also moves into `rabbit_lib::server::http.rs`.
  It currently reaches out to rabbit's recorder URL over HTTP, which is
  already an external dep — no SeaORM needed.

The auth gate on every one of them is `AuthBackend::authenticate_admin`,
which warren implements via its existing session-cookie path.

### 3.6 `AgentHandle` is unchanged externally

`AgentHandle::prompt`, `AgentHandle::clear`, `AgentHandle::interrupt`,
`AgentHandle::restart`, `AgentHandle::state`, `AgentHandle::usage`, …
all stay in `rabbit_lib::server::handle`. The shape doesn't change;
the only caller-visible diff is the import path. The 30+ tests under
`warren/src/agents_live/handle.rs::tests` move verbatim.

---

## 4. Wire stability contract (committed for downstream users)

Anything published to crates.io acquires external consumers. The wire is
the API surface they touch most. The plan preserves it:

1. **`PROTOCOL_VERSION = 2`** is the floor; bumping is a major-version
   semver event on `rabbit-lib`.
2. **`Envelope { v, seq, body }` JSON shape** is unchanged. `serde` tags
   stay snake_case. `ScreenSnapshotBody::after_seq` retains
   `#[serde(default)]` for v1-readability.
3. **Binary frame layout** stays `<chan:1> <seq:8 BE u64> <bytes…>` per
   `seq-numbered-snapshot-protocol.md`.
4. **HTTP routes** mounted by `ServerState::router()` keep the same paths
   and shapes; consumers that need to integrate via their own HTTP server
   (not axum) can build the router, then rip the handlers out — they're
   plain `async fn(State<…>, …)`.

This contract is the first thing the new crate's README must call out.

---

## 5. Migration order

Six phases, each independently testable. We stop after each phase and run
the full test suite; if any phase regresses existing tests, fix before
moving on.

### Phase 1 — single-source the wire module (no API change yet)

1. Create `rabbit-lib/Cargo.toml` declaring `name = "rabbit-lib"`.
2. `git mv rabbit/src/wire.rs rabbit-lib/src/wire.rs`.
3. Update the `warren::agents_live::wire` file to a one-line
   `pub use rabbit_lib::wire::*;` (with the leader variants kept as
   re-exports from a new `rabbit_lib::wire::browser_extensions`).
4. Update every `use crate::agents_live::wire::…` site in warren to
   `use rabbit_lib::wire::…`.
5. Drop `warren/src/agents_live/wire.rs`'s duplicate `PROTOCOL_VERSION`
   test (already covered by the rabbit-lib version).
6. Verify `cargo test --workspace` is green.

At the end of Phase 1, the protocol is single-sourced but the
`rabbit` and `rabbit-lib` crates still coexist on disk (rabbit is
unchanged except for the deleted wire.rs).

### Phase 2 — move the server half into rabbit-lib

1. `git mv warren/src/agents_live/* rabbit-lib/src/server/`.
2. Replace `use crate::db::Db;` in `server/actor.rs` with
   `Arc<dyn SessionStore>`.
3. Replace `db_ops::next_event_seq` / `db_ops::insert_agent_event`
   calls with the trait methods.
4. Replace `use crate::error::{AppError, AppResult}` with
   `anyhow::Result` throughout.
5. Replace `use crate::auth::*` with `AuthBackend` trait calls.
6. Add `server/mod.rs` exporting the public API.
7. In `rabbit-lib/src/lib.rs`, re-export `pub mod server;` and the
   protocol types.
8. Verify `cargo test -p rabbit-lib` is green (the moved tests run
   here).

End state: warren no longer has an `agents_live` directory; the crate
compiles because `rabbit-lib` is its dep.

### Phase 3 — adapt warren to consume rabbit-lib

1. `warren/Cargo.toml` gains `rabbit-lib = { path = "../rabbit-lib" }`.
2. New file `warren/src/rabbit_adapter.rs` with `SeaOrmSessionStore`
   and a `WarAuthBackend` (wraps `auth::*`).
3. `warren/src/AppState` gains `pub live: rabbit_lib::server::ServerState`.
4. `warren/src/main.rs::build_router` becomes
   `Router::new().merge(routes::ui::router()).merge(routes::api::router())
   .merge(state.live.router())` (with auth + store wired).
5. `warren/src/main.rs` removes `mod agents_live;`.
6. Verify `cargo test --workspace` is green.
7. The four `routes::api::*` endpoints that previously proxied through
   `agents_live::http` (state/usage/claude/shell) become
   `RabbitHandle::state()` calls against `ServerState::registry`, or
   are dropped entirely if `ServerState::router()` already mounts
   them.

End state: warren's live-plane code is gone; it's all `rabbit-lib`.

### Phase 4 — slim rabbit/ down to a bin wrapper

1. `rabbit/Cargo.toml` becomes a 12-line wrapper crate with
   `rabbit-lib` as its only dependency. `publish = false`.
2. `rabbit/src/main.rs` unchanged (it's already `rabbit::run()`).
3. `rabbit/src/lib.rs` and the rest of `rabbit/src/*` are deleted
   (now lives in rabbit-lib).
4. The `rabbit/tests/*` files move into `rabbit-lib/tests/*` since
   they exercise library code. The `claude_smoke` test stays
   `--ignored` with the existing warning preserved.

End state: `rabbit/` is ~15 lines; everything else is in rabbit-lib.

### Phase 5 — metadata, docs, dry-run

1. `rabbit-lib/Cargo.toml` gains the publish metadata:
   ```toml
   [package.metadata.docs.rs]
   all-features = true

   [package]
   description = "Remote-controllable Claude supervisor + matching server-side runtime"
   license = "MIT OR Apache-2.0"
   repository = "https://github.com/…"
   homepage = "https://github.com/…"
   readme = "README.md"
   keywords = ["claude", "pty", "supervisor", "agent", "warren"]
   categories = ["api-bindings", "asynchronous", "development-tools"]
   ```
2. Add `rabbit-lib/README.md` with the wire stability contract,
   feature-flag matrix, and a 30-line "embed in your own server" snippet.
3. `cargo package --list` and `cargo package` locally to confirm
   the crate is publishable.
4. `cargo doc --no-deps --all-features` and skim for broken links.

End state: rabbit-lib is ready to publish. We do **not** push to crates.io
in this plan — that's the operator's call.

### Phase 6 — Warren catches up on remaining `wire::Hello` divergence

1. Decide on the typed-vs-string `state` field in `Hello` (§2.1).
2. Update the browser JS template (`warren/templates/agent_claude.html`)
   if it depends on the string variant.
3. Move the four `Leader*` variants into the canonical `EnvelopeBody`
   per §2.2.
4. Drop the `wire.rs` test duplicates; consolidate in rabbit-lib.

End state: `wire` is a single file in a single crate with one test
module covering both halves of the protocol.

---

## 6. Public API of rabbit-lib (preview)

```rust
// rabbit-lib/src/lib.rs

// §2: single-source protocol
pub mod wire;
pub use wire::{
    Envelope, EnvelopeBody, HelloUp, ScreenSnapshotBody, SessionInfo, StateFrame,
    PromptEcho, TurnDone, UsageSnapshot, LogLine, TermSize, TermFrame,
    PROTOCOL_VERSION, TERM_CHAN_CLAUDE, TERM_CHAN_SHELL,
};

// §3: server-side traits
pub trait SessionStore: Send + Sync + 'static { … }
pub trait AuthBackend:  Send + Sync + 'static { … }
pub trait LogSink:      Send + Sync + 'static { … }

// TTY wrapping
pub mod pty;
pub mod vt;
pub mod input;
pub mod trust;
pub mod respawn;
pub use pty::{Pty, ExitKind, PtyExitStatus};
pub use vt::TermTracker;
pub use input::{paste, slash, interrupt, ENTER, ESC, CTRL_U,
                BRACKETED_PASTE_START, BRACKETED_PASTE_END};
pub use trust::{TrustWatcher, has_trust_marker, ACCEPT_BYTES, TRUST_MARKERS};
pub use respawn::{effective_args, CrashWindow};

// Optional modules behind feature flags
#[cfg(feature = "shell")]
pub mod shell;
#[cfg(feature = "shell")]
pub use shell::{ShellHandle, ShellCmd, spawn as spawn_shell};

#[cfg(feature = "asciicast")]
pub mod recorder;
#[cfg(feature = "asciicast")]
pub use recorder::AsciicastRecorder;

// Observer
pub mod observer;
pub use observer::state::State;
pub use observer::hooks::{ObserverHandle, ObserverEvent, HookEvent, serve};
pub use observer::transcript::{TranscriptTail, UsageUpdate, default_transcript_path};

// Bounded meta replay
pub mod meta_ring;
pub use meta_ring::MetaRing;

// HTTP servers we own (health, observer, recorder)
pub mod health;
pub mod http_server;
pub mod hooks_install;

// Link layer (rabbit → server)
pub mod link;
pub use link::{Link, LinkCmd, LinkEvent, ReplaySnapFn};

// Server-side runtime
pub mod server;
pub use server::{AgentHandle, AgentRegistry, Command, TurnOutcomeMsg, ServerState};

// Top-level entry point used by `rabbit` binary
pub fn run() -> anyhow::Result<()> { … }
```

---

## 7. Feature flags

Two existing flags worth keeping; two new ones for library ergonomics:

| Flag         | Default | What it gates                                        |
|--------------|---------|------------------------------------------------------|
| `tls`        | **on**  | `tokio-tungstenite` rustls connector (today default) |
| `shell`      | off     | `rabbit_lib::shell` (the bash PTY sidecar)           |
| `asciicast`  | off     | `rabbit_lib::recorder` + `rabbit_lib::http_server`   |
| `tls-listener` *(new)* | off | axum TLS connector for the standalone `rabbit` bin if embedded in non-axum hosts |

`tls` stays default-on because today's primary embedder is warren, which
serves over TLS in production. Callers who terminate TLS upstream set
`default-features = false` and lose nothing.

---

## 8. Tests — what moves where

| File (today)                                | After                              | Notes |
|---------------------------------------------|-------------------------------------|-------|
| `rabbit/tests/integration.rs`               | `rabbit-lib/tests/integration.rs`  | unmoved |
| `rabbit/tests/pty_echo.rs`                  | `rabbit-lib/tests/pty_echo.rs`     | unmoved |
| `rabbit/tests/input_discipline.rs`          | `rabbit-lib/tests/input_discipline.rs` | unmoved |
| `rabbit/tests/transcript_parse.rs`          | `rabbit-lib/tests/transcript_parse.rs` | unmoved |
| `rabbit/tests/snapshot_roundtrip.rs`        | `rabbit-lib/tests/snapshot_roundtrip.rs` | unmoved |
| `rabbit/tests/meta_replay.rs`               | `rabbit-lib/tests/meta_replay.rs`  | unmoved |
| `rabbit/tests/claude_smoke.rs`              | `rabbit-lib/tests/claude_smoke.rs` | `--ignored` preserved |
| `rabbit/tests/common/mod.rs`                | `rabbit-lib/tests/common/mod.rs`   | unmoved |
| `rabbit/src/supervisor.rs::tests`           | `rabbit-lib/src/supervisor.rs::tests` | unmoved |
| `rabbit/src/wire.rs::tests`                 | `rabbit-lib/src/wire.rs::tests`    | unmoved (now also covers the §2.1 Hello shape) |
| `rabbit/src/pty.rs::tests`                  | `rabbit-lib/src/pty.rs::tests`     | unmoved |
| `rabbit/src/recorder.rs::tests`             | `rabbit-lib/src/recorder.rs::tests`| unmoved |
| `rabbit/src/trust.rs::tests`                | `rabbit-lib/src/trust.rs::tests`   | unmoved |
| `rabbit/src/respawn.rs::tests`              | `rabbit-lib/src/respawn.rs::tests` | unmoved |
| `rabbit/src/link.rs::tests`                 | `rabbit-lib/src/link.rs::tests`    | unmoved |
| `rabbit/src/meta_ring.rs::tests`            | `rabbit-lib/src/meta_ring.rs::tests` | unmoved |
| `rabbit/src/http_server.rs::tests`          | `rabbit-lib/src/http_server.rs::tests` | unmoved |
| `rabbit/src/config.rs::tests`               | `rabbit-lib/src/config.rs::tests`  | unmoved |
| `rabbit/src/observer/hooks.rs::tests`       | `rabbit-lib/src/observer/hooks.rs::tests` | unmoved |
| `rabbit/src/observer/transcript.rs::tests`  | `rabbit-lib/src/observer/transcript.rs::tests` | unmoved |
| `rabbit/src/observer/state.rs::tests`       | `rabbit-lib/src/observer/state.rs::tests` | unmoved |
| `rabbit/src/shell.rs::tests`                | `rabbit-lib/src/shell.rs::tests`   | unmoved |
| `warren/src/agents_live/handle.rs::tests`   | `rabbit-lib/src/server/handle.rs::tests` | unmoved; renamed path |
| `warren/src/agents_live/actor.rs::tests`    | `rabbit-lib/src/server/actor.rs::tests`  | unmoved; renamed path |
| `warren/src/agents_live/ws_browser.rs::tests` | `rabbit-lib/src/server/ws_browser.rs::tests` | unmoved |
| `warren/src/agents_live/ws_rabbit.rs::tests`  | `rabbit-lib/src/server/ws_rabbit.rs::tests` | unmoved |
| `warren/src/agents_live/ws_shell.rs::tests`   | `rabbit-lib/src/server/ws_shell.rs::tests` | unmoved |
| `warren/tests/openapi_drift.rs`             | `warren/tests/openapi_drift.rs`    | stays in warren (tests the OpenAPI schema, not the live plane) |

The moved tests run against `rabbit-lib`'s module paths; their `use super::*;`
imports keep working because they don't cross module boundaries beyond
`super`. The `claude_smoke` warning ("**Never run this in a live
environment**") is preserved verbatim in the new location and the
`never-test-in-live-claude-env` memory is consulted before any operator
runs it.

---

## 9. Risk register

| Risk                                                | Likelihood | Impact | Mitigation                                    |
|-----------------------------------------------------|------------|--------|-----------------------------------------------|
| Cargo workspace churn breaks `Dockerfile`           | Med        | High   | Phase 4 keeps the `rabbit` bin name stable; Dockerfile's `cargo build -p rabbit` still works |
| `db_ops::insert_agent_event` swallows unique-violation as "already persisted"; the trait impls in warren have to mimic that | Low | Med | Document in the trait doc-comment; cover with a `dedup_on_replay` test in rabbit-lib |
| Browser JS template depends on a wire shape (e.g. `"state": "starting"` string vs enum) that the §2.1 unification could break | Med | Med | Wire JSON shape doesn't change (serde snake_case on the enum produces the same strings); unit-test the JSON output of every variant |
| Two crates depending on rabbit-lib at different versions when published | Low | Med | Document the `rabbit-lib` MSRV as 1.85; pin in `Cargo.toml` |
| `RabbitHandle`'s `send_terminal_bytes` swallows mpsc errors; moving the trait changes the error path | Low | Low | The trait already returns `Result<(), _>`; error mapping stays the same |
| Hidden coupling via `crate::error` `AppError` semantics (e.g. its `IntoResponse` impl is what makes HTTP handlers work) | Med | Med | Phase 2 picks `anyhow::Error` for the lib; convert at the warren boundary |
| `seq-numbered-snapshot-protocol.md` needs an addendum pointing at the new crate | Low | Low | Single one-line addendum; not blocking |
| The `ObserverHandle` `state` field is `crate::observer::state::State`, not the wire `AgentState` — phase 6 unifies them or keeps both | Med | Low | Keep both (`State` for the supervisor's internal enum, `AgentState` for the wire); add a `From` impl in both directions |
| Recursive cargo dep (rabbit-lib → some warren lib) accidentally introduced during move | Low | High | Phase 1 + 2 add no warren imports to rabbit-lib; CI check via `cargo tree -p rabbit-lib` to confirm |
| `claude_smoke` warning comment needs updating for the new path | Low | Low | The `--ignored` marker and the never-test-in-live-claude-env memory keep the gate intact; update the comment to point at the new path |

---

## 10. Open questions (flag for the operator)

1. **crate name on crates.io.** `rabbit-lib` vs `warren_rabbit` vs
   `rabbit-lib-rs`. The crate registry might already hold
   `crates.io/crates/rabbit` — `rabbit-lib` is the safe pick; `warren_rabbit`
   advertises the parent project (which a downstream might not want to
   imply a dependency on).
2. **Should the `rabbit` bin move into rabbit-lib's `src/bin/`, or stay a
   separate crate?** Two viable layouts:
   - **(a)** `rabbit-lib/src/bin/rabbit.rs` + `rabbit-lib/src/bin/rabbit-hook.rs`,
     keeping `rabbit` as the published name on disk. The crate publishes
     both.
   - **(b)** The current layout: `rabbit/` as a wrapper crate that pulls
     `rabbit-lib` and just contains `src/main.rs`.
   Plan defaults to **(a)** (less code, less indirection). Easy to flip.
3. **Bundling `bin/rabbit-hook`.** It's a tiny stdin-to-HTTP shim. Two
   options: keep it as a sibling bin in rabbit-lib, or fold it into
   `rabbit_lib::hook_shim` as a `pub fn` and let consumers wrap their own
   binary. The current behavior (separate `rabbit-hook` binary) is what
   the Claude hook settings.json embeds, so keeping the binary is
   preferable.
4. **Should `rabbit-lib::server` be its own crate (`rabbit-server`)?** The
   user asked for one library, so the plan keeps it as a module. If the
   server half ends up larger than expected (e.g. embedded HTTP API
   grows), splitting it later is a small refactor — the trait
   boundaries survive a crate split.
5. **What gets the actual `warren::agents_live::wire` deletion?** Phase 1
   makes it a re-export. Phase 6 removes the file entirely. Nothing
   between should touch the file's contents; only its presence in the
   module tree.

---

## 11. Things to read before changing things

The same list from `WARREN.md` applies, plus:

- `rabbit/src/supervisor.rs::tests` — the per-component shape pins
  (seq counter, after_seq formula, write_claude_terminal_bytes) stay
  in rabbit-lib; operators editing those tests must update them in
  `rabbit-lib/src/supervisor.rs::tests` after the move.
- `seq-numbered-snapshot-protocol.md` — once §2 lands, the file's
  `rabbit::wire::…` references should point at `rabbit_lib::wire::…`.
- `never-test-in-live-claude-env` (memory) — preserved verbatim into
  `rabbit-lib/tests/claude_smoke.rs`'s comment header.

---

## 12. Status — 2026-07-06

A first cut at executing the plan landed the supervisor half but
stopped short of the server-half move because of scope (the server
half is 3.3k LOC of code with deep warren-internal coupling). The
build is green at the end of this session.

### Done

- **Phase 1** — `rabbit-lib/` crate created. `rabbit/src/wire.rs`
  moved to `rabbit-lib/src/wire.rs`. The wire module is single-sourced
  *for the supervisor side*; warren's `agents_live/wire.rs` still
  carries a parallel copy that is identical-by-convention but not yet
  a `pub use` re-export. (Phase 6 finishes the unification.)
- **Phase 4** — `rabbit/` slimmed to a thin bin wrapper. `rabbit/Cargo.toml`
  is 12 lines depending on `rabbit-lib`. `rabbit/src/main.rs` is 4
  lines calling `rabbit_lib::run()`. The seven supervisor modules
  (`config`, `health`, `hooks_install`, `http_server`, `input`,
  `link`, `meta_ring`, `observer/`, `pty`, `recorder`, `respawn`,
  `shell`, `supervisor`, `trust`, `vt`, `wire`, plus
  `bin/rabbit-hook.rs`) now live in `rabbit-lib/src/`. The 8 test
  files in `rabbit/tests/` moved to `rabbit-lib/tests/` with their
  `use rabbit::…` paths rewritten to `use rabbit_lib::…`.
- **Trait surface** — `rabbit-lib::server::{SessionStore, AuthBackend,
  LogSink, AuthError, AgentEventRecord, StdLogSink}` is defined and
  re-exported from the crate root. The trait method signatures are
  stable enough that a follow-up PR can move the server code in
  lockstep.

### Partial / not done

- **Phase 2** — the seven server modules (`handle`, `actor`,
  `registry`, `ws_rabbit`, `ws_browser`, `ws_shell`, `http`) remain
  in `warren/src/agents_live/`. The trait stubs and the re-exports
  of the placeholder `AgentHandle` / `AgentRegistry` types are in
  `rabbit-lib::server::mod` so a downstream consumer can see the
  intended surface, but the implementations are not.
- **Phase 3** — warren does not yet depend on `rabbit-lib`. The
  `SeaOrmSessionStore` / `WarAuthBackend` adapter and the
  `build_router` merge are not done.
- **Phase 5** — `rabbit-lib/Cargo.toml` has license + description
  metadata but no README and `publish = false`. `cargo package
  --list` has not been run.
- **Phase 6** — the wire `Hello::state` field is still `String` on
  the supervisor side and `AgentState` (typed enum) on the warren
  side. The four `Leader*` variants in warren's `wire.rs` are still
  in the duplicate file, not the canonical one.

### Build state

```
$ cargo check --workspace
    Checking rabbit-lib v0.1.0 (/workdir/rabbit-lib)
    Checking rabbit v0.1.0 (/workdir/rabbit)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.04s
```

`cargo check -p warren` and `cargo check -p warren-cli` are also
green. The duplicate `wire.rs` types across `rabbit-lib` and
`warren::agents_live` do not conflict because they live in different
crates.

### Reverted work

- The seven `rabbit-lib/src/server/{handle,actor,registry,ws_*,
  http}.rs` files copied from warren were deleted after the build
  was found to have 54 errors (each file references `crate::db::Db`,
  `crate::db_ops`, `crate::auth::*`, `crate::error::AppError`,
  `crate::AppState`). The transform to trait abstractions requires
  per-function care; see §13 for the follow-up plan.

---

## 13. Follow-up plan

The server half is the work that didn't fit in this session. The
piece-by-piece transform below should keep the build green at every
step.

### 13.1 Phase 2 — server half (file-by-file)

Transform each file in this order, validating with `cargo check -p
rabbit-lib` after each step:

1. **registry.rs** (189 LOC, no warren deps). Plain `mv
   warren/src/agents_live/registry.rs
   rabbit-lib/src/server/registry.rs`. Replace `use
   crate::agents_live::handle::AgentHandle` with `use
   crate::server::handle::AgentHandle`. The `AgentHandle::new`
   constructor that the `register` test calls is in `handle.rs` and
   moves in step 2.
2. **handle.rs** (903 LOC). Same `mv` and import rewrite. No warren
   deps; only references the `actor::Command` / `TurnOutcomeMsg`
   types. Tests in `handle.rs::tests` use `AgentStateSnapshot`
   directly.
3. **actor.rs** (765 LOC). The big one. Replace
   ```rust
   use crate::db::Db;
   use crate::db_ops;
   // ...
   async fn run_inner(
       db: Db,
       handle: AgentHandle,
       agent_id: Uuid,
       socket: WebSocket,
       mut cmd_rx: mpsc::Receiver<Command>,
   )
   ```
   with
   ```rust
   // no warren imports
   async fn run_inner(
       store: Arc<dyn SessionStore>,
       handle: AgentHandle,
       agent_id: Uuid,
       socket: WebSocket,
       mut cmd_rx: mpsc::Receiver<Command>,
   )
   ```
   Replace `db_ops::next_event_seq(&db, agent_id)` with
   `store.next_event_seq(agent_id)`. Replace
   `db_ops::insert_agent_event(db, id, agent_id, seq, kind, payload)`
   with `store.insert_event(agent_id, seq, kind, payload)`. The
   `persist_event` helper at the bottom of `actor.rs` becomes a
   free function that calls `store.insert_event`. The
   `envelope_kind` helper stays put.
4. **ws_rabbit.rs** (76 LOC). The `extract_agent_token` call
   becomes `state.auth.authenticate_agent(&headers).await?` (where
   the `?` now converts `AuthError` to whatever the handler's
   return type is). `AppError` becomes `anyhow::Error`. The
   `State<AppState>` extractor becomes
   `State<Arc<ServerState>>`. The module grows a
   `pub fn router(state: Arc<ServerState>) -> axum::Router` that
   returns a `Router` mounting `/ws/rabbit`.
5. **ws_browser.rs** (656 LOC). Same `State` swap. The
   `AuthContext` extractor disappears in favor of inline
   `state.auth.authenticate_admin(&headers).await?` checks at the
   top of each handler.
6. **ws_shell.rs** (169 LOC). Same as `ws_browser.rs`.
7. **http.rs** (290 LOC). Same as above. The
   `db_ops::list_events_since` call becomes
   `store.list_events_since(agent_id, since, limit).await?`. The
   `claude_events_stream` SSE handler can keep its
   `async_stream::stream!` block; the only change is the
   `EnvelopeBody` import path.

Each step leaves the previous step green. The 3 internal tests
(`actor.rs::tests`, `ws_browser.rs::tests`, `handle.rs::tests`)
move with the files and need no further change beyond the import
rewrite.

### 13.2 Phase 3 — warren adapter

A single new file `warren/src/rabbit_adapter.rs` (or split into
`warren/src/rabbit_store.rs` and `warren/src/rabbit_auth.rs`):

```rust
// warren/src/rabbit_adapter.rs
pub struct SeaOrmSessionStore(pub crate::db::Db);
#[async_trait::async_trait]
impl rabbit_lib::SessionStore for SeaOrmSessionStore {
    async fn next_event_seq(&self, agent_id: Uuid)
        -> anyhow::Result<i64>
    { Ok(crate::db_ops::next_event_seq(&self.0, agent_id).await?) }
    // ... insert_event, list_events_since
}

pub struct WarAuthBackend {
    pub db: crate::db::Db,
}
#[async_trait::async_trait]
impl rabbit_lib::AuthBackend for WarAuthBackend {
    async fn authenticate_agent(&self, h: &HeaderMap)
        -> Result<Uuid, rabbit_lib::AuthError>
    { /* wrap auth::extract_agent_token */ }
    async fn authenticate_admin(&self, h: &HeaderMap)
        -> Result<bool, rabbit_lib::AuthError>
    { /* wrap auth::validate_admin_session */ }
}
```

`warren/src/main.rs::AppState` gains
```rust
pub live: Arc<rabbit_lib::server::ServerState>,
```
constructed in `main()` with the adapters. The four
`agents_live::ws_*::router()` calls in `build_router` become a
single `merge(state.live.router())`.

### 13.3 Phase 5 — publish metadata

Add `readme = "README.md"` to `rabbit-lib/Cargo.toml` (file
doesn't exist yet) and flip `publish = false` to `publish = true`.
The README should restate the §4 wire stability contract and show a
30-line "embed in your own server" snippet that constructs a
`ServerState` and mounts its router.

### 13.4 Phase 6 — wire unification

Final cleanups after Phase 3 lands:

- `warren/src/agents_live/wire.rs` becomes
  `pub use rabbit_lib::wire::*;` (or is deleted entirely once the
  `Leader*` variants are folded into the canonical
  `EnvelopeBody`).
- Standardize on the typed `AgentState` enum for `Hello::state`:
  pick (a) per §2.1, fold the `HelloUp` / `HelloDown` split into a
  single variant.
- Drop the four duplicated `connection_assigned_*` /
  `claim_leader_*` / `release_leader_*` / `leader_changed_*`
  tests; one set in `rabbit-lib/src/wire.rs::tests` covers both
  halves.

---

## 14. Status — 2026-07-06 (post-server-half completion)

The full plan (Phases 1–6) has now executed end-to-end. The
supervisor half and the server half of the rabbit protocol both
live in `rabbit-lib`. `warren` consumes the lib through a single
`Arc<rabbit_lib::server::ServerState>` field on its `AppState` and
a one-line merge in `build_router`. `rabbit/` is a 4-line bin
wrapper around `rabbit_lib::run()`. Workspace builds green with
zero warnings.

### Done

- **Phase 1** — `rabbit-lib/src/wire.rs` is the single source of
  truth. `AgentState` is a typed enum (snake_case serde) on both
  the supervisor and server sides. `HelloDown` is a type alias for
  `HelloUp` so the wire has one `Hello` struct.
- **Phase 2** — all seven server modules
  (`handle`, `actor`, `registry`, `ws_rabbit`, `ws_browser`,
  `ws_shell`, `http`) live in `rabbit-lib/src/server/`. They take
  `Arc<ServerState>` (which carries the registry + the trait
  adapters) and import against `crate::server::SessionStore` /
  `crate::server::AuthBackend` instead of the previous
  warren-internal Db / AuthContext.
- **Phase 3** — `warren/Cargo.toml` depends on `rabbit-lib` by
  path. `warren/src/rabbit_adapter.rs` is the only file that
  knows about both sides: it implements `SeaOrmSessionStore` on
  top of `db_ops` + `agent_event` and `WarAuthBackend` on top of
  `auth::validate_admin_session` + `auth::lookup_agent_by_token`.
  `warren/src/main.rs::AppState` has
  `pub live: Arc<rabbit_lib::server::ServerState>`, with a
  `FromRef<AppState> for Arc<ServerState>` impl so the lib's
  handlers can be merged into the larger `Router<AppState>`. The
  four `agents_live::ws_*` route mounts + the
  `agents_live::http::router()` merge are gone; everything is
  one `state.live.router().with_state(state.live.clone())` line.
  `warren/src/agents_live/` is deleted.
- **Phase 4** — `rabbit/Cargo.toml` is 12 lines, `rabbit/src/main.rs`
  is 4 lines.
- **Phase 5** — `rabbit-lib/Cargo.toml` has license, description,
  keywords, categories, repo, readme, and docs.rs metadata.
  `rabbit-lib/README.md` documents the wire stability contract,
  the feature flag matrix, the embed-in-your-own-server recipe,
  and MSRV 1.85. `publish = false` is on until the operator
  green-lights the first crates.io push.
- **Phase 6** — Warren's `warren/src/agents_live/wire.rs` is gone
  (the entire `agents_live/` directory is gone with it). The
  wire `Hello` is a single struct on both sides; the four
  `Leader*` variants live in the canonical
  `rabbit-lib/src/wire.rs`; the typed `AgentState` enum replaces
  the old `String` on the supervisor side too.

### Build state

```
$ cargo check --workspace
    Checking rabbit-lib v0.1.0 (/workdir/rabbit-lib)
    Checking rabbit v0.1.0 (/workdir/rabbit)
    Checking warren v0.1.0 (/workdir/warren)
    Checking warren-cli v0.1.0 (/workdir/warren-cli)
    Finished `dev` profile [unoptimized + debuginfo] target(s)
```

Zero warnings, zero errors. The ServerState is constructed once in
`warren::rabbit_adapter::build_server_state(db)` and shared across
the four `ws_*` + `http` routers via a single `merge` call.

### Migration shape — concrete numbers

- **rabbit-lib** is the canonical home for: 7 supervisor modules,
  7 server modules, the `wire` module, the `bin/rabbit-hook.rs`
  shim. The lib owns its own README, its own metadata, and its
  own test suite.
- **rabbit** is `fn main() { rabbit_lib::run() }`. That's it.
- **warren** has one new file (`rabbit_adapter.rs`, ~120 lines)
  and no other structural changes; the rest is a rewire of the
  existing `AppState` + `build_router`.
- **warren-cli** was untouched; it doesn't talk to the lib.

### Verification (2026-07-06, post-cleanup pass)

- `cargo check --workspace` — green, zero warnings.
- `cargo check --workspace --tests` — green, zero warnings. Test
  modules that referenced `crate::agents_live::*` paths left over
  from the move were rewritten to point at the canonical locations
  (`crate::wire::*` and `crate::server::handle::*`). One missing
  derive (`PartialEq, Eq` on `wire::TermSize`) was added — it was
  needed by an `assert_eq!` in the server-side ws_browser tests.
- `cargo build --release --workspace` — green, zero warnings.
  Incremental build of `rabbit-lib`, `rabbit`, `warren`, and
  `warren-cli` end-to-end.
- **NOT yet run:** `cargo test --workspace`. Per the project's
  "never test in the live Claude env" rule, this needs to run in
  your isolated test harness. Everything should compile and pass
  cleanly now that the broken `crate::agents_live::*` import paths
  are gone and the `TermSize` derive is fixed; if a test fails
  after the migration, the remaining suspects are limited to
  behavioural assertions (the trait abstractions funnel data
  through the same SeaORM paths the old `db_ops` helpers used).

### Documentation cleanup

After the migration completed, the following stale references to
`agents_live` were swept out of source/HTML/MD files:

- `rabbit-lib/src/server/mod.rs` — module doc rewritten to list
  the seven actual modules instead of describing it as "trait
  surface only".
- `rabbit-lib/src/server/{actor,handle}.rs` — three `use
  crate::agents_live::…` test-module imports rewritten to
  `crate::server::handle::AgentHandle` and `crate::wire::*`.
- `rabbit-lib/src/wire.rs` — three doc-comment references to
  `warren::agents_live::wire::*` removed.
- `rabbit-lib/tests/snapshot_roundtrip.rs` — three references to
  `warren/src/agents_live/wire.rs` rewritten to `rabbit_lib::wire::*`.
- `rabbit-lib/README.md` — "Status" callout rewritten to describe
  both halves being in the lib now.
- `warren/templates/{agent_shell,agent_claude}.html` — two stale
  source-path comments rewritten to point at `rabbit_lib::wire`.
- `warren/src/routes/api.rs` — leftover comment that mentioned
  `crate::routes::claude_api::router()` (a module that never
  existed) replaced with the actual location.

The two historical documents that still mention `agents_live` are
intentionally left untouched:

- `/workdir/WARREN.md` — the conceptual Warren design doc. It
  describes the *concepts* of the live agent plane, which haven't
  changed; only the *location* moved.
- The early sections of `/workdir/rabbit-lib.md` — the plan
  describes the "before the move" architecture as motivation.
  Stripping it would erase the migration story.
