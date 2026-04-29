#!/usr/bin/env bash
# stress_cleanup.sh — wipe every stress_* user the load tests created
#
# Stops daemons, deletes Linux users + their homes, clears DB rows.
# Run as root on the target server after stress_idle.sh /
# stress_concurrent.sh are done.
#
#   bash stress_cleanup.sh

set -euo pipefail

PREFIX="${PREFIX:-stress_}"
DB="${DB:-/var/lib/clawops/data/clawops.db}"

echo "→ cleaning users with openid prefix '$PREFIX'..."

n_db=$(sqlite3 "$DB" "SELECT COUNT(*) FROM users WHERE openid LIKE '${PREFIX}%';" 2>/dev/null || echo 0)
echo "  $n_db users in db"

mapfile -t uids < <(sqlite3 "$DB" \
    "SELECT linux_uid FROM users WHERE openid LIKE '${PREFIX}%';" 2>/dev/null || true)

for uid in "${uids[@]}"; do
    [[ -z "$uid" ]] && continue
    if id "$uid" >/dev/null 2>&1; then
        n_uid=$(id -u "$uid")
        echo "  stop $uid (uid=$n_uid)"
        sudo -u "$uid" env XDG_RUNTIME_DIR="/run/user/$n_uid" \
            systemctl --user stop "zeroclaw@${uid}" 2>/dev/null || true
        # Force-kill stragglers BEFORE userdel — otherwise userdel -r
        # silently fails when a process holds the home dir open, and
        # the home dir survives as a stale orphan after the DB row
        # gets deleted below. (See past stress runs producing 39 such
        # orphans; sweep_orphans.sh exists to clean those.)
        pkill -9 -u "$uid" 2>/dev/null || true
        sleep 0.2
        loginctl disable-linger "$uid" 2>/dev/null || true
        userdel -r "$uid" 2>/dev/null || true
        # belt-and-braces: if userdel -r still left the home dir, nuke it
        rm -rf "/home/$uid"
    fi
done

sqlite3 "$DB" <<SQL
DELETE FROM chat_messages    WHERE openid LIKE '${PREFIX}%';
DELETE FROM sessions         WHERE openid LIKE '${PREFIX}%';
DELETE FROM provision_log    WHERE openid LIKE '${PREFIX}%';
DELETE FROM port_allocations WHERE owner_openid LIKE '${PREFIX}%';
DELETE FROM users            WHERE openid LIKE '${PREFIX}%';
SQL

echo "→ done."
echo "  remaining users:"
sqlite3 "$DB" "SELECT COUNT(*), GROUP_CONCAT(SUBSTR(openid,1,12),' ') FROM users;" \
    | awk -F'|' '{printf "    count=%s  sample=%s\n",$1,$2}'
echo "  /home/claw-* dirs:"
ls /home/ | grep '^claw-' || echo "    none"
