# Mobile-layout integration test

A script-only integration test that boots a fresh warren (on a scratch
Postgres DB), optionally a rabbit (skipped if `/usr/bin/claude` is
missing), opens the agent page in headless chromium at multiple
viewport widths, and asserts on **CDP-measured layout** — not pixel
diffs. Screenshots and `measurements.json` are saved to
`./artifacts/` for human review.

## Run it

```sh
./tests/mobile-layout/run.sh
```

Every prerequisite is **hard-required** — missing any of them fails
the script with the install command, no silent skip. There is no
"exit 0 if infrastructure missing" path: a silent skip here is the
exact failure mode this test exists to prevent.

| Prerequisite | How CI provides it |
| ------------ | ------------------ |
| `psql` client | `sudo apt-get install -y postgresql-client` |
| Postgres server | `services.postgres: postgres:16` in workflow |
| `atlas` | `ariga/setup-atlas@v0` |
| `warren` + `rabbit` | `cargo build --workspace` |
| `claude` | `curl -fsSL https://claude.ai/install.sh \| bash` |
| Chrome | `google-chrome-stable_current_amd64.deb` from dl.google.com |
| `websocket-client` | `pip install --break-system-packages websocket-client` |

## What it asserts

For each viewport in
`tests/mobile-layout/drive.py::VIEWPORTS`:

1. **`matchMedia('(pointer: coarse)')` matches the requested
   pointer.** A precondition — if CDP emulation is silently
   ineffective, the rest of the suite would be a false green.
2. **No horizontal overflow.** `documentElement.scrollWidth >
   clientWidth + 1` is a fail.
3. **FitAddon feedback loop guard** (when `#term` exists):
   `term.getBoundingClientRect().height ≈ wrap.clientHeight`
   within ±2 px. This catches the regression class behind the
   "fix mobile" running gag in the commit log.
4. **Mobile keypad last row visible** on coarse-pointer viewports:
   the bottom-most `.row`'s `bottom` must be ≤ `window.innerHeight`.
