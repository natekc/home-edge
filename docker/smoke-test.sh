#!/bin/sh
set -eu

binary="${1:-/opt/home-edge/home-edge}"
config="${2:-/opt/home-edge/default.toml}"

"$binary" --config "$config" &
pid="$!"
trap 'kill "$pid"' EXIT INT TERM

sleep 1
wget -qO- http://127.0.0.1:8124/api/health | grep '"status":"ok"'
