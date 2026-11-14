#!/usr/bin/env bash
# Rebuild and relaunch good-query with a throwaway config (dev only).
#   dev/restart.sh                      config in target/dev-config, opens dev/sample-project
#   GQ_CONFIG_DIR=/path/to/conf dev/restart.sh
set -e
export PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")/.."
mkdir -p target/dev-config
taskkill //IM good-query.exe //F >/dev/null 2>&1 || true
cargo build -p gq-app 2>&1 | grep -E "^(error|warning: unused)" -A7 || true
test -f target/debug/good-query.exe

# Default: a git-ignored config that opens the sample project.
if [ -z "${GQ_CONFIG_DIR:-}" ]; then
    GQ_CONFIG_DIR="$(cygpath -w "$PWD/target/dev-config")"

    if [ ! -f target/dev-config/workspace.toml ]; then
        printf "open = ['%s']\n" "$(cygpath -w "$PWD/dev/sample-project")" > target/dev-config/workspace.toml
    fi
fi
export GQ_CONFIG_DIR
GQ_DEV_PASSWORD="${GQ_DEV_PASSWORD:-postgres}" ./target/debug/good-query.exe > "${GQ_LOG:-target/dev-config/app.log}" 2>&1 &
sleep "${GQ_WAIT:-4}"
