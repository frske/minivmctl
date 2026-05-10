#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CERT="$ROOT/certs/localhost.pem"
KEY="$ROOT/certs/localhost-key.pem"
BACKEND_BIN="$ROOT/target/debug/scratch-new"
ROCKET_CONFIG="$ROOT/Rocket.toml"
FRONTEND_PORT="5173"

backend_pid=""
frontend_pid=""

cleanup() {
    set +e

    if [[ -n "$backend_pid" ]]; then
        sudo kill "$backend_pid" >/dev/null 2>&1 || true
    fi

    if [[ -n "$frontend_pid" ]]; then
        kill "$frontend_pid" >/dev/null 2>&1 || true
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
        ./scratch-new
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
