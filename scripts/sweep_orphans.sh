#!/usr/bin/env bash
# sweep_orphans.sh — remove stale /home/claw-* directories whose
# Linux user / home dir lives on but no longer corresponds to any DB
# row in clawops users table. Caused by past stress_cleanup runs
# where `userdel -r` silently failed (process holding home open,
# `|| true` swallowing the error) but the DB row got deleted anyway.
#
# Run as root. Idempotent. DB-driven: only removes uids NOT in the
# users table, so any real production user is safe.

set -euo pipefail

DB="${DB:-/var/lib/clawops/data/clawops.db}"
db_uids=$(sqlite3 "$DB" "SELECT linux_uid FROM users" 2>/dev/null || true)

removed=0
for u in $(ls /home 2>/dev/null | grep '^claw-' || true); do
    # if the uid is in the DB, it's a real user — skip
    if echo "$db_uids" | grep -qx "$u"; then
        continue
    fi

    echo "→ orphan: $u"
    n_uid=$(id -u "$u" 2>/dev/null || true)

    if [[ -n "$n_uid" ]]; then
        # try graceful stop first
        sudo -u "$u" env XDG_RUNTIME_DIR="/run/user/$n_uid" \
            systemctl --user stop "zeroclaw@${u}" 2>/dev/null || true
        # force-kill any straggler owned by this uid (the cause of
        # userdel -r failure in the first place)
        pkill -9 -u "$u" 2>/dev/null || true
        sleep 0.2
        loginctl disable-linger "$u" 2>/dev/null || true
        userdel "$u" 2>/dev/null || true
    fi

    rm -rf "/home/$u"
    removed=$((removed + 1))
done

echo
echo "done — $removed orphans swept"
echo "  /home/claw-* now:"
ls /home/ | grep '^claw-' | head -10 || echo "    (none)"
echo "  DB user count:"
sqlite3 "$DB" "SELECT COUNT(*), GROUP_CONCAT(linux_uid,' ') FROM users;" \
    | awk -F'|' '{printf "    count=%s  uids=%s\n",$1,$2}'
