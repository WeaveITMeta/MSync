#!/usr/bin/env bash
# Build MSync, start it in dev mode on a free local port with a fresh temporary database
# and the eustress-engine example as a local CRA, run scripts/smoke.sh against it, and
# stop the server by the PID this script started (never a blanket kill).
#
# Usage (from the repository root):
#   bash scripts/smoke-local.sh
#
# Optional environment:
#   MSYNC_SMOKE_BIN        use this binary instead of building (skips cargo build)
#   MSYNC_SMOKE_RELEASE=1  build and run the release binary
#   MSYNC_SMOKE_TMP        where the temporary database and log go (default target/smoke-run)
#   MSYNC_SMOKE_KEEP=1     keep the server running after the test (prints its PID)
#
# Every secret below is generated for this run, lives only in this process's environment,
# and is thrown away. Nothing here contacts any host but 127.0.0.1.

set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

PY=""
for c in python3 python; do
  if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import socket' >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "smoke-local.sh: python (standard library) not found" >&2; exit 2; }

# ------------------------------------------------------------------ build
if [ -n "${MSYNC_SMOKE_BIN:-}" ]; then
  BIN="$MSYNC_SMOKE_BIN"
else
  PROFILE=debug
  FLAGS=""
  if [ "${MSYNC_SMOKE_RELEASE:-}" = "1" ]; then PROFILE=release; FLAGS="--release"; fi
  echo "== cargo build $FLAGS"
  cargo build $FLAGS || { echo "smoke-local.sh: build failed" >&2; exit 1; }
  TD="${CARGO_TARGET_DIR:-target}"
  BIN="$TD/$PROFILE/msync"
  [ -f "$BIN.exe" ] && BIN="$BIN.exe"
fi
[ -f "$BIN" ] || { echo "smoke-local.sh: no binary at $BIN" >&2; exit 1; }

# ------------------------------------------------------------------ a free port, a temp dir
PORT=$("$PY" -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
TMP_ROOT="${MSYNC_SMOKE_TMP:-$ROOT/target/smoke-run}"
mkdir -p "$TMP_ROOT"
RUN_DIR="$TMP_ROOT/run-$(date +%s)-$$"
mkdir -p "$RUN_DIR"
# A native Windows binary run from Git Bash needs C:/... paths, not /c/...
native() { if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi; }
DB="$(native "$RUN_DIR")/smoke.db"
LOG="$RUN_DIR/server.log"

rand_hex() { "$PY" -c "import secrets; print(secrets.token_hex($1))"; }
export MSYNC_SMOKE_URL="http://127.0.0.1:$PORT"
export MSYNC_SMOKE_ADMIN_TOKEN="smoke-admin-$(rand_hex 16)"
export MSYNC_SMOKE_GH_SECRET="smoke-gh-$(rand_hex 16)"
export MSYNC_SMOKE_GL_SECRET="smoke-gl-$(rand_hex 16)"
export MSYNC_SMOKE_SLACK_SECRET="smoke-slack-$(rand_hex 16)"
export MSYNC_SMOKE_CONSUMER_SECRET="smoke-consumer-$(rand_hex 24)"

echo "== starting $BIN on 127.0.0.1:$PORT (database $DB, log $LOG)"
# The server's environment, explicitly: dev mode, local CRA, no Discord, no Stripe, no
# network connector. `env -i` so nothing from the caller's shell (a real token) leaks in.
env -i PATH="$PATH" SYSTEMROOT="${SYSTEMROOT:-}" TEMP="${TEMP:-/tmp}" TMP="${TMP:-/tmp}" HOME="${HOME:-}" \
  RUST_LOG="info,msync=info" \
  MSYNC_DEV_MODE=true \
  DATABASE_URL="sqlite://$DB" \
  SERVER_HOST=127.0.0.1 SERVER_PORT="$PORT" \
  MSYNC_PUBLIC_URL="http://127.0.0.1:$PORT" \
  MSYNC_CRA_LOCAL_DIR="$(native "$ROOT/examples/cra/eustress-engine")" \
  MSYNC_OWNER_MEMBER_IDS="github:example-founder" \
  MSYNC_ADMIN_TOKEN="$MSYNC_SMOKE_ADMIN_TOKEN" \
  GITHUB_WEBHOOK_SECRET="$MSYNC_SMOKE_GH_SECRET" \
  GITLAB_WEBHOOK_SECRET="$MSYNC_SMOKE_GL_SECRET" \
  MSYNC_SLACK_SIGNING_SECRET="$MSYNC_SMOKE_SLACK_SECRET" \
  MSYNC_SLACK_TEAM_ID="T1000000000" \
  MSYNC_SLACK_API_BASE="http://127.0.0.1:9/slack-api-disabled" \
  MSYNC_SUBJECT_KEY="smoke-subject-key-$(rand_hex 24)" \
  MSYNC_CONSUMER_SECRET_EUSTRESS_SITE_K1="$MSYNC_SMOKE_CONSUMER_SECRET" \
  MSYNC_KEY_ENCRYPTION_KEY="$(rand_hex 32)" \
  MSYNC_PAYOUT_SCHEDULER_SECS=0 \
  MSYNC_ROLE_SYNC_SECS=0 \
  "$BIN" serve > "$LOG" 2>&1 &
PID=$!
echo "== server PID $PID"

stop() {
  if [ "${MSYNC_SMOKE_KEEP:-}" = "1" ]; then
    echo "== leaving the server running: PID $PID, $MSYNC_SMOKE_URL"
    return
  fi
  if kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null
    for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
    kill -0 "$PID" 2>/dev/null && kill -9 "$PID" 2>/dev/null
  fi
  echo "== server PID $PID stopped"
}
trap stop EXIT

# ------------------------------------------------------------------ wait for /health
UP=""
for _ in $(seq 1 600); do
  if ! kill -0 "$PID" 2>/dev/null; then break; fi
  if curl -s -o /dev/null -m 2 "$MSYNC_SMOKE_URL/health"; then UP=1; break; fi
  sleep 0.5
done
if [ -z "$UP" ]; then
  echo "smoke-local.sh: the server did not come up; last log lines:" >&2
  tail -n 40 "$LOG" >&2
  exit 1
fi
echo "== server is up"

bash "$ROOT/scripts/smoke.sh"
CODE=$?
if [ "$CODE" -ne 0 ]; then
  echo "== server log (last 60 lines):"
  tail -n 60 "$LOG" | sed 's/\x1b\[[0-9;]*m//g'
fi
exit "$CODE"
