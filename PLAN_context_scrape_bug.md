# Live `/context` scrape bug — plan as of 2026-07-14

## Status (extended by 2026-07-14 18:55)

Parser + supervisor plumbing is **sound on real bytes** (verified via
`rabbit/src/supervisor.rs::tests::run_context_scrape_against_real_capture_bytes`
which feeds today's `/tmp/context5.bin` through the production
`run_context_scrape` and populates all five primary fields including
`free_pct`). The user is still seeing
"context scrape returned no data — claude may be busy or the
terminal too small; try a larger window" in the live UI. The bug is
therefore **downstream of the supervisor** — somewhere in the
warren↔rabbit WS path, the SSE fan-out, or the browser JS — and
**not yet reproduced on my side**.

## What is known

1. Parser regression pinned by `feed_recognizes_compact_renders_freespace_label`
   in `rabbit/src/observer/context.rs:1187`. Fix lives at
   `rabbit/src/observer/context.rs:711` (`let Some(rel) = … else { continue }`).
2. `rabbit/examples/parse_capture.rs` parses
   `/tmp/context5.bin` (8921-byte live capture from this session) to
   `used=24200, total=200000, used_pct=12.0, free_pct=71.4` plus 7
   categories.
3. `run_context_scrape_against_real_capture_bytes` (cargo test
   `rabbit --lib -- --ignored …`) replays `/tmp/context5.bin`
   through the supervisor's full scrape path and asserts the same
   populated snapshot.
4. The Playwright/curl loop reproducer `/tmp/context_repro.sh` is
   ready; user will run it themselves.

## What is still unknown

- Does warren's SSE stream emit a `Usage` envelope at all when the
  live UI's Context button is pressed?
- If yes, is it populated or empty?
- If populated, does the browser's JS read it correctly and update
  the panel?

These three branches each point at exactly one fix; without
answering them I'm guessing.

## Concrete next steps

1. User runs the `/tmp/context_repro.sh` curl loop against their live
   warren URL + agent. One of these four branches will print:
   - `no Usage envelope arrived in 6s` → bug is upstream of warren
     (rabbit never published; check `record_usage()` race and
     `LinkCmd::SendMeta(EnvelopeBody::Usage(combined))` at
     `rabbit/src/supervisor.rs:533-537`).
   - Usage envelope with all `ctx_*` = None → bug is in supervisor's
     merge step (`rabbit/src/supervisor.rs:507-512`). The
     `combined = latest_usage()` call returns stale/cold-from-cold
     context — verify `crate::observer::latest_usage()` returns a
     `UsageSnapshot` whose `ctx_*` fields are `Default::default()`
     Option (which they are) and that nothing in
     `record_usage()` overwrites our local `combined` between the
     read and the send.
   - Usage envelope with full `ctx_*` fields but UI still shows the
     error → bug is JS rendering. Most likely
     `env.t === 'usage'` doesn't match because warren's
     `EnvelopeBody::Usage(_)` wire tag is `"usage"` but the meta
     channel publishes an *unwrapped* `UsageSnapshot` whose `t`
     field is whatever `serde_json::to_string(&UsageSnapshot)`
     produces (no `t`). Check the JS `case 'usage':` arm in
     `warren/templates/agent_claude.html:516` and confirm what
     `env.t` actually is on the wire.
   - Usage envelope lands but JS sees a *different* `t` → envelope
     wire tag mismatch; serde rename or version drift.

2. If the curl loop runs but the message is ambiguous, stand up the
   CDP/chromium browser test described in
   `/root/.claude/projects/-workdir/memory/live-warren-rabbit-repro.md`
   to drive the actual page. chromium is now installed in the
   baseimage (added this session).

## Files in flight (not yet `git commit`ed)

- `rabbit/src/observer/context.rs` —
  `match_free_space` fix at line 711, regression test at line 1187.
- `rabbit/src/supervisor.rs` —
  `run_context_scrape_populates_all_fields_on_captured_modal`
  (synthetic, runs in CI)
  and `run_context_scrape_against_real_capture_bytes`
  (`#[ignore]`, requires `/tmp/context5.bin`).
