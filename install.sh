#!/bin/sh
# Build orbit-rs + orbit-net and install them + systemd units on the appliance.
# Safe to re-run: rebuilds, reinstalls, restarts either way -- never overwrites
# an existing config file.
#
# orbit-kms (renderer) and orbit-web (settings panel) are separate repos with
# their own install.sh; orbit-rs spawns/curls them but does not build them.
set -eu

PREFIX="${PREFIX:-/usr/local}"
CONF_DIR="${CONF_DIR:-/data/orbit/config}"
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BIN="$PREFIX/bin/orbit-rs"
NETBIN="$PREFIX/bin/orbit-net"

command -v cargo >/dev/null 2>&1 || {
    echo "[orbit-rs] cargo not found -- install Rust first (apt install cargo, or rustup)" >&2
    exit 1
}

echo "[orbit-rs] building (release)..."
( cd "$HERE" && cargo build --release )

echo "[orbit-rs] installing binaries -> $PREFIX/bin"
sudo install -m755 "$HERE/target/release/orbit-rs" "$BIN"
sudo install -m755 "$HERE/scripts/orbit-net" "$NETBIN"

echo "[orbit-rs] seeding config -> $CONF_DIR (existing files are left alone)"
sudo mkdir -p "$CONF_DIR"
for f in orbit-rs.env network.conf; do
    if [ -f "$CONF_DIR/$f" ]; then
        echo "            $f already exists, kept as-is"
    else
        sudo install -m644 "$HERE/config/$f" "$CONF_DIR/$f"
        echo "            $f seeded from the packaged default"
    fi
done

if [ ! -d /data ]; then
    echo "[orbit-rs] warning: /data does not exist on this system -- config will land" >&2
    echo "           on the root filesystem. Mount the appliance's /data partition first," >&2
    echo "           or override with CONF_DIR=..." >&2
fi

echo "[orbit-rs] installing systemd units"
# Substitute this run's actual PREFIX/CONF_DIR into the units, so overriding
# them above does not silently diverge from the installed unit's defaults.
sed -e "s#^ExecStart=.*#ExecStart=$BIN#" \
    "$HERE/systemd/orbit-rs.service" | sudo tee /etc/systemd/system/orbit-rs.service >/dev/null

sed -e "s#^ExecStart=.*#ExecStart=$NETBIN apply#" \
    -e "s#^ExecReload=.*#ExecReload=$NETBIN apply#" \
    "$HERE/systemd/orbit-net.service" | sudo tee /etc/systemd/system/orbit-net.service >/dev/null

sed -e "s#^PathChanged=.*#PathChanged=$CONF_DIR/network.conf#" \
    "$HERE/systemd/orbit-net.path" | sudo tee /etc/systemd/system/orbit-net.path >/dev/null

sudo systemctl daemon-reload
sudo systemctl enable orbit-net orbit-net.path orbit-rs >/dev/null
sudo systemctl restart orbit-net orbit-rs   # picks up rebuilt binaries even if already running

echo
echo "[orbit-rs] up."
echo "           logs:      journalctl -fu orbit-rs -u orbit-net"
echo "           network:   orbit-net status"
echo "           config:    $CONF_DIR/orbit-rs.env, $CONF_DIR/network.conf"
echo
echo "           Still needed for a working feed + panel (separate repos):"
echo "             orbit-kms  https://github.com/planktonwhc/orbit-kms  (renderer)"
echo "             orbit-web  https://github.com/planktonwhc/orbit-web (settings panel)"
