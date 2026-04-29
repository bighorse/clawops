#!/usr/bin/env bash
# stress_concurrent.sh — measure RT under K concurrent chat users
#
# Picks K already-provisioned stress_* users, issues fresh tokens via
# /admin/issue-token, sends ONE simple chat per user concurrently,
# collects end-to-end response time and HTTP status.
#
# *** This DOES call the real LLM. Budget per chat ~0.01-0.05 RMB ***
# Default K=10 ≈ ¥0.50; K=30 ≈ ¥1.5; K=50 ≈ ¥2.5.
#
#   bash stress_concurrent.sh                  # default K=10
#   K=30 bash stress_concurrent.sh             # 30 concurrent
#   PROMPT="你好" K=20 bash stress_concurrent.sh
#
# Output: /tmp/stress-concurrent-K<K>.txt

set -euo pipefail

ADMIN_TOKEN="${ADMIN_TOKEN:-}"
if [[ -z "$ADMIN_TOKEN" ]]; then
    ADMIN_TOKEN=$(awk -F'=' '/^token *=/ {gsub(/[" ]/,"",$2); print $2; exit}' \
                  /etc/clawops/clawops.toml || true)
fi
[[ -z "$ADMIN_TOKEN" ]] && { echo "ADMIN_TOKEN missing" >&2; exit 1; }

BASE="${BASE:-http://127.0.0.1:8088}"
K="${K:-10}"
PROMPT="${PROMPT:-你好}"
PREFIX="${PREFIX:-stress_}"
OUT="${OUT:-/tmp/stress-concurrent-K${K}.txt}"
> "$OUT"

# baseline: how many stress_* users exist
EXISTING=$(sqlite3 /var/lib/clawops/data/clawops.db \
    "SELECT COUNT(*) FROM users WHERE openid LIKE '${PREFIX}%';" 2>/dev/null || echo 0)
if (( EXISTING < K )); then
    echo "Need K=$K users with prefix $PREFIX, found $EXISTING. Run stress_idle.sh first." >&2
    exit 1
fi

echo "→ K=$K concurrent users, prompt: \"$PROMPT\""
echo "→ output: $OUT"

# 1. Get K tokens (sequential, fast — issue-token is cheap)
echo "→ issuing $K tokens..."
TOKENS_FILE=$(mktemp)
for i in $(seq 1 "$K"); do
    openid="${PREFIX}${i}"
    tok=$(curl -s -X POST "$BASE/admin/issue-token" \
        -H "X-Admin-Token: $ADMIN_TOKEN" \
        -H 'Content-Type: application/json' \
        -d "{\"openid\":\"$openid\"}" \
        | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')
    echo "$openid $tok" >> "$TOKENS_FILE"
done

# 2. Mem snapshot before
echo "→ before-fire mem (MB):"
free -m | awk 'NR==2 {printf "  used=%s free=%s avail=%s\n",$3,$4,$7}'

# 3. Fire K parallel chats (xargs -P K)
echo "→ firing $K parallel /chat..."
START_NS=$(date +%s%N)
< "$TOKENS_FILE" \
    xargs -n2 -P "$K" -I{} bash -c '
        read -r openid tok <<<"$@"
        t0=$(date +%s%N)
        code=$(curl -s -o /tmp/_resp_${openid}.json -w "%{http_code}" \
            -X POST "$BASE/chat" \
            --max-time 180 \
            -H "Authorization: Bearer $tok" \
            -H "Content-Type: application/json" \
            -d "{\"content\":\"$PROMPT\"}")
        t1=$(date +%s%N)
        dur_ms=$(( (t1 - t0) / 1000000 ))
        bytes=$(wc -c < /tmp/_resp_${openid}.json)
        rm -f /tmp/_resp_${openid}.json
        printf "%s\t%s\t%dms\t%db\n" "$openid" "$code" "$dur_ms" "$bytes"
    ' _ {} | tee "$OUT"
END_NS=$(date +%s%N)
WALL_MS=$(( (END_NS - START_NS) / 1000000 ))

# 4. Mem snapshot after
echo
echo "→ after-fire mem (MB):"
free -m | awk 'NR==2 {printf "  used=%s free=%s avail=%s\n",$3,$4,$7}'

# 5. Stats
echo
echo "==== summary K=$K ===="
echo "wall clock total: ${WALL_MS} ms"

awk -F'\t' '
    /^stress_/ {
        code=$2; ms=$3; sub(/ms/,"",ms);
        all[NR]=ms+0
        codes[code]++
    }
    END {
        n = length(all)
        if (n == 0) { print "no rows"; exit }
        # sort
        asort(all)
        printf "n=%d  min=%d  p50=%d  p95=%d  max=%d (ms)\n", n, all[1], all[int(n*0.5)+1], all[int(n*0.95)+1], all[n]
        printf "http codes: "
        for (c in codes) printf "%s=%d  ", c, codes[c]
        print ""
    }
' "$OUT"

rm -f "$TOKENS_FILE"
