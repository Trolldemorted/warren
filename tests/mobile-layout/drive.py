#!/usr/bin/env python3
"""Drive a headless chromium over CDP to assert the warren agent page
layout at multiple viewports.

Boots a fresh warren on a scratch Postgres DB (atlas migrate apply),
optionally a rabbit (skipped if /usr/bin/claude is missing), opens
the agent page in chromium at each viewport width, measures the
resulting layout via Runtime.evaluate, and writes screenshots +
measurements.json to ./artifacts/. Exits non-zero if any assertion
fails.

The probe catches two specific regressions from the mobile-layout
"running gag":

  1. xterm FitAddon feedback loop — term.height drift away from
     wrap.clientHeight.
  2. Mobile keypad last row clipped on small coarse-pointer
     viewports.

The probe also asserts `matchMedia('(pointer: coarse)')` matches the
expected pointer for the viewport so a silently ineffective CDP
emulation can't make coarse cases a false green.
"""
import base64
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Any

# Each tuple: (label, width, height, mobile, expected_pointer).
# Mobile=true triggers touch + coarse media; expected_pointer is
# what matchMedia should report at this viewport — used as a
# precondition so a broken Emulation override can't false-green
# the assertions below.
VIEWPORTS: list[tuple[str, int, int, bool, str]] = [
    ("desktop-fine",   1400, 900, False, "fine"),
    ("tablet-fine",    1100, 900, False, "fine"),
    ("tablet-coarse",   980, 900, True,  "coarse"),
    ("phone-landscape", 768, 900, True,  "coarse"),
    ("phone-portrait",  390, 844, True,  "coarse"),
]

HERE = Path(__file__).resolve().parent
ARTIFACTS = HERE / "artifacts"
PROBE_TIMEOUT_S = 15.0
NAV_TIMEOUT_S = 20.0
WARREN_HEALTH_TIMEOUT_S = 30.0


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def pick_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def psql_exec(url: str, sql: str) -> str:
    out = subprocess.run(
        ["psql", url, "-tAc", sql],
        check=True, capture_output=True, text=True,
    )
    return out.stdout.strip()


def wait_for_port(port: int, timeout_s: float) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return
        except OSError:
            time.sleep(0.2)
    raise TimeoutError(f"port {port} did not open within {timeout_s}s")


def wait_for_http(url: str, timeout_s: float) -> None:
    deadline = time.monotonic() + timeout_s
    last_err: Exception | None = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                if r.status < 500:
                    return
        except Exception as e:  # noqa: BLE001
            last_err = e
        time.sleep(0.3)
    raise TimeoutError(f"{url} did not respond within {timeout_s}s: {last_err}")


def http_post_json(url: str, body: dict, cookie: str | None = None) -> tuple[int, dict, str]:
    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method="POST")
    req.add_header("content-type", "application/json")
    if cookie:
        req.add_header("cookie", cookie)
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            set_cookie = r.headers.get("set-cookie", "")
            payload = json.loads(r.read().decode() or "{}")
            return r.status, payload, set_cookie
    except urllib.error.HTTPError as e:
        set_cookie = e.headers.get("set-cookie", "") if e.headers else ""
        try:
            payload = json.loads(e.read().decode() or "{}")
        except Exception:  # noqa: BLE001
            payload = {}
        return e.code, payload, set_cookie


