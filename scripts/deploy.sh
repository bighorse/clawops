#!/usr/bin/env bash
# Deploy or upgrade ClawOps on a server. Run as root, on the server.
#
#   bash /opt/clawops/scripts/deploy.sh
#   bash /opt/clawops/scripts/deploy.sh --ref main --no-build   # templates only
#
# Idempotent. Safe to re-run. On a failed health check it puts the previous
# binary back and restarts, so a bad build costs a restart, not an outage.
#
# Assumes server-bootstrap.sh has already run (rust toolchain, directories,
# /etc/systemd/user/zeroclaw@.service).

set -euo pipefail

REPO=/opt/clawops
REF=""
BUILD=1
REFRESH=1
CONFIG=/etc/clawops/clawops.toml
URL="http://127.0.0.1:8088"

usage() {
  cat <<'USAGE'
Options:
  --ref <git-ref>   Check out this ref before building (default: leave as-is).
  --no-build        Skip cargo build; deploy templates/config only.
  --no-refresh      Don't re-render existing tenants' workspaces.
  --config <path>   clawops.toml (default /etc/clawops/clawops.toml).
  --url <url>       Health-check URL (default http://127.0.0.1:8088).
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ref) REF="$2"; shift 2 ;;
    --no-build) BUILD=0; shift ;;
    --no-refresh) REFRESH=0; shift ;;
    --config) CONFIG="$2"; shift 2 ;;
    --url) URL="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 1 ;;
  esac
done

