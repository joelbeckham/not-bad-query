#!/usr/bin/env bash
# Rebuild and relaunch good-query with a throwaway config (dev only).
#   GQ_CONFIG_DIR=/path/to/conf dev/restart.sh
set -e
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."
taskkill //IM good-query.exe //F >/dev/null 2>&1 || true
cargo build -p gq-app 2>&1 | grep -E "^(error|warning: unused)" -A7 || true
test -f target/debug/good-query.exe
: "${GQ_CONFIG_DIR:?set GQ_CONFIG_DIR}"
GQ_DEV_PASSWORD="${GQ_DEV_PASSWORD:-postgres}" ./target/debug/good-query.exe > "${GQ_LOG:-/dev/null}" 2>&1 &
sleep "${GQ_WAIT:-4}"
