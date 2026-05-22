#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CERT="$ROOT/certs/localhost.pem"
KEY="$ROOT/certs/localhost-key.pem"
BACKEND_BIN="$ROOT/target/debug/minivmctl"
ROCKET_CONFIG="$ROOT/Rocket.toml"
FRONTEND_PORT="5173"
BACKEND_STOP_WAIT_SECONDS="35"
FRONTEND_STOP_WAIT_SECONDS="2"

backend_pid=""
frontend_pid=""

stop_process() {
    local pid="$1"
    local use_sudo="$2"
    local wait_seconds="$3"
    local attempts=$((wait_seconds * 10))

    if ! kill -0 "$pid" >/dev/null 2>&1; then
        return
    fi

    if [[ "$use_sudo" == "yes" ]]; then
        sudo kill -TERM "$pid" >/dev/null 2>&1 || true
    else
        kill -TERM "$pid" >/dev/null 2>&1 || true
    fi

    for ((i = 0; i < attempts; i++)); do
        if ! kill -0 "$pid" >/dev/null 2>&1; then
            return
        fi
        sleep 0.1
    done

    if [[ "$use_sudo" == "yes" ]]; then
        sudo kill -KILL "$pid" >/dev/null 2>&1 || true
    else
        kill -KILL "$pid" >/dev/null 2>&1 || true
    fi
}

cleanup() {
    set +e

    if [[ -n "$backend_pid" ]]; then
        printf 'waiting up to %ss for backend shutdown...\n' "$BACKEND_STOP_WAIT_SECONDS"
        stop_process "$backend_pid" yes "$BACKEND_STOP_WAIT_SECONDS"
    fi

    if [[ -n "$frontend_pid" ]]; then
        stop_process "$frontend_pid" no "$FRONTEND_STOP_WAIT_SECONDS"
    fi
}

require_file() {
    local path="$1"
    local hint="$2"

    if [[ ! -f "$path" ]]; then
        printf 'missing %s\n%s\n' "$path" "$hint" >&2
        exit 1
    fi
}

require_file "$CERT" "generate certificates before starting dev"
require_file "$KEY" "generate certificates before starting dev"
require_file "$ROCKET_CONFIG" "Rocket.toml is required for the HTTPS API"

command -v python3 >/dev/null 2>&1 || {
    printf 'missing python3\n' >&2
    exit 1
}

printf 'building backend...\n'
cargo build --manifest-path "$ROOT/Cargo.toml"
require_file "$BACKEND_BIN" "cargo build did not create the expected backend binary"

trap cleanup EXIT INT TERM

printf 'checking sudo access for backend...\n'
sudo -v

printf 'starting backend: https://127.0.0.1:8443\n'
(
    cd "$ROOT/target/debug"
    exec sudo env \
        ROCKET_CONFIG="$ROCKET_CONFIG" \
        ROCKET_TLS_CERTS="$CERT" \
        ROCKET_TLS_KEY="$KEY" \
        ./minivmctl
) &
backend_pid="$!"

printf 'starting frontend: https://127.0.0.1:%s\n' "$FRONTEND_PORT"
(
    exec python3 -m http.server "$FRONTEND_PORT" \
        --bind 127.0.0.1 \
        --directory "$ROOT/frontend" \
        --tls-cert "$CERT" \
        --tls-key "$KEY"
) &
frontend_pid="$!"

printf '\nfrontend: https://127.0.0.1:%s\n' "$FRONTEND_PORT"
printf 'api:      https://127.0.0.1:8443\n'
printf 'press Ctrl-C to stop both processes\n\n'

wait -n "$backend_pid" "$frontend_pid"
