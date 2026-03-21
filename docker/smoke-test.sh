#!/bin/sh
set -eu

binary="${1:-/opt/pi-control-plane/pi-control-plane}"
config="${2:-/opt/pi-control-plane/default.toml}"

"$binary" --config "$config" &
pid="$!"
trap 'kill "$pid"' EXIT INT TERM

sleep 1
wget -qO- http://127.0.0.1:8124/api/health | grep '"status":"ok"'