- `rabbit/examples/parse_capture.rs` — diagnostic with `//!` doc.
- `/tmp/cap2.py` — tightened trigger (separate `bypass` /
  `permissions` substrings, since Claude Code 2.1+ paints them
  separated by cursor positioning).
- `/tmp/context5.bin` — fresh 8921-byte live capture from this
  session.
- `/tmp/context_repro.sh` — admin curl loop, hands the user a
  four-way decision tree.
- `memory/live-claude-pty-capture.md`,
  `memory/live-warren-rabbit-repro.md`,
  `MEMORY.md` updated.

## Baseimage changes recommended (this session)

- Add `chromium` (apt). ~150 MB. Required for the CDP recipe.
- Add `python3-pip` + `python3-websocket-client` (or
  `python3-playwright`).
- Document `/dev/pts` requirement.
- Document the seven nested-Code env marker names future smoke
  tests must strip.
- Consider a pre-trusted scratch dir under `/tmp/` so first-run
  repro tests skip the trust dialog.

## Rollback note

All supervisor and parser changes are in `#[ignore]` and
regression tests respectively; nothing affects a CI run unless
the user passes `--ignored`. Trivial to revert with
`git checkout` per file.

## End-to-end live reproduction (this session, 18:55)

A live rabbit was spawned against this container's warren (PSK
`shellsmoke123`, agent `47682757-178c-4f8c-91ac-4f2ad70f5190`,
claude child PID `169735`, workdir `/tmp/scratch-rabbit`). After
warm-up, three diagnostic surfaces all came back green:

1. **Wire envelope** (SSE `…/claude/events/stream`):
   `{"t":"usage","source":"context_check","ctx_used_tokens":26500,
   "ctx_total_tokens":200000,"ctx_used_pct":13.0,"ctx_free_pct":70.2,
   "ctx_window_tokens":200000,"ctx_categories":{6 entries},
   "ctx_scrape_incomplete":false}`.
2. **Parser + supervisor** (`cargo test -p rabbit --lib
   run_context_scrape_against_real_capture_bytes -- --ignored`):
   returns the same shape from `/tmp/context5.bin`.
3. **Browser DOM** (headless chromium CDP driver, `/tmp/cdp_drive.py`):
   `#ctx-used-total` = `28,000 / 200,000`, `#ctx-used-pct` = `14.0%`,
   `#ctx-free-pct` = `69.5%`, `#ctx-window` = `200,000 tokens`,
   `#ctx-categories` populated, `#ctx-hint` empty.

**On this warren the bug does not reproduce end-to-end.** Branches
1–3 of the original decision tree are ruled out on the local stack.

## Where to look on the user's warren

If the user is still seeing the no-data hint, that environment has
a regression not present locally. Most likely candidates given
what's *not* exercised here:

- A warren older than the four "hopefully fix context" commits
  (commits `8c558ca`, `0aa3e17`, `00d9fa1`, `bdaa4e6`).
- A warren behind a reverse proxy that buffers the SSE / WS,
  dropping the first replay-frame or the first Usage envelope on
  reconnect. CDP-driven repro is unaffected by this if it points
  at the user's URL.
- A different Claude Code version where `/context` paints the
  modal differently (parser regression in a different package
  version).

Re-run `/tmp/cdp_drive.py` against the user's URL once the local
warren is replaced — the script's verdict line names the branch
without operator judgement.

## New files this session

- `/tmp/cdp_drive.py` — headless chromium CDP driver for the
  agent page. Reads `PSK` and `AGENT` env. Verdict line prints
  one of three outcomes.
- `memory/cdp-agent-page-driver.md` + MEMORY.md updated.
- `PLAN_context_scrape_bug.md` — this section added.

## Running infrastructure left over

- `rabbit` PID (process group supervisor, claude PID 169735) is
  still running. Kill it with `pkill -P <rabbit_pid>` or
  `kill <rabbit_pid> 169735` when finished.
- `chromium` PID listening on `:9222`. Kill with `pkill -f chrom`.
- The agent `47682757-…` and channel `b435350a-…` were created in
  the live DB and may need cleanup via `DELETE` from the admin UI.