class CDP:
    """Thin CDP client over websocket-client. Tracks in-flight requests
    by id; events without an id are dropped (we don't subscribe to any)."""

    def __init__(self, ws_url: str):
        import websocket  # imported here so the module-level import isn't required
        self._ws = websocket.create_connection(ws_url, timeout=10)
        self._ws.settimeout(10)
        self._next_id = 1

    def close(self) -> None:
        try:
            self._ws.close()
        except Exception:  # noqa: BLE001
            pass

    def call(self, method: str, params: dict | None = None, timeout_s: float = 10.0) -> dict:
        msg_id = self._next_id
        self._next_id += 1
        self._ws.send(json.dumps({"id": msg_id, "method": method, "params": params or {}}))
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            raw = self._ws.recv()
            if not raw:
                continue
            msg = json.loads(raw)
            if msg.get("id") != msg_id:
                continue
            if "error" in msg:
                raise RuntimeError(f"CDP {method}: {msg['error']}")
            return msg.get("result", {})
        raise TimeoutError(f"CDP {method} timed out after {timeout_s}s")

    # ---- Convenience wrappers -------------------------------------------------

    def set_viewport(self, width: int, height: int, mobile: bool) -> None:
        self.call("Emulation.setDeviceMetricsOverride", {
            "width": width,
            "height": height,
            "deviceScaleFactor": 2 if mobile else 1,
            "mobile": mobile,
        })
        self.call("Emulation.setTouchEmulationEnabled", {"enabled": mobile})
        if mobile:
            self.call("Emulation.setEmulatedMedia", {"features": [{"name": "pointer", "value": "coarse"}]})
        else:
            self.call("Emulation.setEmulatedMedia", {"features": [{"name": "pointer", "value": "fine"}]})

    def navigate(self, url: str) -> None:
        self.call("Page.navigate", {"url": url}, timeout_s=NAV_TIMEOUT_S)

    def evaluate(self, expression: str, timeout_s: float = 10.0) -> Any:
        result = self.call(
            "Runtime.evaluate",
            {
                "expression": expression,
                "returnByValue": True,
                "awaitPromise": True,
            },
            timeout_s=timeout_s,
        )
        if "exceptionDetails" in result:
            raise RuntimeError(f"eval exception: {result['exceptionDetails']}")
        return result.get("result", {}).get("value")

    def screenshot(self, clip: dict | None = None) -> bytes:
        params: dict = {"format": "png"}
        if clip is not None:
            params["clip"] = clip
        result = self.call("Page.captureScreenshot", params, timeout_s=15.0)
        return base64.b64decode(result["data"])

    def set_session_cookie(self, name: str, value: str, domain: str, path: str = "/") -> None:
        self.call("Network.setCookie", {
            "name": name,
            "value": value,
            "domain": domain,
            "path": path,
            "httpOnly": True,
        })


PROBE = r"""
(() => {
  const q = s => document.querySelector(s);
  const rect = el => {
    if (!el) return null;
    const b = el.getBoundingClientRect();
    const cs = getComputedStyle(el);
    return {
      x: Math.round(b.x), y: Math.round(b.y),
      w: Math.round(b.width), h: Math.round(b.height),
      r: Math.round(b.right), btm: Math.round(b.bottom),
      disp: cs.display, vis: cs.visibility,
    };
  };
  const de = document.documentElement;
  const vw = window.innerWidth;
  const term = q('#term');
  const wrap = q('.term-wrap');
  const keypad = q('.mobile-keypad');
  // Last visible row of the keypad: the bottom-most .row whose display
  // isn't none. If a row is clipped, its bottom will exceed vh.
  let keypadLastRowBottom = null;
  let keypadLastRow = null;
  if (keypad && getComputedStyle(keypad).display !== 'none') {
    const rows = [...keypad.querySelectorAll('.row')];
    for (const row of rows) {
      if (getComputedStyle(row).display === 'none') continue;
      const rb = row.getBoundingClientRect();
      const text = (row.textContent || '').trim().slice(0, 24);
      keypadLastRowBottom = Math.round(rb.bottom);
      keypadLastRow = text;
    }
  }
  // The xterm canvas cols/rows as last seen by `refit()` in the
  // template (or at `term.open()` for the shell template, which
  // doesn't use FitAddon). Reads `window.__lastCols`/`__lastRows`,
  // which stay 0 until the first successful refit — that's exactly
  // the regression we want to catch (the page shipped at 160×50
  // because the wrap was transiently 0×0 and refit bailed).
  const lastCols = window.__lastCols || 0;
  const lastRows = window.__lastRows || 0;
  // The .claude-grid gets the .disconnected class server-side if
  // the live-registry snapshot at page-render time had no entry for
  // this agent (i.e. rabbit is not currently connected to warren).
  // `setConnected(true)` removes it on first WS message. When true,
  // the .offline-overlay covers the wrap — the buffer is empty by
  // design (no ScreenSnapshot will arrive), so the content probe
  // below has to skip.
  const grid = q('#claude-grid') || q('#shell-grid');
  const disconnected = !!(grid && grid.classList.contains('disconnected'));
  // Walk the visible xterm rows and dump the painted text. Returns
  // null when the terminal isn't there (shell template, or before
  // xterm.js loaded). Returns the *joined text* of every non-blank
  // visible row (up to `MAX_CONTENT_LINES`) plus the concatenated
  // length so a future assertion can check *what* was painted, not
  // just whether *something* was — the user's gripe with the
  // previous dimension-only probe was exactly that "xterm refit ok"
  // doesn't imply "the Claude greeting actually rendered". Capture
  // ANSI escape sequences as the raw character codes (no strip)
  // because translateToString(true) already drops them — but
  // box-drawing chars / arrows stay as the JS string the buffer
  // holds, so a `Claude` substring or `Welcome` substring search is
  // the right assertion shape for downstream checks.
  const MAX_CONTENT_LINES = 20;
  const contentLines = [];
  let contentLen = 0;
  if (window.term && window.term.buffer && window.term.buffer.active) {
    const buf = window.term.buffer.active;
    for (let i = 0; i < buf.length && contentLines.length < MAX_CONTENT_LINES; i++) {
      const line = buf.getLine(i);
      if (!line) continue;
      const t = line.translateToString(true).replace(/\s+$/, '');
      if (t.length > 0) {
        contentLines.push(t);
        contentLen += t.length;
      }
    }
  }
  return {
    vw, vh: window.innerHeight,
    docScrollW: de.scrollWidth,
    docClientW: de.clientWidth,
    docScrollH: de.scrollHeight,
    docClientH: de.clientHeight,
    hOverflow: de.scrollWidth > de.clientWidth + 1,
    vOverflow: de.scrollHeight > de.clientHeight + 1,
    mqCoarse: matchMedia('(pointer: coarse)').matches,
    mqFine: matchMedia('(pointer: fine)').matches,
    wrap: rect(wrap),
    term: rect(term),
    termScrollH: term ? term.scrollHeight : null,
    termClientH: term ? term.clientHeight : null,
    keypad: rect(keypad),
    keypadLastRow,
    keypadLastRowBottom,
    lastCols,
    lastRows,
    disconnected,
    contentLen,
    contentLines,
  };
})()
"""


