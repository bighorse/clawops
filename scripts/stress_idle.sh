#!/usr/bin/env bash
# stress_idle.sh — measure how many idle daemons one box can host
#
# Repeatedly provisions a new ClawOps user (which spawns a per-user
# zeroclaw daemon) and records system + per-daemon memory.  Stops at
# either MAX_USERS or when free memory drops below MEM_FLOOR_MB.
#
# Run as root on the target server. No LLM calls — costs nothing.
#
#   bash stress_idle.sh                    # default: N=200, step=5
#   MAX_USERS=300 STEP=10 bash stress_idle.sh
#
# Output: /tmp/stress-idle.csv  (also tail-printed live)

set -euo pipefail

ADMIN_TOKEN="${ADMIN_TOKEN:-}"
if [[ -z "$ADMIN_TOKEN" ]]; then
    ADMIN_TOKEN=$(awk -F'=' '/^token *=/ {gsub(/[" ]/,"",$2); print $2; exit}' \
                  /etc/clawops/clawops.toml || true)
fi
if [[ -z "$ADMIN_TOKEN" ]]; then
    echo "ADMIN_TOKEN env not set and could not parse /etc/clawops/clawops.toml" >&2
    exit 1
fi

BASE="${BASE:-http://127.0.0.1:8088}"
MAX_USERS="${MAX_USERS:-200}"
STEP="${STEP:-5}"
MEM_FLOOR_MB="${MEM_FLOOR_MB:-1500}"   # bail out when free mem dips below this
PREFIX="${PREFIX:-stress_}"
OUT="${OUT:-/tmp/stress-idle.csv}"

echo "iter,users,mem_used_mb,mem_free_mb,mem_avail_mb,daemons,daemon_avg_rss_mb,daemon_max_rss_mb,load_1m" > "$OUT"
echo "→ output: $OUT"
echo "→ ADMIN_TOKEN ok, BASE=$BASE, target $MAX_USERS users, step $STEP"

snapshot() {
    local n=$1
    # /proc/meminfo: MemTotal MemFree MemAvailable Buffers Cached ...
    local mem_total mem_free mem_avail mem_used
    mem_total=$(awk '/^MemTotal:/ {print int($2/1024)}' /proc/meminfo)
    mem_free=$(awk '/^MemFree:/ {print int($2/1024)}' /proc/meminfo)
    mem_avail=$(awk '/^MemAvailable:/ {print int($2/1024)}' /proc/meminfo)
    mem_used=$((mem_total - mem_avail))

    local pids
    pids=$(pgrep -f 'zeroclaw daemon' || true)
    local count=0 sum=0 max=0
    if [[ -n "$pids" ]]; then
        # ps RSS is in KB
        while read -r rss; do
            [[ -z "$rss" ]] && continue
            count=$((count + 1))
            sum=$((sum + rss))
            (( rss > max )) && max=$rss
        done < <(ps -o rss= -p $(echo "$pids" | tr '\n' ',' | sed 's/,$//') 2>/dev/null || true)
    fi
    local avg=0
    (( count > 0 )) && avg=$((sum / count / 1024))
    local maxmb=$((max / 1024))
    local load1m
    load1m=$(awk '{print $1}' /proc/loadavg)

    printf "%d,%d,%d,%d,%d,%d,%d,%d,%s\n" \
        "$n" "$n" "$mem_used" "$mem_free" "$mem_avail" "$count" "$avg" "$maxmb" "$load1m" \
        | tee -a "$OUT"

    if (( mem_avail < MEM_FLOOR_MB )); then
        echo "!! mem_available ${mem_avail}MB < floor ${MEM_FLOOR_MB}MB — stopping" >&2
        return 1
    fi
}

# baseline before any test users
echo "→ baseline (0 stress users)"
snapshot 0 || exit 0

for i in $(seq 1 "$MAX_USERS"); do
    openid="${PREFIX}${i}"
    code=$(curl -s -o /tmp/_provision.json -w '%{http_code}' \
        -X POST "$BASE/admin/provision" \
        -H "X-Admin-Token: $ADMIN_TOKEN" \
        -H 'Content-Type: application/json' \
        -d "{\"openid\":\"$openid\"}")
    if [[ "$code" != "200" ]]; then
        echo "!! provision $openid HTTP $code — $(head -c 200 /tmp/_provision.json)" >&2
        # often 409 (already exists) on rerun; fine, keep going
        if [[ "$code" != "409" ]]; then
            break
        fi
    fi

    if (( i % STEP == 0 )); then
        if ! snapshot "$i"; then
            echo "→ stopped at $i users"
            break
        fi
    fi
done

echo
echo "==== summary ===="
column -t -s, "$OUT"
