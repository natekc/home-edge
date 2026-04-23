#!/bin/sh
# Atomic upgrade script for home-edge.
#
# Run on the device after extracting the release tarball:
#   tar -xzf home-edge-<target>.tar.gz -C /tmp/update
#   sh /tmp/update/upgrade.sh
#
# What it does:
#   1. Stop the service (gracefully, with a timeout)
#   2. Back up the current binary to <bin>.bak
#   3. Install the new binary atomically (via a temp file + rename)
#   4. Optionally install an updated systemd unit if it changed
#   5. Restart the service
#
# Rollback:
#   systemctl stop home-edge
#   cp /usr/local/bin/home-edge.bak /usr/local/bin/home-edge
#   systemctl start home-edge
# Or via the Makefile:
#   make rollback HOST=pi@raspberrypi.local

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

PREFIX="${PREFIX:-/usr/local}"
BIN="${PREFIX}/bin/home-edge"
CONFIG_DIR="${CONFIG_DIR:-/etc/home-edge}"
STATE_DIR="${STATE_DIR:-/var/lib/home-edge}"
LOG_DIR="${LOG_DIR:-/var/log/home-edge}"
SERVICE_NAME="home-edge"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
NEW_BIN="${SCRIPT_DIR}/home-edge"
NEW_SERVICE="${SCRIPT_DIR}/home-edge.service"

need_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "error: upgrade.sh must run as root (try: sudo sh upgrade.sh)" >&2
        exit 1
    fi
}

# ---------------------------------------------------------------------------
# First-time install: delegate to install.sh if the binary doesn't exist yet.
# ---------------------------------------------------------------------------
if [ ! -f "$BIN" ]; then
    echo "Binary not found at $BIN — running first-time install..."
    need_root
    sh "${SCRIPT_DIR}/install.sh"
    exit 0
fi

need_root

# ---------------------------------------------------------------------------
# Stop the service
# ---------------------------------------------------------------------------
if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet "${SERVICE_NAME}"; then
    echo "Stopping ${SERVICE_NAME}..."
    systemctl stop "${SERVICE_NAME}"
    STOPPED=1
else
    STOPPED=0
fi

# ---------------------------------------------------------------------------
# Atomic binary swap
# ---------------------------------------------------------------------------
echo "Installing new binary..."
# Keep one generation of backup for rollback
cp -f "$BIN" "${BIN}.bak"
# Write to a temp file on the same filesystem, then rename for atomicity
install -m 0755 "$NEW_BIN" "${BIN}.new"
mv -f "${BIN}.new" "$BIN"
echo "Binary installed: $BIN  (previous saved as ${BIN}.bak)"

# ---------------------------------------------------------------------------
# Update systemd unit if it differs (don't overwrite on every upgrade unless changed)
# ---------------------------------------------------------------------------
if [ -f "$NEW_SERVICE" ] && [ -d /etc/systemd/system ]; then
    if ! cmp -s "$NEW_SERVICE" "$SERVICE_FILE" 2>/dev/null; then
        echo "Updating systemd unit..."
        install -m 0644 "$NEW_SERVICE" "$SERVICE_FILE"
        systemctl daemon-reload
    fi
fi

# ---------------------------------------------------------------------------
# Restart
# ---------------------------------------------------------------------------
if [ "$STOPPED" -eq 1 ] || systemctl is-enabled --quiet "${SERVICE_NAME}" 2>/dev/null; then
    echo "Starting ${SERVICE_NAME}..."
    systemctl start "${SERVICE_NAME}"
    echo "Done. Service status:"
    systemctl status "${SERVICE_NAME}" --no-pager -l || true
else
    echo "Done. (Service was not running before upgrade; start it with: systemctl start ${SERVICE_NAME})"
fi