def assert_viewport(name: str, m: dict, mobile: bool, measurements: list[dict]) -> list[str]:
    """Returns a list of failure messages. Empty list means pass."""
    fails: list[str] = []
    # Precondition: pointer media query must match what we asked for.
    if mobile and not m["mqCoarse"]:
        fails.append(f"mqCoarse false at mobile viewport (CDP emulation ineffective?)")
    if not mobile and not m["mqFine"]:
        fails.append(f"mqFine false at desktop viewport (CDP emulation ineffective?)")
    # Horizontal overflow breaks every mobile layout contract.
    if m["hOverflow"]:
        fails.append(
            f"horizontal overflow: scrollWidth={m['docScrollW']} > clientWidth={m['docClientW']}"
        )
    # The FitAddon feedback-loop regression: term grew/shrank to
    # something other than the wrap. Tolerate a 2px rounding delta.
    if m["term"] and m["wrap"]:
        delta = abs(m["term"]["h"] - m["wrap"]["h"])
        if delta > 2:
            fails.append(
                f"term height {m['term']['h']} != wrap height {m['wrap']['h']} (delta {delta}px)"
            )
    # Coarse viewports with a visible keypad: last row must fit inside
    # the viewport — the original "last row clipped" report.
    if mobile and m["keypad"] and m["keypad"]["disp"] != "none":
        if m["keypadLastRowBottom"] is None:
            fails.append("mobile-keypad visible but no rows found")
        elif m["keypadLastRowBottom"] > m["vh"]:
            fails.append(
                f"keypad last row ({m['keypadLastRow']!r}) clipped: "
                f"bottom={m['keypadLastRowBottom']} > vh={m['vh']}"
            )
    # xterm canvas was actually refit to the wrap. `window.__lastCols`
    # stays 0 if `refitWhenReady` exhausted its retries without ever
    # getting valid dims from `fitAddon.proposeDimensions()` — that's
    # the "desktop page sometimes broken at load" regression, where
    # xterm ships at the 160×50 template default. We don't check the
    # *value* of lastCols when 0; we just refuse to silently green a
    # build that never sized its canvas.
    if m["lastCols"] <= 0:
        fails.append("xterm canvas was never refit (window.__lastCols == 0)")
    else:
        # Coarse viewports: xterm canvas cols must be narrow enough to
        # fit a phone. The wrap at 390 CSS px is ~370 px inside
        # padding; at 8 px/char (ui-monospace 13px) that's ~46 cols.
        # Anything ≥ 80 means the refit fell back to the template
        # 160-col default and long lines will wrap character-by-
        # character on the user's phone.
        if mobile and m["lastCols"] > 80:
            fails.append(
                f"xterm too wide on mobile: cols={m['lastCols']} "
                f"(wrap.clientWidth={m['wrap']['w'] if m['wrap'] else '?'}px) "
                f"— long lines will wrap mid-word"
            )
        # Fine viewports: xterm canvas should at least span most of
        # the wrap. A 60-col canvas in a 1200px wrap means the refit
        # bailed early — about the same as the mobile-side regression.
        if not mobile and m["lastCols"] < 60:
            fails.append(
                f"xterm too narrow on desktop: cols={m['lastCols']} "
                f"(wrap.clientWidth={m['wrap']['w'] if m['wrap'] else '?'}px)"
            )
        # Sanity: refit dims should match `wrap.clientWidth / 8`
        # (monospace char width at 13px). ±2 tolerates the padding
        # the wrap loses to its `.5rem` inner padding and the
        # renderer rounding to integer cols.
        if m["wrap"] and m["wrap"]["w"] > 0:
            expected_cols = m["wrap"]["w"] // 8
            if abs(m["lastCols"] - expected_cols) > 2:
                fails.append(
                    f"xterm cols {m['lastCols']} != wrap.clientWidth/8 ≈ {expected_cols} "
                    f"(wrap.clientWidth={m['wrap']['w']}px)"
                )
        # Content probe: when the grid is *not* in offline-overlay
        # mode (the WS handshake succeeded and `setConnected(true)`
        # removed the `.disconnected` class), xterm must have
        # actually painted the Claude TUI — not just sized the canvas.
        # This is the assertion that would have caught the
        # "fully-black terminal pane on desktop" regression: every
        # layout assertion above passes when the canvas is the right
        # size but the buffer was never written to (no `term.write()`
        # ever fired). The previous test suite was dimension-only and
        # let this slip. Skip in offline mode — no `ScreenSnapshot`
        # flows when rabbit is absent (CI on `ubuntu-latest` doesn't
        # ship `/usr/bin/claude`), so the buffer is empty by design.
        #
        # Three layered checks when online:
        # 1. `contentLen >= 200` — the Claude greeting banner alone
        #    is several hundred characters across ~10 lines; a value
        #    well below that means the canvas is mostly blank.
        # 2. `len(contentLines) >= 3` — the banner has structure (a
        #    border, multiple rows of content, a prompt indicator).
        #    A single non-blank line would mean only the cursor or a
        #    single char got through.
        # 3. At least one line contains "claude" (case-insensitive)
        #    — Claude Code's TUI greets with "Welcome to Claude Code"
        #    or similar; "claude" appears in every version we've
        #    shipped. If Anthropic drops the substring from the
        #    greeting entirely, this is the canary that catches it
        #    (and the assertion message makes the failure obvious).
        if not m["disconnected"]:
            joined = " ".join(m["contentLines"])
            if m["contentLen"] < 200:
                fails.append(
                    f"xterm canvas is online but only {m['contentLen']} chars "
                    f"of text rendered across {len(m['contentLines'])} lines "
                    f"— Claude's greeting banner is several hundred chars. "
                    f"Captured lines: {m['contentLines']!r}"
                )
            elif len(m["contentLines"]) < 3:
                fails.append(
                    f"xterm canvas is online but only "
                    f"{len(m['contentLines'])} non-blank line(s) rendered — "
                    f"the greeting banner has multiple rows. "
                    f"Captured lines: {m['contentLines']!r}"
                )
            elif "claude" not in joined.lower():
                fails.append(
                    f"xterm canvas is online and painted "
                    f"{m['contentLen']} chars but none of them contain "
                    f"'claude' — the Claude Code greeting should be visible. "
                    f"Captured lines: {m['contentLines']!r}"
                )
    log(f"  {name}: "
        f"vw={m['vw']}x{m['vh']} "
        f"mqCoarse={m['mqCoarse']} "
        f"term.h={m['term']['h'] if m['term'] else 'n/a'} "
        f"wrap.h={m['wrap']['h'] if m['wrap'] else 'n/a'} "
        f"lastCols={m['lastCols']} "
        f"keypad.row.bottom={m['keypadLastRowBottom']} "
        f"disconnected={m['disconnected']} "
        f"contentLen={m['contentLen']} "
        f"contentLines={len(m['contentLines'])} "
        f"{'FAIL ' + '; '.join(fails) if fails else 'ok'}")
    # When the content probe is what failed, dump the captured lines
    # so the human reviewer can see exactly what got painted — much
    # faster debugging than re-running with a browser.
    if any('contentLen' in f or 'non-blank' in f or "'claude'" in f for f in fails):
        log(f"  {name} captured content:")
        for i, line in enumerate(m["contentLines"]):
            log(f"    [{i:02d}] {line!r}")
    return fails


