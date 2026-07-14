#!/usr/bin/env bash
# Deploy metasearch on g1 (ORGOS lab), replacing Perplexica/Vane.
#
# Run ON g1 (SSH / vssh) from anywhere; idempotent. Does not run unless invoked.
#
# Environment:
#   METASEARCH_DIR   install path (default: ~/orgos-lab/metasearch)
#   VANE_COMPOSE     Perplexica compose file (default: ~/orgos-lab/Perplexica/docker-compose.yaml)
#   HOST_PORT        published UI port (default: 3003)
#   DRY_RUN=1        print actions, do not stop Vane or start containers
#   SKIP_VANE_STOP=1 skip stopping perplexica-vane-1 / Perplexica stack
#   CONFIRM_VANE_STOP=1 non-interactive yes to stop Vane (required in CI/automation)
#
# Examples:
#   ./scripts/deploy-g1.sh
#   DRY_RUN=1 ./scripts/deploy-g1.sh
#   SKIP_VANE_STOP=1 ./scripts/deploy-g1.sh
#   CONFIRM_VANE_STOP=1 ./scripts/deploy-g1.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
METASEARCH_DIR="${METASEARCH_DIR:-${HOME}/orgos-lab/metasearch}"
VANE_COMPOSE="${VANE_COMPOSE:-${HOME}/orgos-lab/Perplexica/docker-compose.yaml}"
HOST_PORT="${HOST_PORT:-3003}"
COMPOSE_FILE="deploy/docker-compose.g1.yml"
HEALTH_URL="http://127.0.0.1:${HOST_PORT}/healthz"

log() { printf '%s\n' "$*"; }
run() {
  if [[ "${DRY_RUN:-}" == "1" ]]; then
    log "[dry-run] $*"
  else
    log "+ $*"
    "$@"
  fi
}

stop_vane() {
  if [[ "${SKIP_VANE_STOP:-}" == "1" ]]; then
    log "SKIP_VANE_STOP=1 — leaving Vane / Perplexica running."
    return 0
  fi

  local need_stop=0
  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx 'perplexica-vane-1'; then
    need_stop=1
  elif [[ -f "$VANE_COMPOSE" ]]; then
    if docker compose -f "$VANE_COMPOSE" ps -q 2>/dev/null | grep -q .; then
      need_stop=1
    fi
  fi

  if [[ "$need_stop" -eq 0 ]]; then
    log "No running Vane / Perplexica stack detected — nothing to stop."
    return 0
  fi

  log ""
  log "This will stop Perplexica/Vane (UI :3003, embedded SearXNG :8889 on host)."
  log "Standalone SearXNG on :8888 is NOT stopped."
  if [[ "${CONFIRM_VANE_STOP:-}" != "1" ]]; then
    read -r -p "Stop Vane now? [y/N] " ans
    case "${ans,,}" in
      y|yes) ;;
      *) log "Aborted — set SKIP_VANE_STOP=1 to deploy alongside Vane (port conflict likely)."; exit 1 ;;
    esac
  fi

  if docker ps --format '{{.Names}}' 2>/dev/null | grep -qx 'perplexica-vane-1'; then
    run docker stop perplexica-vane-1
  fi
  if [[ -f "$VANE_COMPOSE" ]]; then
    run docker compose -f "$VANE_COMPOSE" down
  fi
}

sync_repo() {
  local root_canon deploy_canon
  root_canon="$(cd "$ROOT" && pwd)"
  deploy_canon="$(cd "$METASEARCH_DIR" 2>/dev/null && pwd || echo "")"

  if [[ "$root_canon" == "$deploy_canon" ]] || [[ -z "$deploy_canon" && "$METASEARCH_DIR" == "${HOME}/orgos-lab/metasearch" ]]; then
    log "Using repo at $root_canon"
    DEPLOY_ROOT="$root_canon"
    if [[ -d "$DEPLOY_ROOT/.git" ]]; then
      run git -C "$DEPLOY_ROOT" pull --ff-only
    fi
    return 0
  fi

  if [[ -n "$deploy_canon" && -d "$deploy_canon/.git" ]]; then
    log "Updating existing clone at $deploy_canon"
    DEPLOY_ROOT="$deploy_canon"
    run git -C "$DEPLOY_ROOT" pull --ff-only
    return 0
  fi

  log "Installing metasearch to $METASEARCH_DIR"
  run mkdir -p "$(dirname "$METASEARCH_DIR")"
  if [[ -d "$ROOT/.git" ]]; then
    run git clone "$ROOT" "$METASEARCH_DIR" 2>/dev/null || run git -C "$METASEARCH_DIR" pull --ff-only
  else
    run rsync -a --delete \
      --exclude target --exclude .git --exclude .metasearch-cache \
      "$ROOT/" "$METASEARCH_DIR/"
  fi
  DEPLOY_ROOT="$(cd "$METASEARCH_DIR" && pwd)"
}

wait_healthy() {
  local deadline=$((SECONDS + 120))
  if [[ "${DRY_RUN:-}" == "1" ]]; then
    log "Waiting for ${HEALTH_URL} (skipped in dry-run)"
    return 0
  fi
  echo -n "Waiting for ${HEALTH_URL} "
  while (( SECONDS < deadline )); do
    if curl -sf "$HEALTH_URL" >/dev/null 2>&1; then
      echo "ok"
      return 0
    fi
    echo -n "."
    sleep 2
  done
  echo
  log "error: metasearch did not become healthy within 120s"
  log "  docker compose -f ${DEPLOY_ROOT}/${COMPOSE_FILE} logs --tail=50"
  exit 1
}

print_urls() {
  log ""
  log "metasearch is up (replaces Vane on host :${HOST_PORT})."
  log ""
  log "  Tailscale:  http://100.111.32.108:${HOST_PORT}/"
  log "  LAN:        http://192.168.50.44:${HOST_PORT}/"
  log "  VPN:        http://10.98.32.103:${HOST_PORT}/"
  log "  localhost:  http://127.0.0.1:${HOST_PORT}/"
  log ""
  log "Smoke tests:"
  log "  curl -sf ${HEALTH_URL}"
  log "  curl -sf 'http://127.0.0.1:${HOST_PORT}/api/v1/research?q=test' | head -c 200"
  log ""
  log "Rollback Vane:"
  log "  docker compose -f ${VANE_COMPOSE} up -d"
  log "  docker compose -f ${DEPLOY_ROOT}/${COMPOSE_FILE} down"
}

main() {
  log "=== metasearch g1 deploy ==="
  stop_vane
  sync_repo
  cd "$DEPLOY_ROOT"
  run docker compose -f "$COMPOSE_FILE" up -d --build
  wait_healthy
  print_urls
}

main "$@"
