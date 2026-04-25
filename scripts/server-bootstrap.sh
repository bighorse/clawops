#!/usr/bin/env bash
# ClawOps server bootstrap for Ubuntu 22.04.
# Run as root on a freshly-provisioned host.
#
#   bash server-bootstrap.sh
#
# Idempotent — safe to re-run.
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root" >&2
    exit 1
fi

if ! grep -q 'Ubuntu 22' /etc/os-release; then
    echo "warning: this script targets Ubuntu 22.04; current OS:"
    cat /etc/os-release
    read -r -p "continue anyway? [y/N] " ans
    [[ "$ans" =~ ^[Yy]$ ]] || exit 1
fi

echo "==> apt update && upgrade"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get upgrade -y -q

echo "==> base packages"
apt-get install -y -q \
    build-essential pkg-config libssl-dev cmake git curl jq sqlite3 \
    fail2ban ufw \
    ca-certificates

echo "==> rustup (system-wide for the build user)"
if ! command -v rustc >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi

echo "==> ufw firewall (allow ssh + clawops)"
ufw --force reset
ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp comment 'ssh'
ufw allow 8088/tcp comment 'clawops'
ufw --force enable
ufw status verbose

echo "==> fail2ban for sshd"
cat > /etc/fail2ban/jail.d/sshd.local <<'EOF'
[sshd]
enabled = true
port = 22
maxretry = 5
findtime = 600
bantime = 3600
EOF
systemctl enable --now fail2ban
systemctl restart fail2ban

echo "==> sshd hardening (require key auth, disable root password)"
sshd_cfg=/etc/ssh/sshd_config
sed -i.bak \
    -e 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' \
    -e 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' \
    -e 's/^#\?ChallengeResponseAuthentication.*/ChallengeResponseAuthentication no/' \
    "$sshd_cfg"
echo
echo "  IMPORTANT: before reloading sshd, make sure root has an authorized_keys entry."
echo "  If you are still on a password session, do NOT run the next line until you've"
echo "  added your public key to /root/.ssh/authorized_keys (or have console access)."
echo "    systemctl reload sshd"
echo

echo "==> clawops directories"
mkdir -p /etc/clawops /var/lib/clawops /var/log/clawops
mkdir -p /var/lib/clawops/data

echo "==> systemd user-template unit"
install -m 0644 \
    "$(dirname "$0")/../systemd/zeroclaw@.service" \
    /etc/systemd/user/zeroclaw@.service
systemctl daemon-reload

echo
echo "==> bootstrap done."
echo "Next steps:"
echo "  1. Add your SSH pubkey to /root/.ssh/authorized_keys, then run:"
echo "       systemctl reload sshd"
echo "  2. Build zeroclaw and clawops binaries (cargo build --release on this host"
echo "     OR scp prebuilt binaries to /usr/local/bin/)."
echo "  3. Copy clawops.toml to /etc/clawops/clawops.toml (use clawops.example.toml as base)."
echo "  4. Put shared LLM env in /etc/clawops/zeroclaw.env, e.g.:"
echo "       echo 'ZEROCLAW_API_KEY=sk-xxx' > /etc/clawops/zeroclaw.env"
echo "       chmod 600 /etc/clawops/zeroclaw.env"
echo "  5. Start clawops as root for now:"
echo "       /usr/local/bin/clawops --config /etc/clawops/clawops.toml serve"
