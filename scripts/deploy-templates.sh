#!/usr/bin/env bash
# Deploy template changes to the running clawops service.
#
# Steps:
#   1. git pull (or skip with --no-pull)
#   2. rsync /opt/clawops/templates/workspace/ → /etc/clawops/templates/workspace/
#   3. POST /admin/refresh-all-workspaces
#
# Usage: deploy-templates.sh [--no-pull] [--dry-run]

set -euo pipefail

REPO_DIR="/opt/clawops"
SRC_TEMPLATES="$REPO_DIR/templates/workspace"
DST_TEMPLATES="/etc/clawops/templates/workspace"
CLAWOPS_URL="http://127.0.0.1:8088"
TOKEN_FILE="/etc/clawops/clawops.toml"

NO_PULL=0
DRY_RUN=0
for arg in "$@"; do
  case $arg in
    --no-pull) NO_PULL=1 ;;
    --dry-run) DRY_RUN=1 ;;
  esac
done

# Read admin token from config
ADMIN_TOKEN=$(grep -E '^token\s*=' "$TOKEN_FILE" | head -1 | sed 's/.*=\s*"\(.*\)"/\1/')
if [[ -z "$ADMIN_TOKEN" ]]; then
  echo "ERROR: could not read admin token from $TOKEN_FILE" >&2
  exit 1
fi

log() { echo "[$(date '+%H:%M:%S')] $*"; }

# Step 1: git pull
if [[ $NO_PULL -eq 0 ]]; then
  log "Step 1: git pull"
  if [[ $DRY_RUN -eq 0 ]]; then
    git -C "$REPO_DIR" pull origin main
  else
    log "  (dry-run) git -C $REPO_DIR pull origin main"
  fi
else
  log "Step 1: skipped (--no-pull)"
fi

# Step 2: rsync templates
log "Step 2: rsync $SRC_TEMPLATES/ → $DST_TEMPLATES/"
if [[ $DRY_RUN -eq 0 ]]; then
  rsync -av --delete "$SRC_TEMPLATES/" "$DST_TEMPLATES/"
else
  rsync -avn --delete "$SRC_TEMPLATES/" "$DST_TEMPLATES/"
fi

# Step 3: refresh all workspaces
log "Step 3: refresh-all-workspaces"
if [[ $DRY_RUN -eq 0 ]]; then
  RESP=$(curl -sf -X POST "$CLAWOPS_URL/admin/refresh-all-workspaces" \
    -H "X-Admin-Token: $ADMIN_TOKEN" \
    -H "Content-Type: application/json")
  log "  response: $RESP"
  # Check for errors
  ERRORS=$(echo "$RESP" | grep -o '"errors":\[[^]]*\]' || true)
  if echo "$RESP" | grep -q '"errors":\[\]'; then
    REFRESHED=$(echo "$RESP" | grep -o '"refreshed":[0-9]*' | grep -o '[0-9]*')
    log "  OK — $REFRESHED workspaces refreshed"
  else
    log "  WARNING: errors in response: $ERRORS" >&2
    exit 1
  fi
else
  log "  (dry-run) POST $CLAWOPS_URL/admin/refresh-all-workspaces"
fi

log "Done."
