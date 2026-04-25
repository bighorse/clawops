#!/usr/bin/env bash
# Build and install zeroclaw + clawops binaries on the server.
# Assumes server-bootstrap.sh has already been run (rust toolchain present).
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "must run as root" >&2
    exit 1
fi

# Source code locations on the server.
ZEROCLAW_SRC=${ZEROCLAW_SRC:-/opt/zeroclaw}
CLAWOPS_SRC=${CLAWOPS_SRC:-/opt/clawops}

# shellcheck disable=SC1091
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"

if [[ ! -d "$ZEROCLAW_SRC" ]]; then
    echo "==> cloning zeroclaw to $ZEROCLAW_SRC"
    git clone https://github.com/bighorse/zeroclaw.git "$ZEROCLAW_SRC"
else
    echo "==> updating zeroclaw"
    git -C "$ZEROCLAW_SRC" fetch --quiet && git -C "$ZEROCLAW_SRC" reset --hard origin/master
fi

echo "==> building zeroclaw (release, this takes 5-10 minutes the first time)"
cd "$ZEROCLAW_SRC"
cargo build --release
install -m 0755 target/release/zeroclaw /usr/local/bin/zeroclaw
/usr/local/bin/zeroclaw --version

if [[ ! -d "$CLAWOPS_SRC" ]]; then
    echo "==> set CLAWOPS_SRC to a path containing your clawops source, e.g.:"
    echo "      CLAWOPS_SRC=/opt/clawops $0"
    echo "    or scp it from your dev machine first."
    exit 1
fi

echo "==> building clawops"
cd "$CLAWOPS_SRC"
cargo build --release
install -m 0755 target/release/clawops /usr/local/bin/clawops
install -m 0644 -D systemd/zeroclaw@.service /etc/systemd/user/zeroclaw@.service
mkdir -p /etc/clawops/templates
cp -r templates/workspace /etc/clawops/templates/
systemctl daemon-reload

echo
echo "==> install done."
echo "Edit /etc/clawops/clawops.toml so 'provisioner.template_dir' points to /etc/clawops/templates/workspace"
echo "    and 'database.url' points to /var/lib/clawops/data/clawops.db"
echo "    and 'provisioner.backend = systemd'."
