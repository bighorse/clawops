#!/usr/bin/env bash
# Push one lobster breed (品种) from a development machine into a ClawOps
# swarm, in one command.
#
#   push-breed.sh --breed shangji --dir ./breed
#
# Reads --url / --token from the environment (CLAWOPS_URL,
# CLAWOPS_ADMIN_TOKEN) so the common case is just --breed and --dir.
#
# What it does, in order:
#   1. checks the tree is a breed (config.toml.hbs present)
#   2. scans it for secrets that must not ride along to every tenant
#   3. computes the tree digest the same way ClawOps does
#   4. compares against the live digest — identical means nothing to do,
#      and skipping saves every tenant on that breed a daemon restart
#   5. tars it up and PUTs it; ClawOps validates, swaps atomically, and
#      re-renders that breed's tenants
#
# Exit codes: 0 pushed or already current, 1 usage/validation, 2 rejected
# by the server, 3 pushed but some tenants failed to re-render.

set -euo pipefail

BREED=""
DIR=""
URL="${CLAWOPS_URL:-http://127.0.0.1:8088}"
TOKEN="${CLAWOPS_ADMIN_TOKEN:-}"
NO_REFRESH=0
FORCE=0
DRY_RUN=0

usage() {
  sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
  cat <<'USAGE'

Options:
  --breed <name>   Breed name, [a-z0-9_-]+. Required.
  --dir <path>     Template tree to push. Required.
  --url <url>      ClawOps base URL. Default $CLAWOPS_URL or 127.0.0.1:8088.
  --token <tok>    Admin token. Default $CLAWOPS_ADMIN_TOKEN.
  --no-refresh     Install without re-rendering existing tenants.
  --force          Push even when the digest already matches.
  --dry-run        Validate and print the digest; send nothing.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --breed) BREED="$2"; shift 2 ;;
    --dir) DIR="$2"; shift 2 ;;
    --url) URL="$2"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --no-refresh) NO_REFRESH=1; shift ;;
    --force) FORCE=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 1 ;;
  esac
done

die() { echo "ERROR: $*" >&2; exit 1; }

