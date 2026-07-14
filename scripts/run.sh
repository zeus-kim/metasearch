#!/usr/bin/env bash
# Build, start, and open the metasearch web UI.
# Usage: ./scripts/run.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Resolve port: env > settings.yml > default 8889
if [[ -z "${METASEARCH_PORT:-}" ]] && [[ -f settings.yml ]]; then
  METASEARCH_PORT="$(grep -E '^\s*port:' settings.yml | head -1 | awk '{print $2}')"
fi
PORT="${METASEARCH_PORT:-8889}"
BIND="${METASEARCH_BIND:-127.0.0.1}"
DISPLAY_HOST="$BIND"
if [[ "$DISPLAY_HOST" == "0.0.0.0" ]] || [[ -z "$DISPLAY_HOST" ]]; then
  DISPLAY_HOST="127.0.0.1"
fi
BASE_URL="http://${DISPLAY_HOST}:${PORT}"

if [[ "${METASEARCH_RELEASE:-}" == "1" ]]; then
  BIN="${METASEARCH_BIN:-target/release/metasearch}"
  BUILD_CMD=(cargo build --release --bin metasearch)
else
  BIN="${METASEARCH_BIN:-target/debug/metasearch}"
  BUILD_CMD=(cargo build --bin metasearch)
fi

stop_stale_metasearch() {
  local pids
  pids="$(lsof -ti ":${PORT}" 2>/dev/null || true)"
  [[ -z "$pids" ]] && return 0

  for pid in $pids; do
    local cmd
    cmd="$(ps -p "$pid" -o comm= 2>/dev/null || echo "unknown")"
    if echo "$cmd" | grep -qi metasearch; then
      echo "Stopping stale metasearch (PID ${pid}) on port ${PORT}..."
      kill "$pid" 2>/dev/null || true
      sleep 0.5
    else
      echo "error: port ${PORT} is in use by '${cmd}' (PID ${pid}), not metasearch."
      echo "  Find the process:  lsof -i :${PORT}"
      echo "  Or change server.port in settings.yml (or set METASEARCH_PORT)."
      exit 1
    fi
  done
}

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    echo
    echo "Stopping metasearch (PID ${SERVER_PID})..."
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

echo "Building metasearch — first run may take 1-2 min..."
"${BUILD_CMD[@]}"
echo "If you don't see 'Deep research' next to Images tab, hard-refresh (Cmd+Shift+R)"

stop_stale_metasearch

echo "Starting metasearch on ${BASE_URL}/ ..."
"$BIN" &
SERVER_PID=$!

echo -n "Waiting for server"
deadline=$((SECONDS + 30))
while (( SECONDS < deadline )); do
  if curl -sf "${BASE_URL}/healthz" >/dev/null 2>&1; then
    echo " ok"
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo
    echo "error: metasearch exited before becoming ready. See logs above."
    wait "$SERVER_PID" || true
    exit 1
  fi
  echo -n "."
  sleep 0.5
done

if ! curl -sf "${BASE_URL}/healthz" >/dev/null 2>&1; then
  echo
  echo "error: server did not respond on ${BASE_URL}/healthz within 30s."
  exit 1
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
  open "${BASE_URL}/"
fi

echo "Server running at ${BASE_URL}/ — Ctrl+C to stop"
wait "$SERVER_PID"