def find_chrome_page_target(port: int) -> dict:
    """Poll /json until a 'page' target is ready, return it."""
    deadline = time.monotonic() + 15.0
    last_err: Exception | None = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/json", timeout=2) as r:
                targets = json.loads(r.read().decode())
            for t in targets:
                if t.get("type") == "page":
                    return t
        except Exception as e:  # noqa: BLE001
            last_err = e
        time.sleep(0.3)
    raise TimeoutError(f"no page target on chromium debug port {port}: {last_err}")


def main() -> int:
    warren_bin = os.environ["WARREN_BIN"]
    browser = os.environ["MOBILE_LAYOUT_BROWSER"]
    with_rabbit = os.environ.get("MOBILE_LAYOUT_RABBIT") == "1"

    db = f"warren_layout_{os.getpid()}_{time.time_ns()}"
    admin_url = "postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable"
    target_url = f"postgres://postgres@127.0.0.1:5432/{db}?sslmode=disable"

    warren_port = pick_free_port()
    rabbit_port = pick_free_port()
    cdp_port = pick_free_port()

    run_id = time.strftime("%Y%m%dT%H%M%S")
    run_dir = ARTIFACTS / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    log(f"artifacts: {run_dir}")

    warren: subprocess.Popen | None = None
    rabbit: subprocess.Popen | None = None
    chrome: subprocess.Popen | None = None

    try:
        # 1. scratch DB + migrations
        log(f"creating scratch DB {db}")
        psql_exec(admin_url, f'CREATE DATABASE "{db}"')

        migrations_dir = HERE.parent.parent / "warren" / "migrations_atlas"
        apply = subprocess.run(
            ["atlas", "migrate", "apply",
             "--dir", f"file://{migrations_dir}",
             "--url", target_url],
            check=False, capture_output=True, text=True,
        )
        if apply.returncode != 0:
            log(f"atlas migrate apply failed:\n{apply.stderr}")
            return 1
        log("migrations applied")

        # 2. boot warren
        psk = "test-psk-" + "x" * 32
        env = dict(os.environ, DATABASE_URL=target_url, WARREN_ADMIN_PSK=psk,
                   RUST_LOG="warn")
        log(f"booting warren on :{warren_port}")
        env["BIND_ADDR"] = f"127.0.0.1:{warren_port}"
        warren = subprocess.Popen(
            [warren_bin, "server"],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        try:
            wait_for_http(f"http://127.0.0.1:{warren_port}/healthz", WARREN_HEALTH_TIMEOUT_S)
        except TimeoutError:
            warren.terminate()
            log(f"warren failed to become healthy. log:\n{warren.stdout.read() if warren.stdout else ''}")
            return 1
        log("warren healthy")

        base = f"http://127.0.0.1:{warren_port}"

        # 3. login (cookie session) + create agent. We POST via urllib so
        #    we can capture the admin cookie, then later inject it into
        #    the browser via CDP Network.setCookie.
        status, _, set_cookie = http_post_json(
            f"{base}/api/login", {"password": psk},
        )
        if status != 200 or "warren_session=" not in set_cookie:
            log(f"login failed: status={status} set-cookie={set_cookie!r}")
            return 1
        cookie_name, _, cookie_val = set_cookie.split(";", 1)[0].partition("=")
        cookie = f"{cookie_name}={cookie_val}"
        log(f"logged in as admin ({cookie_name}=<redacted>)")

        status, agent, _ = http_post_json(
            f"{base}/api/agents",
            {"name": "mobile-layout-test", "class": "claude", "model": "sonnet"},
            cookie=cookie,
        )
        if status != 200 or "id" not in agent:
            log(f"create agent failed: status={status} body={agent}")
            return 1
        agent_id = agent["id"]
        authtoken = agent.get("authtoken", "")
        log(f"created agent {agent_id}")

        # 4. optionally spawn rabbit
        if with_rabbit:
            rabbit_bin = HERE.parent.parent / "target" / "debug" / "rabbit"
            if not rabbit_bin.exists():
                rabbit_bin_path = shutil.which("rabbit")
                rabbit_bin = Path(rabbit_bin_path) if rabbit_bin_path else rabbit_bin  # type: ignore[arg-type]
            if not rabbit_bin.exists():  # type: ignore[union-attr]
                log("note: rabbit binary not built — skipping rabbit spawn")
            else:
                rabbit_env = dict(os.environ, DATABASE_URL=target_url,
                                  WARREN_URL=base, WARREN_TOKEN=authtoken,
                                  WORKDIR="/tmp", RUST_LOG="warn")
                log(f"booting rabbit on :{rabbit_port}")
                rabbit = subprocess.Popen(
                    [str(rabbit_bin)],
                    env=rabbit_env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                )

        # 5. boot chromium with remote debugging
        profile = tempfile.mkdtemp(prefix="warren-chrome-")
        log(f"booting chromium on :{cdp_port} (profile {profile})")
        chrome = subprocess.Popen(
            [browser,
             "--headless=new",
             "--no-sandbox",
             "--disable-gpu",
             "--disable-dev-shm-usage",
             f"--user-data-dir={profile}",
             f"--remote-debugging-port={cdp_port}",
             "--window-size=1400,900",
             "about:blank"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_for_port(cdp_port, 15.0)
        target = find_chrome_page_target(cdp_port)
        cdp = CDP(target["webSocketDebuggerUrl"])
        try:
            cdp.call("Page.enable")
            cdp.call("Runtime.enable")
            cdp.call("Network.enable")
            cdp.set_session_cookie(cookie_name, cookie_val, "127.0.0.1")

            measurements: list[dict] = []
            all_fails: dict[str, list[str]] = {}

            for name, w, h, mobile, expected_pointer in VIEWPORTS:
                cdp.set_viewport(w, h, mobile)
                # give the viewport change a tick to settle
                time.sleep(0.4)
                url = f"{base}/agent/{agent_id}/claude"
                cdp.navigate(url)
                # wait for the page to settle — xterm or the offline
                # overlay. 4s gives `refitWhenReady`'s 20-frame
                # retry loop (~320ms typical, up to ~640ms in
                # slow-paint cases) time to finish before the WS
                # `ScreenSnapshot` arrives and writes content into
                # the canvas.
                time.sleep(4.0)

                m = cdp.evaluate(PROBE, timeout_s=PROBE_TIMEOUT_S)
                m["viewport"] = name
                m["width"] = w
                m["height"] = h
                m["mobile"] = mobile
                m["expected_pointer"] = expected_pointer
                measurements.append(m)

                vp_dir = run_dir / name
                vp_dir.mkdir(exist_ok=True)
                (vp_dir / "full.png").write_bytes(cdp.screenshot())
                if m["wrap"]:
                    cb = m["wrap"]
                    (vp_dir / "term.png").write_bytes(cdp.screenshot({
                        "x": cb["x"], "y": cb["y"],
                        "width": cb["w"], "height": cb["h"],
                        "scale": 1,
                    }))
                if m["keypad"] and m["keypad"]["disp"] != "none":
                    cb = m["keypad"]
                    (vp_dir / "keypad.png").write_bytes(cdp.screenshot({
                        "x": cb["x"], "y": cb["y"],
                        "width": cb["w"], "height": cb["h"],
                        "scale": 1,
                    }))

                fails = assert_viewport(name, m, mobile, measurements)
                if fails:
                    all_fails[name] = fails

            (run_dir / "measurements.json").write_text(
                json.dumps({
                    "run_id": run_id,
                    "agent_id": agent_id,
                    "with_rabbit": with_rabbit,
                    "viewports": measurements,
                }, indent=2)
            )

            if all_fails:
                log("FAIL:")
                for name, fails in all_fails.items():
                    for f in fails:
                        log(f"  {name}: {f}")
                return 1
            log("all viewports passed")
            return 0
        finally:
            cdp.close()
    finally:
        for p, name in ((chrome, "chromium"), (rabbit, "rabbit"), (warren, "warren")):
            if p and p.poll() is None:
                p.terminate()
                try:
                    p.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait(timeout=5)
                _ = name
        try:
            psql_exec(admin_url, f'DROP DATABASE IF EXISTS "{db}"')
        except Exception as e:  # noqa: BLE001
            log(f"warning: drop database failed: {e}")


if __name__ == "__main__":
    sys.exit(main())