5. **Claude greeting barrier arrangement**: the xterm buffer's top
   + bottom banner borders are present (≥ 2 barrier rows detected),
   aligned at the same start/end columns (± 1 for renderer
   rounding), and contain no internal gaps (between first and last
   `─` of a row, every cell must be `─`). Accepts both `─`
   (U+2500, light horizontal — Claude Code's logged-in banner) and
   `╌` (U+254B, heavy horizontal — the first-run theme-picker menu
   that claude shows without a subscription; CI runs in this
   state). Catches banner geometry that survived the content
   probe but is structurally broken — e.g., a missing cell from a
   bad reflow, or the banner wrapping because the canvas was too
   narrow so top and bottom borders no longer line up.
6. **Input cursor stays at column 0** (when online): Claude Code's
   TUI shows the user prompt as `❯` (U+276F) at column 0 of its
   dedicated row. When the xterm canvas is too narrow for Claude's
   intended TUI width, xterm wraps at the cell level and the
   prompt glyph ends up mid-line on a wrapped status fragment —
   visible as the "New task?" prefix getting cut to "ew task?"
   and partial words ("r", "n") floating at the right edge.
   Detection walks every buffer row and flags any row where a
   cursor glyph appears past column 0 AND column 0 holds real
   content (avoiding false-positives on leading-blank rows that
   just happen to contain a typed `>` echo). To exercise the
   post-accept input-box state (the state real users see), the
   probe advances past Claude's first-run workspace trust dialog
   on the first viewport by sending `\r` over a secondary
   WebSocket opened from the page context, then a Down arrow +
   Enter to dismiss Claude's account-type picker menu (CI runs
   Claude fresh without a subscription, which shows that menu
   after the trust dialog). If the second menu can't be dismissed
   (Claude can't reach anonymous login), the probe detects the
   menu state from the buffer — `❯ N.` pattern at column 1 — and
   suppresses the prompt-glyph check rather than false-failing
   the menu's intentional cursor position.

6. **Layout collapse under banner-visible state** (every viewport):
   when the `#prompt-rejected-banner` is visible (the probe forces
   this so the assertion fires regardless of whether a real prompt
   rejection is in flight), `.term-wrap.height` must be ≥ 100 px.
   This catches the regression where the banner — a direct child
   of `.claude-grid` — gets grid-auto-placed into row 1 col 2,
   pushing the aside into row 2 col 1. The aside's intrinsic
   content then drives row 2 tall, collapsing the term-wrap row to
   its `minmax(0, 1fr)` floor (~16 px) and making the term pane
   invisible. The check fires whether or not rabbit is online —
   the probe forces the banner visible itself.
6. **No buffer overflow at any viewport**: every xterm buffer row's
   length is ≤ canvas width. Lines longer than the canvas wrap
   visually but the page has no horizontal scroll on the canvas
   (`xterm-viewport` ships `overflow-y: scroll` only), so the
   wrapped characters silently clip. The probe advances past
   Claude's first-run trust dialog so this check runs against the
   actual input-box state real users see.
7. **Wrap scrollbar reachable when canvas overflows**: when the
   xterm canvas is wider than `.term-wrap`, the wrap must actually
   be horizontally scrollable (`wrap.scrollWidth > wrap.clientWidth
   + 1`). The agent template sets `MIN_COLS = TERM_COLS` in
   `refit()` so Claude's wide status line fits on one buffer row
   instead of fragmenting mid-word; the wrap's `overflow-x: auto`
   is what makes the rest of the line reachable. If a future
   template regression removes either piece, the buffer-row
   check (above) and this CSS-level check fire together and the
   user's "no horizontal scrollbar anywhere, content silently
   clips" complaint can't slip back in.

## Viewports

| Label             | Width | Height | mobile |
| ----------------- | ----- | ------ | ------ |
| `desktop-fine`    | 1400  | 900    | no     |
| `tablet-fine`     | 1100  | 900    | no     |
| `tablet-coarse`   |  980  | 900    | yes    |
| `phone-landscape` |  768  | 900    | yes    |
| `phone-portrait`  |  390  | 844    | yes    |

## Artifacts

```
tests/mobile-layout/artifacts/<run-id>/<viewport>/
├── full.png        # full viewport screenshot
├── term.png        # .term-wrap element screenshot
├── keypad.png      # .mobile-keypad element screenshot (coarse only)
measurements.json   # raw probe results for every viewport
```

The screenshots are not diffed — they exist for a human reviewer
eyeballing a layout contract change. CI uploads the whole
`artifacts/` tree as a workflow artifact so the actual pixels are
always available.

## CI

`.github/workflows/ci-integration.yml` runs this after the existing
drift steps. The workflow installs all prerequisites that
`ubuntu-latest` doesn't ship by default: `psql` (postgresql-client
apt package), Chrome (the `dl.google.com` deb — GitHub's runner has
no browser on PATH), `claude` (the upstream `claude.ai/install.sh`,
which writes to `~/.local/bin/claude` already on PATH), and the
`websocket-client` Python package.

## How it works (drive.py)

1. CREATE DATABASE `warren_layout_<pid>_<nanos>` on the test
   Postgres; run `atlas migrate apply` against it.
2. Spawn `warren server --bind 127.0.0.1:<port>`, wait on
   `/healthz`.
3. `POST /api/login` with the admin PSK → capture `warren_session`.
4. `POST /api/agents` with `Cookie: warren_session=…` → capture
   `id` and `authtoken`.
5. If `/usr/bin/claude` exists: spawn `rabbit` with
   `WARREN_URL`/`WARREN_TOKEN`/`WORKDIR`.
6. Spawn `chromium --remote-debugging-port=<port>`, find the page
   target via `/json`, connect CDP.
7. For each viewport:
   - `Emulation.setDeviceMetricsOverride` + `setTouchEmulationEnabled` + `setEmulatedMedia` features `pointer:coarse|fine`.
   - `Network.setCookie` injects the admin session.
   - `Page.navigate` to `/agent/<id>/claude`, wait for xterm to settle.
   - `Runtime.evaluate` the layout probe (returns rects, media queries, keypad last-row bottom).
   - `Page.captureScreenshot` for `full.png` and clipped `term.png` / `keypad.png`.
8. Tear down warren + rabbit + chromium; `DROP DATABASE`.

## Why not a Rust integration test?

The first attempt at this was a 543-line `#[ignore]` test using
`headless_chrome`. It OOMed the dev container during a `cargo build`
and got reverted. Scripts run in CI without recompiling Rust, are
trivial to debug interactively, and don't touch the `Cargo.lock`
dependency graph that other contributors depend on.