[[ $EUID -eq 0 ]] || { echo "must run as root" >&2; exit 1; }
log() { echo "[$(date '+%H:%M:%S')] $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

# Non-interactive ssh does not load the profile, so cargo is off PATH. This
# is the single most common way `ssh root@host 'deploy.sh'` fails with a
# bare "command not found".
export PATH="/root/.cargo/bin:$PATH"

[[ -d "$REPO" ]] || die "$REPO not found — clone the repo there first"
[[ -f "$CONFIG" ]] || die "$CONFIG not found — copy clawops.example.toml and edit it"

# ── 1. source ───────────────────────────────────────────────────────
if [[ -n "$REF" ]]; then
  log "fetching and checking out $REF"
  # A dirty tree here means someone edited on the server without
  # committing. Refuse rather than silently discarding their work — this
  # has happened before and the edits were the only copy.
  if [[ -n "$(git -C "$REPO" status --porcelain)" ]]; then
    git -C "$REPO" status --short >&2
    die "$REPO has uncommitted changes (above). Commit, stash or discard them first."
  fi
  git -C "$REPO" fetch --quiet origin "$REF"
  git -C "$REPO" checkout --quiet FETCH_HEAD
fi
log "source at $(git -C "$REPO" rev-parse --short HEAD) ($(git -C "$REPO" log -1 --format=%s | head -c 60))"

# ── 2. build ────────────────────────────────────────────────────────
if [[ $BUILD -eq 1 ]]; then
  log "cargo build --release (5-10 min on first run)"
  # Never pipe this: a pipe swallows cargo's exit code and a failed build
  # reads as success. Log to a file and check the status directly.
  BUILD_LOG=$(mktemp)
  if ! ( cd "$REPO" && cargo build --release ) >"$BUILD_LOG" 2>&1; then
    tail -40 "$BUILD_LOG" >&2
    die "build failed (full log: $BUILD_LOG)"
  fi
  rm -f "$BUILD_LOG"
  log "build ok"
fi

NEW_BIN="$REPO/target/release/clawops"
[[ -x "$NEW_BIN" ]] || die "$NEW_BIN missing — build first (drop --no-build)"

# ── 3. back up DB and binary ────────────────────────────────────────
DB_URL=$(grep -E '^\s*url\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*"\(.*\)"/\1/')
DB_PATH=${DB_URL#sqlite://}; DB_PATH=${DB_PATH%%\?*}
STAMP=$(date '+%Y%m%d-%H%M%S')
if [[ -f "$DB_PATH" ]]; then
  mkdir -p /var/backups/clawops
  # sqlite3's own backup is WAL-safe; a plain cp of a live DB is not.
  if command -v sqlite3 >/dev/null; then
    sqlite3 "$DB_PATH" ".backup '/var/backups/clawops/clawops-$STAMP.db'"
  else
    cp "$DB_PATH" "/var/backups/clawops/clawops-$STAMP.db"
    echo "  (sqlite3 not installed — used cp; install it for a WAL-safe backup)" >&2
  fi
  log "DB backed up to /var/backups/clawops/clawops-$STAMP.db"
fi

OLD_BIN=""
if [[ -f /usr/local/bin/clawops ]]; then
  OLD_BIN=/usr/local/bin/clawops.old
  cp /usr/local/bin/clawops "$OLD_BIN"
fi

# ── 4. install ──────────────────────────────────────────────────────
# A running executable cannot be overwritten in place ("Text file busy").
# Copy alongside, then rename — rename swaps the inode and the running
# process keeps its old one until it exits.
install -m 0755 "$NEW_BIN" /usr/local/bin/.clawops.new
mv /usr/local/bin/.clawops.new /usr/local/bin/clawops
log "installed $(/usr/local/bin/clawops --version 2>/dev/null || echo 'clawops (no --version)')"

install -m 0644 "$REPO/systemd/zeroclaw@.service" /etc/systemd/user/zeroclaw@.service
install -m 0644 "$REPO/systemd/clawops.service" /etc/systemd/system/clawops.service

# Mirror the repo's default-breed templates into place, deleting what the
# repo no longer has. rsync isn't on every box (it was missing from
# server-bootstrap.sh until recently), so fall back to a wipe-and-copy.
sync_tree() {
  local src=$1 dst=$2
  if command -v rsync >/dev/null; then
    rsync -a --delete "$src/" "$dst/"
    return
  fi
  # Guard the rm: a mis-parsed config value must not point this at / or ~.
  case "$dst" in
    /|/etc|/usr|/var|/home|"") die "refusing to wipe '$dst'" ;;
    /*) : ;;
    *) die "template_dir must be an absolute path, got '$dst'" ;;
  esac
  rm -rf "${dst:?}"
  mkdir -p "$dst"
  cp -a "$src/." "$dst/"
}

TEMPLATE_DIR=$(grep -E '^\s*template_dir\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*"\(.*\)"/\1/')
if [[ -n "$TEMPLATE_DIR" ]]; then
  mkdir -p "$TEMPLATE_DIR"
  sync_tree "$REPO/templates/workspace" "$TEMPLATE_DIR"
  log "templates synced to $TEMPLATE_DIR"
fi

BREEDS_DIR=$(grep -E '^\s*breeds_dir\s*=' "$CONFIG" | head -1 | sed 's/.*=\s*"\(.*\)"/\1/')
if [[ -n "$BREEDS_DIR" ]]; then
  # Only created, never synced from the repo: breeds are pushed by
  # `push-breed.sh`, they are not repo content.
  mkdir -p "$BREEDS_DIR"
  log "breeds_dir present: $BREEDS_DIR"
fi

systemctl daemon-reload

# ── 5. restart and health-check ─────────────────────────────────────
# A restart drops in-flight /chat requests and every open SSE stream.
# Clients see ERR_CONNECTION_CLOSED, not an error response.
log "restarting clawops"
systemctl enable --quiet clawops 2>/dev/null || true
systemctl restart clawops

HEALTHY=0
for _ in $(seq 1 30); do
  if curl -sf -m 3 "$URL/health" >/dev/null 2>&1; then HEALTHY=1; break; fi
  sleep 1
done

if [[ $HEALTHY -eq 0 ]]; then
  echo "health check failed after 30s. Last 40 log lines:" >&2
  journalctl -u clawops -n 40 --no-pager >&2
  if [[ -n "$OLD_BIN" && -f "$OLD_BIN" ]]; then
    log "rolling back to previous binary"
    install -m 0755 "$OLD_BIN" /usr/local/bin/.clawops.new
    mv /usr/local/bin/.clawops.new /usr/local/bin/clawops
    systemctl restart clawops
    sleep 3
    curl -sf -m 3 "$URL/health" >/dev/null 2>&1 \
      && echo "rolled back; old binary is healthy" >&2 \
      || echo "rollback ALSO unhealthy — check the config, not the binary" >&2
  fi
  exit 1
fi
log "health ok: $(curl -s "$URL/health")"

# ── 6. roll templates out to existing tenants ───────────────────────
if [[ $REFRESH -eq 1 ]]; then
  COUNT=$(/usr/local/bin/clawops --config "$CONFIG" list 2>/dev/null | grep -c . || true)
  if [[ "${COUNT:-0}" -gt 0 ]]; then
    log "re-rendering $COUNT tenant workspace(s)"
    /usr/local/bin/clawops --config "$CONFIG" refresh-workspace --all \
      || die "some tenants failed to refresh — see above; the gateway itself is healthy"
  else
    log "no tenants yet; nothing to refresh"
  fi
fi

log "deploy done. Breeds on this box:"
/usr/local/bin/clawops --config "$CONFIG" breeds 2>/dev/null | grep -v ' INFO \| DEBUG ' || true
