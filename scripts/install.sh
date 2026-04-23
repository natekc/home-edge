#!/bin/sh
# First-time installation script for home-edge.
#
# Run on the device after extracting the release tarball:
#   tar -xzf home-edge-<target>.tar.gz -C /tmp/install
#   sudo sh /tmp/install/install.sh
#
# Idempotent: safe to re-run.  For upgrades use upgrade.sh instead.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

PREFIX="${PREFIX:-/usr/local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="${CONFIG_DIR:-/etc/home-edge}"
STATE_DIR="${STATE_DIR:-/var/lib/home-edge}"
LOG_DIR="${LOG_DIR:-/var/log/home-edge}"
SERVICE_USER="home-edge"
SERVICE_NAME="home-edge.service"

if [ "$(id -u)" -ne 0 ]; then
    echo "error: install.sh must run as root (try: sudo sh install.sh)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Create dedicated system user (no login shell, no home directory)
# ---------------------------------------------------------------------------
if ! id "$SERVICE_USER" >/dev/null 2>&1; then
    echo "Creating system user: $SERVICE_USER"
    useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER" 2>/dev/null || \
    adduser -S -H -s /sbin/nologin "$SERVICE_USER" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# Directories and binary
# ---------------------------------------------------------------------------
install -d -o root    -g root         -m 0755 "$BIN_DIR"
install -d -o root    -g root         -m 0755 "$CONFIG_DIR"
install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 "$STATE_DIR"
install -d -o "$SERVICE_USER" -g "$SERVICE_USER" -m 0750 "$LOG_DIR"

install -m 0755 "${SCRIPT_DIR}/home-edge"    "$BIN_DIR/home-edge"

# Only write config if not already present (preserve operator customisations)
if [ ! -f "${CONFIG_DIR}/config.toml" ]; then
    install -m 0644 "${SCRIPT_DIR}/default.toml" "$CONFIG_DIR/config.toml"
    echo "Config written to ${CONFIG_DIR}/config.toml — edit before starting the service."
else
    echo "Config already exists at ${CONFIG_DIR}/config.toml — not overwritten."
fi

# ---------------------------------------------------------------------------
# systemd unit
# ---------------------------------------------------------------------------
if [ -d /etc/systemd/system ]; then
    install -m 0644 "${SCRIPT_DIR}/home-edge.service" "/etc/systemd/system/$SERVICE_NAME"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload
        systemctl enable "$SERVICE_NAME"
        echo "Service enabled. Start with: systemctl start $SERVICE_NAME"
    fi
fi

echo "Installed home-edge to $BIN_DIR/home-edge"
