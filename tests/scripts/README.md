# Smoke-test scripts

Self-contained, reusable smoke tests for the `next-socks5` SOCKS5 server. They
require **no internet access**: every target is a local server started by the
script itself, and all background processes are cleaned up on exit.

## Scripts

| Script | What it does |
| --- | --- |
| `smoke_connect.sh` | Builds the server, then proves SOCKS5 **CONNECT** works end-to-end through the proxy against a local `python3 -m http.server`. Covers a no-auth proxy plus a password-auth proxy (correct credentials succeed, wrong credentials are rejected). |
| `smoke_udp.sh` | Builds the server, then proves SOCKS5 **UDP ASSOCIATE** relay works against a local UDP echo server, driven by a small inline `python3` SOCKS5 UDP client (a datagram is relayed through the proxy and the echo is verified). |
| `run_all.sh` | Runs `cargo test`, then `smoke_connect.sh`, then `smoke_udp.sh`, and prints a PASS/FAIL summary. Exits non-zero if any step fails. |

## Running

```sh
bash tests/scripts/smoke_connect.sh
bash tests/scripts/smoke_udp.sh
bash tests/scripts/run_all.sh
```

The scripts are executable, so `./tests/scripts/run_all.sh` also works. They can
be invoked from any working directory; the repository root is resolved from the
script location.

## Environment variables

All have sensible defaults, so no configuration is needed for a normal run.

| Variable | Default | Used by | Meaning |
| --- | --- | --- | --- |
| `BIN` | `target/debug/next-socks5` | all | Path to the server binary. |
| `PORT` | `11080` | all | Proxy listen port. |
| `HTTP_PORT` | `18081` | `smoke_connect.sh` | Local HTTP server port. |

## Requirements

- `cargo` (to build the binary)
- `curl` with SOCKS5 support (`--socks5`)
- `python3`

## Notes

- SOCKS5 credentials are passed to `curl` with `--proxy-user user:pass`
  alongside `--socks5`, which is the most portable form across `curl` builds.
