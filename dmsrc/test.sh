#!/bin/bash

# Runs the standalone harness against a server of its own and fails when any of its checks did, unlike run.sh,
# which is for watching it against whatever server is already up.
set -euo pipefail
cd "$(dirname "$0")/.."

for tool in DreamMaker DreamDaemon; do
    if ! command -v "$tool" >/dev/null; then
        echo "$tool is not on PATH; set up BYOND first" >&2
        exit 1
    fi
done

# Before 516.1688 a proc that calls one asleep on a byond,await call loses the rest of its body, which breaks every awaited export.
required=516.1688
version=$(DreamDaemon -version 2>&1 | grep -oE 'Version [0-9]+\.[0-9]+' | head -n 1 | cut -d' ' -f2)
if [[ -z "$version" || $(printf '%s\n' "$required" "$version" | sort -V | head -n 1) != "$required" ]]; then
    echo "DreamDaemon is BYOND ${version:-of unknown version}, the harness needs $required or later" >&2
    exit 1
fi

cargo build-client
cargo build-server
DreamMaker dmsrc/sada.dme

scratch=$(mktemp -d)
export SADA_TEST_SOCKET="$scratch/control.sock"

cat >"$scratch/config.toml" <<CONFIG
[server]
listen = "127.0.0.1:0"
control_socket = "$SADA_TEST_SOCKET"

[routing]
policy = "proximity"
CONFIG

SADA_CONFIG="$scratch/config.toml" target/debug/sada-server >"$scratch/server.log" 2>&1 &
server=$!

stop_server() {
    if kill -0 "$server" 2>/dev/null; then
        kill -INT "$server"
        for _ in $(seq 1 50); do
            kill -0 "$server" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$server" 2>/dev/null || true
    fi
    wait "$server" 2>/dev/null || true
    rm -rf "$scratch"
}
trap stop_server EXIT

for _ in $(seq 1 100); do
    [[ -S "$SADA_TEST_SOCKET" ]] && break
    if ! kill -0 "$server" 2>/dev/null; then
        cat "$scratch/server.log" >&2
        echo "the server exited before it was listening" >&2
        exit 1
    fi
    sleep 0.1
done

if [[ ! -S "$SADA_TEST_SOCKET" ]]; then
    cat "$scratch/server.log" >&2
    echo "the server never started listening on $SADA_TEST_SOCKET" >&2
    exit 1
fi

# A world that runtimes before reaching its own Del() never exits.
output=$(timeout 120 DreamDaemon dmsrc/sada.dmb -trusted 2>&1) || true
echo "$output"

if grep -qE "FAIL|init failed|runtime error" <<<"$output"; then
    exit 1
fi

# The last line test.dm logs: anything that stopped short of it did not run every check.
grep -q "no control errors" <<<"$output"
