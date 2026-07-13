# rabbit

The per-agent supervisor. One `rabbit` process wraps one real `claude` CLI in a
PTY and bridges it to [warren](../warren) over a single WebSocket link:

- **terminal plane** — raw PTY bytes streamed both ways (channel `0x01`), with a
  bounded replay buffer so late browser joiners see the current screen.
- **meta plane** — structured events (state, usage, prompt echoes, logs, …)
  carried as JSON envelopes with monotonic `seq`s, buffered in a bounded
  [`MetaRing`](src/meta_ring.rs) and replayed on reconnect until warren `Ack`s
  them.

The `rabbit` binary is a thin wrapper over [`rabbit::run`](src/lib.rs); a second
binary, `rabbit-hook`, is the shim installed into claude's `settings.json` to
forward lifecycle hooks to the supervisor's observer port.

## Configuration (env)

All configuration is via environment variables (see [`config.rs`](src/config.rs)):

| Var | Default | Purpose |
|-----|---------|---------|
| `WARREN_URL` | — (required) | warren base URL (`http(s)://…`; rewritten to `ws(s)://…/ws/rabbit`). |
| `AGENT_TOKEN` | — (required) | Bearer token presented on the WS upgrade. |
| `WORKDIR` | `/workdir` | claude's cwd; also where `settings.json` hooks are installed. |
| `CLAUDE_BIN` | `claude` | claude executable. |
| `CLAUDE_ARGS` | `--dangerously-skip-permissions` | Base argv (space-split). |
| `MODEL` | — | Appended as `--model <MODEL>` if set. |
| `TERM_COLS` / `TERM_ROWS` | `120` / `40` | Initial PTY size. |
| `REPLAY_BYTES` | `262144` | Terminal replay-buffer cap (bytes). |
| `META_RING_BYTES` | `262144` | Unacked-meta replay-buffer cap (bytes). |
| `OBSERVER_PORT` | `7777` | Local port the hook shim posts lifecycle events to. |
| `HEALTH_PORT` | `8080` | Serves `/healthz` and `/readyz`. |
| `SHUTDOWN_GRACE_MS` | `1500` | Grace budget after ESC before a hard kill (see below). |
| `CRASH_WINDOW_SECS` / `CRASH_THRESHOLD` | `300` / `3` | Crash-loop guard. |
| `RABBIT_HOOK_BIN` | — | Override the path baked into installed hooks. |
| `AUTO_TRUST` | `1` (on) | Auto-accept claude's first-run trust dialog (see below). Set `0`/`false`/`no` to disable. |
| `RABBIT_ENABLE_SHELL` | `0` (off) | Spawn a second bash PTY and expose it as `/agent/:id/shell`. |
| `SHELL_BIN` | `/bin/bash` | Binary for the shell PTY. |
| `SHELL_ARGS` | `-i` | Space-split base argv for the shell PTY. |

## Kubernetes deployment

### Persistent volumes — mount **both** `/workdir` and `~/.claude`

Session resume across pod restarts depends on two independent pieces of on-disk
state, so **both** must live on a PVC (not `emptyDir`):

- **`/workdir`** (`$WORKDIR`) — the project tree claude operates on.
- **`~/.claude`** — where claude stores its session transcripts and the
  encoded-cwd conversation history that `--resume`/`--continue` read back.

Respawn policy ([`respawn.rs`](src/respawn.rs)) makes the reason concrete:

- On restart with a known session id (from the `SessionStart` hook), rabbit
  respawns `claude --resume <id>`.
- Before the hook has fired (e.g. immediately after a cold pod start), it falls
  back to `claude --continue`, which resumes claude's *most recent* conversation
  — **but only if `~/.claude` survived**. On an `emptyDir`, a pod restart wipes
  the transcript and `--resume <id>` fails, silently starting a fresh session
  and losing context.

Minimal shape:

```yaml
volumeMounts:
  - { name: work,   mountPath: /workdir }
  - { name: claude, mountPath: /home/rabbit/.claude }   # adjust to the image's $HOME
volumes:
  - { name: work,   persistentVolumeClaim: { claimName: agent-work } }
  - { name: claude, persistentVolumeClaim: { claimName: agent-claude } }
```

### Probes

The health server ([`health.rs`](src/health.rs)) exposes:

- **`/healthz`** — always `200 ok` while the process is up. Use as the
  **liveness** probe.
- **`/readyz`** — `200 ready` only when a claude child is running and the
  supervisor is not shutting down; `503` otherwise. Use as the **readiness**
  probe so warren stops routing to a pod that is draining or has no live child.

```yaml
livenessProbe:  { httpGet: { path: /healthz, port: 8080 } }
readinessProbe: { httpGet: { path: /readyz,  port: 8080 } }
```

### Graceful shutdown vs `terminationGracePeriodSeconds`

On `SIGTERM`/`SIGINT` the supervisor flips `/readyz` to `503`, sends claude an
`ESC`, and waits up to `SHUTDOWN_GRACE_MS` (default **1500ms**) for it to exit;
if it doesn't, the child is hard-killed and the process exits. That budget is
enforced by [`supervisor::graceful_expired`](src/supervisor.rs) and unit-tested.

Keep `terminationGracePeriodSeconds` comfortably above `SHUTDOWN_GRACE_MS` — the
operator-recommended **30s** leaves ample headroom — so the kubelet never
SIGKILLs mid-drain. Note the grace budget bounds *shutdown*, not *startup*:
a cold `claude --resume` over a very long transcript can add startup latency,
which readiness (not the termination grace period) absorbs.

### First-run trust dialog

On a **fresh** workdir (a newly-mounted PVC), claude's first launch blocks on a
"Do you trust the files in this folder?" dialog and swallows any pasted prompt
as its answer. Unattended, nobody presses Enter and the agent hangs on first
boot. The supervisor therefore watches PTY output for the dialog and
auto-accepts it with Enter ([`trust.rs`](src/trust.rs)), bounded to a few
accepts so a stray marker in model output can't spew keystrokes. Disable with
`AUTO_TRUST=0` if you pre-trust the workdir out of band (the `trust` markers are
the single source of truth shared with the `claude_smoke` test).

## Tests

```sh
cargo test -p rabbit            # unit + CI-safe integration tests
cargo test -p rabbit -- --ignored   # includes claude_smoke (spawns real claude)
```

CI-safe integration tests under [`tests/`](tests) stub every dependency — a
local WS server ([`meta_replay.rs`](tests/meta_replay.rs)), a `/bin/cat` fake
TUI ([`tests/common`](tests/common)), fixture transcripts, etc. Anything that
spawns a real `claude` or hits the API is gated behind `#[ignore]`
([`claude_smoke.rs`](tests/claude_smoke.rs)) and must be run in a purpose-built
environment, never a live one.
