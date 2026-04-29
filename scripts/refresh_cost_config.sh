#!/usr/bin/env bash
# refresh_cost_config.sh — upgrade [cost] section in every existing
# per-user config.toml without re-provisioning (preserves paired_token,
# port allocation, and chat history).
#
# Run as root after a config.toml.hbs change to roll the change to all
# already-running daemons. Idempotent: skips users already on the
# new schema (detected via presence of `daily_limit_usd` key).
#
#   bash refresh_cost_config.sh

set -euo pipefail

read -r -d '' NEW_COST_BLOCK <<'EOT' || true
[cost]
enabled = true
daily_limit_usd = 0.7
monthly_limit_usd = 15.0
warn_at_percent = 80
allow_override = false

[cost.prices."qwen3.6-flash"]
input = 0.169
output = 1.014

[cost.prices."qwen3.6-plus"]
input = 0.282
output = 1.690
EOT

updated=0
for u in $(ls /home | grep '^claw-'); do
    cfg="/home/$u/.zeroclaw/config.toml"
    if [[ ! -f "$cfg" ]]; then
        continue
    fi

    # No idempotent skip: always replace [cost] from the latest
    # template. Pricing/limits change too often for a marker check
    # to work; backup the old config so rollback is one cp away.
    cp -p "$cfg" "${cfg}.bak.$(date +%Y%m%d%H%M%S)"

    # Strip the existing [cost] section (and any [cost.*] sub-tables)
    # from the file, then append the new block at end. awk passes
    # everything except cost-related lines through; we add the new
    # block explicitly.
    awk -v new="$NEW_COST_BLOCK" '
        BEGIN { in_cost=0 }
        /^\[cost\]$/ || /^\[cost\./ { in_cost=1; next }
        /^\[/ && !/^\[cost/ { in_cost=0 }
        !in_cost { print }
        END { print ""; print new }
    ' "$cfg" > "${cfg}.tmp"
    mv "${cfg}.tmp" "$cfg"
    chown "$u:$u" "$cfg"
    chmod 600 "$cfg"

    UID_N=$(id -u "$u")
    sudo -u "$u" env XDG_RUNTIME_DIR="/run/user/$UID_N" \
        systemctl --user restart "zeroclaw@$u" 2>/dev/null || \
        echo "  warn: failed to restart zeroclaw@$u"

    updated=$((updated + 1))
    echo "updated $u (daemon restarted)"
done

echo
echo "done — updated=$updated users"
echo "rollback per-user: cp /home/<uid>/.zeroclaw/config.toml.bak.<latest> /home/<uid>/.zeroclaw/config.toml"
