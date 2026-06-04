#!/usr/bin/env bash
#
# bench.sh -- a self-contained, zero-extra-deps performance harness for
# next-socks5, using only bash + curl + python3 (no proxychains / iperf / Go).
#
# Through the SOCKS5 proxy, on a SINGLE host (loopback), it measures the three
# headline KPIs from RFC 3511 / RFC 9411, for both no-auth and password auth:
#   1. Throughput -- parallel large-file downloads (aggregate MB/s) + a direct
#      (no-proxy) baseline so you can see the proxy's relay overhead.
#   2. Latency    -- per-request total time for a small object (p50/p95/p99).
#   3. CPS        -- connection-establishment rate: every request is a fresh
#      SOCKS5 handshake (RFC 3511 5.3), the metric that dominates a proxy.
#
# IMPORTANT CAVEATS (this is a first-order sanity check, NOT an RFC-grade run):
#   * Single-host loopback OVERSTATES throughput (no real NIC) and UNDERSTATES
#     CPS (curl's per-process spawn cost, not the proxy, is the ceiling). For
#     real numbers use TWO machines and a Go/Rust load client (see the guide).
#   * curl is the only mainstream tool with NATIVE SOCKS5; that is why it is used
#     here. It tests TCP CONNECT only (no UDP ASSOCIATE).
#   * The generated config RELAXES [egress] (which blocks loopback/private by
#     default) so the loopback upstream is reachable. NEVER ship this config.
#
# Env knobs (defaults in parentheses):
#   BIN(target/release/next-socks5) PORT(11080) UPSTREAM_PORT(18080)
#   BIG_MB(200) SMALL_BYTES(1024) THR_PAR(8) LAT_REQS(1000) CPS_REQS(20000) CONC(200)

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

BIN="${BIN:-target/release/next-socks5}"
PORT="${PORT:-11080}"
UPSTREAM_PORT="${UPSTREAM_PORT:-18080}"
BIG_MB="${BIG_MB:-200}"
SMALL_BYTES="${SMALL_BYTES:-1024}"
THR_PAR="${THR_PAR:-8}"
LAT_REQS="${LAT_REQS:-1000}"
CPS_REQS="${CPS_REQS:-20000}"
CONC="${CONC:-200}"

TMP="$(mktemp -d)"
HTTP_PID=""
PROXY_PID=""
cleanup() {
    [[ -n "$PROXY_PID" ]] && kill "$PROXY_PID" 2>/dev/null
    [[ -n "$HTTP_PID" ]] && kill "$HTTP_PID" 2>/dev/null
    wait 2>/dev/null
    rm -rf "$TMP"
}
trap cleanup EXIT

now() { python3 -c 'import time; print(time.time())'; }
U="http://127.0.0.1:$UPSTREAM_PORT"

echo "==> build release"
cargo build --release >/dev/null 2>&1 || { echo "build failed"; exit 1; }

echo "==> upstream: ${BIG_MB} MiB + ${SMALL_BYTES} B on 127.0.0.1:${UPSTREAM_PORT}"
head -c "$((BIG_MB * 1024 * 1024))" /dev/zero >"$TMP/big.bin"
head -c "$SMALL_BYTES" /dev/zero >"$TMP/small.bin"
( cd "$TMP" && exec python3 -m http.server "$UPSTREAM_PORT" --bind 127.0.0.1 ) >/dev/null 2>&1 &
HTTP_PID=$!
sleep 1

# Build a bench config for the given auth mode (0/1). Egress is relaxed so the
# loopback upstream is reachable.
mk_cfg() {
    local mode="$1" f="$TMP/cfg_$1.toml"
    {
        echo "listen = \"127.0.0.1:$PORT\""
        echo "[egress]"
        echo "block_loopback = false"
        echo "block_link_local = false"
        echo "block_private = false"
        if [[ "$mode" == "1" ]]; then
            echo "[auth]"
            echo "method = \"password\""
            echo "[[auth.users]]"
            echo "username = \"alice\""
            echo "password = \"secret\""
        fi
    } >"$f"
    echo "$f"
}

# curl SOCKS5 args for the given auth mode.
pxargs() {
    if [[ "$1" == "1" ]]; then
        printf -- '--socks5-hostname 127.0.0.1:%s --proxy-user alice:secret' "$PORT"
    else
        printf -- '--socks5-hostname 127.0.0.1:%s' "$PORT"
    fi
}

start_proxy() {
    "$BIN" serve --no-tui --no-admin --config "$1" >"$TMP/proxy.log" 2>&1 &
    PROXY_PID=$!
    sleep 1
    kill -0 "$PROXY_PID" 2>/dev/null || { echo "proxy failed to start:"; cat "$TMP/proxy.log"; exit 1; }
}
stop_proxy() {
    [[ -n "$PROXY_PID" ]] && kill "$PROXY_PID" 2>/dev/null
    wait "$PROXY_PID" 2>/dev/null
    PROXY_PID=""
}

# Sum per-stream curl speed_download (B/s) across THR_PAR concurrent downloads.
throughput() { # $1 = extra curl args ("" for direct baseline)
    seq "$THR_PAR" | xargs -P "$THR_PAR" -I{} \
        curl -s -o /dev/null -w '%{speed_download}\n' $1 "$U/big.bin" \
    | awk '{s+=$1} END{ if(NR)printf "%.1f MB/s aggregate / %d streams (%.1f MB/s each)\n", s/1048576, NR, (s/NR)/1048576 }'
}

latency() { # $1 = curl proxy args
    local i
    for ((i = 0; i < LAT_REQS; i++)); do
        curl -s -o /dev/null -w '%{time_total}\n' $1 "$U/small.bin"
    done | awk '{print $1*1000}' | sort -n | awk '
        {v[NR]=$1; s+=$1}
        END{ n=NR; if(!n){print "no samples"; exit}
             i50=int(n*0.50); if(i50<1)i50=1; i95=int(n*0.95); if(i95<1)i95=1; i99=int(n*0.99); if(i99<1)i99=1;
             printf "p50=%.2f  p95=%.2f  p99=%.2f  max=%.2f  avg=%.2f ms (n=%d)\n",
                    v[i50], v[i95], v[i99], v[n], s/n, n }'
}

cps() { # $1 = curl proxy args
    local codes="$TMP/codes.txt" t0 t1 ok el
    : >"$codes"
    t0="$(now)"
    seq "$CPS_REQS" | xargs -P "$CONC" -I{} \
        curl -s -o /dev/null -w '%{http_code}\n' $1 "$U/small.bin" >>"$codes" 2>/dev/null
    t1="$(now)"
    ok="$(grep -c '^200$' "$codes" || true)"
    el="$(awk "BEGIN{print $t1-$t0}")"
    awk "BEGIN{ printf \"%.0f conn/s  (%d/%d ok, conc=%d, %.1fs)\n\", $ok/$el, $ok, $CPS_REQS, $CONC, $el }"
}

echo
echo "==> direct baseline (no proxy)"
printf '  throughput: '; throughput ""

for mode in 0 1; do
    label="no-auth"; [[ "$mode" == "1" ]] && label="password-auth"
    cfg="$(mk_cfg "$mode")"
    start_proxy "$cfg"
    echo
    echo "==> via proxy [$label]"
    printf '  throughput: '; throughput "$(pxargs "$mode")"
    printf '  latency:    '; latency "$(pxargs "$mode")"
    printf '  CPS:        '; cps "$(pxargs "$mode")"
    stop_proxy
done
echo
echo "done. Reminder: loopback numbers are indicative only — see the guide for a two-host, Go-client run."
