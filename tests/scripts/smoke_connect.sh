#!/usr/bin/env bash
#
# smoke_connect.sh -- end-to-end CONNECT smoke test for the next-socks5 proxy.
#
# Proves the SOCKS5 CONNECT command works through the proxy against a LOCAL
# HTTP server (no internet access required), covering:
#   1. No-auth proxy: curl through the proxy succeeds.
#   2. Password-auth proxy: correct credentials succeed; wrong ones are
#      rejected (curl fails).
#
# Configurable via environment variables (defaults in parentheses):
#   BIN        path to the server binary (target/debug/next-socks5)
#   PORT       proxy listen port        (11080)
#   HTTP_PORT  local HTTP server port   (18081)
#
# Exits 0 only if every assertion passes.

set -euo pipefail

# Resolve the repository root from this script's location so the test can be
# run from any working directory.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

BIN="${BIN:-target/debug/next-socks5}"
PORT="${PORT:-11080}"
HTTP_PORT="${HTTP_PORT:-18081}"

# Track background PIDs and the temp dir so the EXIT trap can clean everything.
HTTP_PID=""
PROXY_PID=""
TMP_DIR="$(mktemp -d)"

cleanup() {
    # Kill background helpers and wait on them so the shell does not print
    # asynchronous "Terminated" job-control notices after we exit.
    if [[ -n "$PROXY_PID" ]]; then
        kill "$PROXY_PID" 2>/dev/null || true
        wait "$PROXY_PID" 2>/dev/null || true
    fi
    if [[ -n "$HTTP_PID" ]]; then
        kill "$HTTP_PID" 2>/dev/null || true
        wait "$HTTP_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

FAILED=0
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1"; FAILED=1; }

# Start the proxy in the given mode and wait until it is ready to accept
# connections (it prints a "listening" banner in headless mode).
start_proxy() {
    "$@" >"$TMP_DIR/proxy.log" 2>&1 &
    PROXY_PID=$!
    sleep 1
    if ! kill -0 "$PROXY_PID" 2>/dev/null; then
        echo "proxy failed to start; log:"
        cat "$TMP_DIR/proxy.log"
        return 1
    fi
}

stop_proxy() {
    [[ -n "$PROXY_PID" ]] && kill "$PROXY_PID" 2>/dev/null || true
    [[ -n "$PROXY_PID" ]] && wait "$PROXY_PID" 2>/dev/null || true
    PROXY_PID=""
}

echo "==> Building (cargo build)"
cargo build

# Serve a known file from the temp dir so the HTTP server returns content.
echo "smoke-connect-ok" >"$TMP_DIR/index.html"

echo "==> Starting local HTTP server on 127.0.0.1:$HTTP_PORT"
( cd "$TMP_DIR" && exec python3 -m http.server "$HTTP_PORT" --bind 127.0.0.1 ) \
    >"$TMP_DIR/http.log" 2>&1 &
HTTP_PID=$!
sleep 1
if ! kill -0 "$HTTP_PID" 2>/dev/null; then
    echo "local HTTP server failed to start; log:"
    cat "$TMP_DIR/http.log"
    exit 1
fi

# ---------------------------------------------------------------------------
# Case 1: no-auth proxy, CONNECT should succeed.
# ---------------------------------------------------------------------------
echo "==> Case 1: no-auth CONNECT"
start_proxy "$BIN" --no-tui --listen "127.0.0.1:$PORT"
if curl --fail --silent --show-error \
        --socks5 "127.0.0.1:$PORT" "http://127.0.0.1:$HTTP_PORT/"; then
    pass "no-auth CONNECT succeeded"
else
    fail "no-auth CONNECT failed"
fi
stop_proxy

# ---------------------------------------------------------------------------
# Case 2: password-auth proxy.
# ---------------------------------------------------------------------------
echo "==> Case 2: password-auth CONNECT"
CONFIG="$TMP_DIR/auth.toml"
cat >"$CONFIG" <<EOF
listen = "127.0.0.1:$PORT"

[auth]
method = "password"
[[auth.users]]
username = "alice"
password = "secret"
EOF

start_proxy "$BIN" --no-tui --config "$CONFIG"

# Correct credentials -> success.
# Use --proxy-user with --socks5 rather than the user:pass@host form: it is the
# most portable way to pass SOCKS5 credentials across curl builds.
if curl --fail --silent --show-error \
        --proxy-user "alice:secret" \
        --socks5 "127.0.0.1:$PORT" "http://127.0.0.1:$HTTP_PORT/"; then
    pass "password-auth with correct credentials succeeded"
else
    fail "password-auth with correct credentials failed"
fi

# Wrong credentials -> expect failure (curl non-zero exit). We must NOT use
# --fail here in a way that masks the assertion: we explicitly invert the exit
# status so a curl failure is the success condition.
if curl --fail --silent \
        --proxy-user "alice:wrong" \
        --socks5 "127.0.0.1:$PORT" "http://127.0.0.1:$HTTP_PORT/" >/dev/null 2>&1; then
    fail "password-auth with wrong credentials unexpectedly succeeded"
else
    pass "password-auth with wrong credentials was rejected"
fi
stop_proxy

echo
if [[ "$FAILED" -eq 0 ]]; then
    echo "smoke_connect: ALL PASSED"
    exit 0
else
    echo "smoke_connect: FAILURES DETECTED"
    exit 1
fi
