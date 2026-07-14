#!/usr/bin/env bash
# Run deploy-g1.sh on g1 from your dev machine via vssh (optional wrapper).
#
# Requires: SSH to g1 (Tailscale 100.111.32.108) or `vssh exec g1`.
# Does not require MCP — plain SSH is enough.
#
# Usage:
#   ./scripts/deploy-g1-remote.sh
#   DRY_RUN=1 ./scripts/deploy-g1-remote.sh
#   CONFIRM_VANE_STOP=1 ./scripts/deploy-g1-remote.sh
#   SKIP_VANE_STOP=1 ./scripts/deploy-g1-remote.sh
#
# Manual equivalent (no vssh):
#   rsync -az --exclude target --exclude .git ./ dragon@100.111.32.108:~/orgos-lab/metasearch/
#   ssh dragon@100.111.32.108 'cd ~/orgos-lab/metasearch && CONFIRM_VANE_STOP=1 ./scripts/deploy-g1.sh'
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
G1_HOST="${G1_HOST:-100.111.32.108}"
G1_USER="${G1_USER:-dragon}"
REMOTE_DIR="${REMOTE_DIR:-orgos-lab/metasearch}"
REMOTE="${G1_USER}@${G1_HOST}"

REMOTE_ENV=()
for var in DRY_RUN SKIP_VANE_STOP CONFIRM_VANE_STOP METASEARCH_DIR VANE_COMPOSE HOST_PORT; do
  if [[ -n "${!var:-}" ]]; then
    REMOTE_ENV+=("${var}=${!var}")
  fi
done
ENV_PREFIX=""
if ((${#REMOTE_ENV[@]} > 0)); then
  ENV_PREFIX="${REMOTE_ENV[*]} "
fi

run_remote() {
  local cmd=$1
  if command -v vssh >/dev/null 2>&1; then
    echo "+ vssh exec ${G1_HOST} -- bash -lc $(printf '%q' "$cmd")"
    if [[ "${DRY_RUN:-}" == "1" ]]; then
      return 0
    fi
    vssh exec "${G1_HOST}" -- bash -lc "$cmd"
  else
    echo "+ ssh ${REMOTE} bash -lc $(printf '%q' "$cmd")"
    if [[ "${DRY_RUN:-}" == "1" ]]; then
      return 0
    fi
    ssh "${REMOTE}" bash -lc "$cmd"
  fi
}

if command -v rsync >/dev/null 2>&1; then
  RSYNC_FLAGS=(-az --delete --exclude target --exclude .git --exclude .metasearch-cache)
  if [[ "${DRY_RUN:-}" == "1" ]]; then
    echo "[dry-run] rsync ${RSYNC_FLAGS[*]} ${ROOT}/ ${REMOTE}:~/${REMOTE_DIR}/"
  else
    echo "+ rsync ${ROOT}/ → ${REMOTE}:~/${REMOTE_DIR}/"
    rsync "${RSYNC_FLAGS[@]}" "${ROOT}/" "${REMOTE}:~/${REMOTE_DIR}/"
  fi
fi

REMOTE_CMD="set -euo pipefail
mkdir -p ~/${REMOTE_DIR}
cd ~/${REMOTE_DIR}
chmod +x scripts/deploy-g1.sh
${ENV_PREFIX}./scripts/deploy-g1.sh"

run_remote "$REMOTE_CMD"
