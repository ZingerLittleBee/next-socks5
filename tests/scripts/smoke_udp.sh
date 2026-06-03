#!/usr/bin/env bash
#
# smoke_udp.sh -- end-to-end UDP ASSOCIATE smoke test for the next-socks5 proxy.
#
# Proves the SOCKS5 UDP relay works through the proxy against a LOCAL UDP echo
# server (no internet access required). curl cannot drive a raw UDP relay, so a
# small inline python3 SOCKS5 UDP client does the work:
#   - opens a TCP control connection, negotiates no-auth, sends UDP ASSOCIATE,
#   - parses the BND.ADDR/PORT from the reply,
#   - relays a datagram through the proxy to a local UDP echo server,
#   - verifies the echoed payload comes back.
#
# Configurable via environment variables (defaults in parentheses):
#   BIN   path to the server binary (target/debug/next-socks5)
#   PORT  proxy listen port        (11080)
#
# Exits 0 only if the UDP round-trip succeeds.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

BIN="${BIN:-target/debug/next-socks5}"
PORT="${PORT:-11080}"

PROXY_PID=""
TMP_DIR="$(mktemp -d)"

cleanup() {
    # Kill the proxy and wait on it so the shell does not print an
    # asynchronous "Terminated" job-control notice after we exit.
    if [[ -n "$PROXY_PID" ]]; then
        kill "$PROXY_PID" 2>/dev/null || true
        wait "$PROXY_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

echo "==> Building (cargo build)"
cargo build

echo "==> Starting no-auth proxy on 127.0.0.1:$PORT"
"$BIN" --no-tui --listen "127.0.0.1:$PORT" >"$TMP_DIR/proxy.log" 2>&1 &
PROXY_PID=$!
sleep 1
if ! kill -0 "$PROXY_PID" 2>/dev/null; then
    echo "proxy failed to start; log:"
    cat "$TMP_DIR/proxy.log"
    exit 1
fi

echo "==> Running python3 SOCKS5 UDP client"
PROXY_PORT="$PORT" python3 - <<'PY'
import os
import socket
import struct
import sys
import threading

PROXY_HOST = "127.0.0.1"
PROXY_PORT = int(os.environ["PROXY_PORT"])
PAYLOAD = b"hello"


def fail(msg):
    print(f"UDP FAIL: {msg}")
    sys.exit(1)


# 1. Local UDP echo server: receive one datagram and send it straight back.
echo_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
echo_sock.bind(("127.0.0.1", 0))
echo_ip, echo_port = echo_sock.getsockname()


def echo_loop():
    try:
        data, addr = echo_sock.recvfrom(65535)
        echo_sock.sendto(data, addr)
    except OSError:
        pass


threading.Thread(target=echo_loop, daemon=True).start()

# 2. TCP control connection: greeting + method negotiation (no-auth).
ctrl = socket.create_connection((PROXY_HOST, PROXY_PORT), timeout=3)
ctrl.sendall(b"\x05\x01\x00")
method_reply = ctrl.recv(2)
if method_reply != b"\x05\x00":
    fail(f"unexpected method reply: {method_reply!r}")

# 3. UDP ASSOCIATE request with a zero DST (RFC 1928 section 7).
ctrl.sendall(b"\x05\x03\x00\x01\x00\x00\x00\x00\x00\x00")
reply = ctrl.recv(10)
if len(reply) != 10 or reply[0] != 0x05 or reply[1] != 0x00:
    fail(f"unexpected ASSOCIATE reply: {reply!r}")

bnd_ip = socket.inet_ntoa(reply[4:8])
bnd_port = struct.unpack("!H", reply[8:10])[0]
# An unspecified BND.ADDR means "same host as the control connection".
if bnd_ip == "0.0.0.0":
    bnd_ip = "127.0.0.1"

# 4. Build a SOCKS5 UDP datagram targeting the local echo server and send it
#    from a fresh UDP socket. The TCP control socket MUST stay open for the
#    lifetime of the association.
udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
udp.settimeout(3)
header = b"\x00\x00\x00\x01" + socket.inet_aton(echo_ip) + struct.pack("!H", echo_port)
udp.sendto(header + PAYLOAD, (bnd_ip, bnd_port))

# 5. Read the relayed echo back and strip the 10-byte SOCKS UDP header.
try:
    data, _ = udp.recvfrom(65535)
except socket.timeout:
    fail("timed out waiting for relayed echo")

if len(data) < 10:
    fail(f"reply too short: {data!r}")

payload = data[10:]
if payload != PAYLOAD:
    fail(f"payload mismatch: got {payload!r}, want {PAYLOAD!r}")

ctrl.close()
print("UDP PASS")
sys.exit(0)
PY

echo
echo "smoke_udp: ALL PASSED"
