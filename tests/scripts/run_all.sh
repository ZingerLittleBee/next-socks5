#!/usr/bin/env bash
#
# run_all.sh -- full verification of the next-socks5 server.
#
# Runs, in order:
#   1. cargo test  (all unit + integration tests)
#   2. smoke_connect.sh  (end-to-end CONNECT smoke test)
#   3. smoke_udp.sh      (end-to-end UDP ASSOCIATE smoke test)
#
# Prints a final summary and exits non-zero if any step failed. All environment
# variables understood by the individual scripts (BIN, PORT, HTTP_PORT) are
# inherited.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

# Record each step's result for the final summary.
declare -a RESULTS
FAILED=0

run_step() {
    local name="$1"
    shift
    echo
    echo "============================================================"
    echo "==> $name"
    echo "============================================================"
    if "$@"; then
        RESULTS+=("PASS  $name")
    else
        RESULTS+=("FAIL  $name")
        FAILED=1
    fi
}

run_step "cargo test" cargo test
run_step "smoke_connect.sh" bash "$SCRIPT_DIR/smoke_connect.sh"
run_step "smoke_udp.sh" bash "$SCRIPT_DIR/smoke_udp.sh"

echo
echo "============================================================"
echo "Summary"
echo "============================================================"
for line in "${RESULTS[@]}"; do
    echo "  $line"
done

if [[ "$FAILED" -eq 0 ]]; then
    echo "ALL STEPS PASSED"
    exit 0
else
    echo "SOME STEPS FAILED"
    exit 1
fi