[[ -n "$BREED" ]] || die "--breed is required"
[[ -n "$DIR" ]] || die "--dir is required"
[[ -d "$DIR" ]] || die "--dir '$DIR' is not a directory"
[[ "$BREED" =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die "breed name must match [a-z0-9][a-z0-9_-]*"
URL="${URL%/}"

# ── 1. shape ────────────────────────────────────────────────────────
[[ -f "$DIR/config.toml.hbs" ]] || die \
  "$DIR has no config.toml.hbs — a breed is a *template* tree (.hbs files),
       not a rendered workspace. See docs/breed-sync.md."

# Files ClawOps never reads. Shipping them is harmless but they bloat the
# bundle and make digests churn on every local run. The same list drives
# both the tar and the digest — if the two ever diverged, the script
# would hash one tree and upload a different one, and the "already
# current" check would be wrong in both directions.
IGNORE_NAMES=('.git' '__pycache__' 'node_modules' '.DS_Store')
IGNORE_GLOBS=('*.pyc' '*.swp' '*~')

TAR_EXCLUDE=()
FIND_PRUNE=()
for n in "${IGNORE_NAMES[@]}" "${IGNORE_GLOBS[@]}"; do
  TAR_EXCLUDE+=(--exclude="$n")
  FIND_PRUNE+=(-name "$n" -prune -o)
done

# Relative paths of everything that will actually be sent.
bundle_files() {
  ( cd "$DIR" && find . "${FIND_PRUNE[@]}" -type f -printf '%P\n' )
}

# ── 2. secrets ──────────────────────────────────────────────────────
# A breed is rendered into every tenant's workspace, so anything
# hard-coded here reaches every tenant. Secrets belong in clawops.toml
# (which reaches the template as {{llm.api_key}} and friends) or in the
# systemd EnvironmentFile — never in the bundle.
scan_secrets() {
  local hits=0 f
  while IFS= read -r -d '' f; do
    # `{{...}}` placeholders are the correct form and must not trip this.
    local stripped
    stripped=$(sed 's/{{[^}]*}}//g' "$f")
    if grep -qE '(sk-[A-Za-z0-9]{16,}|zc_[0-9a-f]{32,}|enc2:)' <<<"$stripped"; then
      echo "  $f: looks like a hard-coded key or paired token" >&2
      hits=1
    fi
    if grep -qiE '^[[:space:]]*(api_key|password|secret|token)[[:space:]]*=[[:space:]]*"[^"]{8,}"' <<<"$stripped"; then
      echo "  $f: literal credential assignment" >&2
      hits=1
    fi
  done < <(find "$DIR" -type f \( -name '*.hbs' -o -name '*.toml' -o -name '*.md' \) -print0)
  return $hits
}
if ! scan_secrets; then
  die "secrets found in the bundle (above). Move them to clawops.toml and
       reference them as {{llm.api_key}} etc, or export them via the
       systemd EnvironmentFile. Refusing to push."
fi

# ── 3. digest ───────────────────────────────────────────────────────
# Must match crate::breeds::digest_of: sha256 over the sorted
# "<relpath>\0<sha256(content)>\n" manifest. LC_ALL=C sorts by byte, the
# same order a Rust BTreeMap<String, _> iterates in.
local_digest() {
  local rel
  bundle_files | LC_ALL=C sort | while IFS= read -r rel; do
    printf '%s\0%s\n' "$rel" "$(sha256sum "$DIR/$rel" | cut -d' ' -f1)"
  done | sha256sum | cut -d' ' -f1
}
DIGEST=$(local_digest)
FILES=$(bundle_files | wc -l | tr -d ' ')
echo "breed=$BREED files=$FILES digest=$DIGEST"

if [[ $DRY_RUN -eq 1 ]]; then
  echo "dry-run: nothing sent"
  exit 0
fi
[[ -n "$TOKEN" ]] || die "no admin token (--token or \$CLAWOPS_ADMIN_TOKEN)"

# ── 4. skip a no-op ─────────────────────────────────────────────────
if [[ $FORCE -eq 0 ]]; then
  REMOTE=$(curl -sf -H "X-Admin-Token: $TOKEN" "$URL/admin/breeds/$BREED" 2>/dev/null \
           | sed -n 's/.*"digest"[[:space:]]*:[[:space:]]*"\([0-9a-f]*\)".*/\1/p' | head -1 || true)
  if [[ "$REMOTE" == "$DIGEST" ]]; then
    echo "already current on $URL — nothing to push (use --force to override)"
    exit 0
  fi
  [[ -n "$REMOTE" ]] && echo "remote digest $REMOTE differs; pushing"
fi

# ── 5. push ─────────────────────────────────────────────────────────
QS=""
[[ $NO_REFRESH -eq 1 ]] && QS="?refresh=false"

BODY=$(mktemp); trap 'rm -f "$BODY"' EXIT
CODE=$(tar -C "$DIR" "${TAR_EXCLUDE[@]}" -czf - . \
  | curl -s -o "$BODY" -w '%{http_code}' \
      -X PUT --data-binary @- \
      -H "X-Admin-Token: $TOKEN" \
      -H 'Content-Type: application/gzip' \
      "$URL/admin/breeds/$BREED$QS")

if [[ "$CODE" != "200" ]]; then
  echo "push rejected (HTTP $CODE):" >&2
  cat "$BODY" >&2; echo >&2
  case "$CODE" in
    # The server's own message here is just "length limit exceeded",
    # which doesn't name the knob to turn.
    413) echo "       bundle is larger than [provisioner] max_bundle_bytes in clawops.toml" >&2 ;;
    503) echo "       ClawOps is in single-breed mode — set [provisioner] breeds_dir" >&2 ;;
  esac
  exit 2
fi

cat "$BODY"; echo
# The server reports per-tenant rollout failures rather than raising: the
# templates are already live by then. Surface them as a non-zero exit so
# a CI step or a deploy skill doesn't read this as a clean success.
if grep -q '"failures":\[[^]]' "$BODY"; then
  echo "WARNING: some tenants failed to re-render (see failures above)" >&2
  echo "         retry with: curl -X POST -H 'X-Admin-Token: …' $URL/admin/breeds/$BREED/refresh" >&2
  exit 3
fi
echo "OK — breed '$BREED' is live on $URL"
