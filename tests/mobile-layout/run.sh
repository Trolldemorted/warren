#!/usr/bin/env bash
# Boots a fresh warren on a scratch DB, spawns a rabbit, drives a
# headless browser at the agent page across viewports, asserts on
# CDP-measured layout (no image diff), and writes screenshots +
# measurements.json to ./artifacts/ for human review.
#
# Every prerequisite is HARD REQUIRED. Missing any of them is a CI
# configuration bug, not a skip condition — the script fails loudly
# with the install command. There is no "skip if missing" path: a
# silent exit-0 here would let a broken layout regression ship to
# master uncaught, which is the failure mode this whole test exists
# to prevent.
set -euo pipefail
# Don't `cd "$(dirname "$0")"` here — that breaks when the runner's
# cwd already is the script's directory (e.g. via a workflow
# `working-directory:` directive, or `bash run.sh` from inside
# tests/mobile-layout), where `cd ./tests/mobile-layout` fails with
# "No such file or directory". The script never needs its own dir;
# SCRIPT_DIR is resolved via ${BASH_SOURCE[0]} for the warren
# binary lookup, and drive.py is invoked as an absolute path
# below.

# 1. python deps. websocket-client drives the CDP websocket.
python3 -c 'import websocket' 2>/dev/null \
  || pip install --quiet --break-system-packages websocket-client

# 2. atlas binary. CI installs via ariga/setup-atlas@v0.
export PATH="${PATH}:/tmp"
if ! command -v atlas >/dev/null 2>&1; then
  echo "FAIL: atlas binary not on PATH. CI installs via the workflow's 'Install Atlas' step." >&2
  echo "      Locally: download from https://atlasgo.io and put atlas on PATH." >&2
  exit 1
fi

# 3. postgres client + server. CI installs psql via apt and runs
#    postgres:16 as a service container.
if ! command -v psql >/dev/null 2>&1; then
  echo "FAIL: psql not on PATH. CI installs via 'sudo apt-get install -y postgresql-client'." >&2
  exit 1
fi
if ! psql 'postgres://postgres@127.0.0.1:5432/postgres?sslmode=disable' \
      -tAc 'SELECT 1' >/dev/null 2>&1; then
  echo "FAIL: postgres unreachable at 127.0.0.1:5432." >&2
  echo "      CI uses the services.postgres block in ci-integration.yml." >&2
  echo "      Locally: docker run -d -p 5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:16" >&2
  exit 1
fi

# 4. warren binary. Resolve the script's own location first so the
#    search root isn't hardcoded to `/workdir` (this dev container's
#    bind-mount path) — on GitHub Actions the checkout lives at
#    $GITHUB_WORKSPACE (`/home/runner/work/<repo>/<repo>`) and the
#    workspace build drops the binary at <repo>/target/debug/warren.
#    Same for drive.py's rabbit lookup at HERE.parent.parent, so the
#    two stay in sync. Falls back to PATH for `cargo install`-style
#    setups.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
WARREN_BIN=""
if [[ -x "$REPO_ROOT/target/debug/warren" ]]; then
  WARREN_BIN="$REPO_ROOT/target/debug/warren"
elif command -v warren >/dev/null 2>&1; then
  WARREN_BIN=$(command -v warren)
else
  echo "FAIL: warren binary not found at $REPO_ROOT/target/debug/warren (run: cargo build --workspace)" >&2
  exit 1
fi
export WARREN_BIN
export REPO_ROOT

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
  echo "FAIL: no usable chrome/chromium binary on PATH." >&2
  echo "      CI installs google-chrome-stable via dl.google.com deb." >&2
  echo "      Locally: wget -qO /tmp/chrome.deb https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb && sudo apt-get install -y /tmp/chrome.deb" >&2
  exit 1
fi

# 6. claude + rabbit are checked inside drive.py and fail with their
#    own install instructions. The CI workflow installs claude via
#    the upstream installer (writes to ~/.local/bin/claude, which is
#    on PATH for ubuntu-latest runners) and rabbit via
#    `cargo build --workspace`. No need to re-check here.

exec python3 "$SCRIPT_DIR/drive.py" "$@"
