#!/bin/sh
set -eu

PREFIX="${PREFIX:-/usr/local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="${CONFIG_DIR:-/etc/home-edge}"
STATE_DIR="${STATE_DIR:-/var/lib/home-edge}"
LOG_DIR="${LOG_DIR:-/var/log/home-edge}"
SERVICE_NAME="home-edge.service"

install -d "$BIN_DIR" "$CONFIG_DIR" "$STATE_DIR" "$LOG_DIR"
install -m 0755 ./home-edge "$BIN_DIR/home-edge"
install -m 0644 ./default.toml "$CONFIG_DIR/config.toml"

if [ -d /etc/systemd/system ]; then
    install -m 0644 ./home-edge.service "/etc/systemd/system/$SERVICE_NAME"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload
        systemctl enable "$SERVICE_NAME"
    fi
fi

echo "Installed home-edge to $BIN_DIR"
