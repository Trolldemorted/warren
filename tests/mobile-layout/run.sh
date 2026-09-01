#!/usr/bin/env bash
# Boots a fresh warren on a scratch DB, optionally a rabbit, drives a
# headless browser at the agent page across viewports, asserts on
# CDP-measured layout (no image diff), and writes screenshots +
# measurements.json to ./artifacts/ for human review.
#
# Skips gracefully (exit 0) when prerequisites are missing — matching
# the schema_drift/openapi_drift pattern, so a contributor without
# chrome or postgres isn't broken by CI failure or local runs.
set -euo pipefail
cd "$(dirname "$0")"

# 1. python deps. websocket-client drives the CDP websocket.
python3 -c 'import websocket' 2>/dev/null \
  || pip install --quiet --break-system-packages websocket-client

# 2. atlas binary. CI installs via the workflow; locally it usually
#    lives at /tmp/atlas from a previous download.
export PATH="${PATH}:/tmp"
if ! command -v atlas >/dev/null 2>&1; then
  echo "skip: atlas binary not on PATH (install from https://atlasgo.io)" >&2
  exit 0
fi

# 3. postgres reachable. The test DB lives at 127.0.0.1:5432; see
#    CLAUDE.md "Test database".
if ! command -v psql >/dev/null 2>&1; then
  echo "skip: psql not on PATH" >&2
  exit 0
fi
if ! psql 'postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable' \
      -tAc 'SELECT 1' >/dev/null 2>&1; then
  echo "skip: postgres unreachable at 127.0.0.1:5432" >&2
  exit 0
fi

# 4. warren binary. Either the cargo-built one in the workspace or one
#    on PATH.
WARREN_BIN=""
if [[ -x /workdir/target/debug/warren ]]; then
  WARREN_BIN=/workdir/target/debug/warren
elif command -v warren >/dev/null 2>&1; then
  WARREN_BIN=$(command -v warren)
else
  echo "skip: warren binary not found (run: cargo build -p warren --bin warren)" >&2
  exit 0
fi
export WARREN_BIN

# 5. a real browser binary. chromium-browser on Ubuntu is a snap stub
#    that errors with "install with snap"; reject that. We capture the
#    output into a var so `pipefail` doesn't trip on the stub's
#    non-zero exit even when the grep would match.
for candidate in google-chrome google-chrome-stable chromium chromium-browser; do
  if command -v "$candidate" >/dev/null 2>&1; then
    out=$("$candidate" --version 2>&1 || true)
    if echo "$out" | grep -qi 'snap install\|requires the'; then
      continue
    fi
    export MOBILE_LAYOUT_BROWSER="$candidate"
    break
  fi
done
if [[ -z "${MOBILE_LAYOUT_BROWSER:-}" ]]; then
  echo "skip: no usable chrome/chromium binary on PATH" >&2
  exit 0
fi

# 6. rabbit + claude are optional. drive.py does the
#    `shutil.which("claude")` check itself; when claude is present
#    it spawns rabbit, when absent it logs "offline-overlay mode"
#    and continues with page-chrome-only assertions. The CI workflow
#    installs claude via the upstream installer (writes to
#    ~/.local/bin/claude, not /usr/bin) before this script runs.
#    Fresh CI has no subscription, so claude shows its default
#    first-run menu — the barrier check accepts both `─` (logged-in)
#    and `╌` (theme picker) so we don't gate on auth either.

exec python3 drive.py "$@"
