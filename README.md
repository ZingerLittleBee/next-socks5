# next-socks5

[![build](https://github.com/zinger-labs/next-socks5/actions/workflows/build.yml/badge.svg)](https://github.com/zinger-labs/next-socks5/actions/workflows/build.yml)

A lightweight, scalable **SOCKS5 server** written in Rust (RFC 1928 + RFC 1929),
with a live terminal dashboard and a headless mode for containers. The protocol
is hand-written; the dependency footprint is kept deliberately small.

## Features

- **SOCKS5 commands** — `CONNECT` and `UDP ASSOCIATE` (RFC 1928). `BIND` is
  rejected with reply code `0x07` by design.
- **Authentication** — No-Auth (`0x00`) and Username/Password (`0x02`, RFC 1929).
- **Address types** — IPv4, IPv6, and Domain (`ATYP` `0x01` / `0x04` / `0x03`),
  with server-side DNS resolution for both CONNECT and UDP targets.
- **Full RFC error mapping** — every reply code `0x00`–`0x08` is produced where
  applicable (e.g. unknown command → `0x07`, unknown address type → `0x08`,
  connection limit → `0x02`, refused/unreachable/timeout mapped from the OS).
- **UDP relay** — SOCKS5 encapsulation, `FRAG != 0` dropped, source-IP
  filtering, a client-reachable `BND.ADDR` (never `0.0.0.0`), and idle reclaim.
- **TUI dashboard** — real-time throughput, active-connection table, success/
  error stats, and a scrolling log (built on ratatui).
- **Headless mode** — `--no-tui` streams events to stdout, ideal for systemd /
  containers. The TUI is an optional cargo feature, so headless builds drop the
  ratatui/crossterm dependencies entirely.
- **Robustness** — connect / TCP-idle / UDP-idle timeouts, optional
  `max_connections` limit, half-open-aware relay, and graceful shutdown.
- **Configuration** — TOML file with CLI overrides.
- **Small & portable** — pure Rust, no C dependencies; ships as fully static
  musl binaries and a ~2 MB `scratch`-based container image.

## Installation

### Option 1 — One-line installer (recommended)

The installer picks **binary** or **docker**, generates credentials and a free
port automatically, and starts the service.

```bash
# Binary install, auth enabled with auto-generated user/password, random port:
curl -fsSL https://raw.githubusercontent.com/zinger-labs/next-socks5/main/install.sh | sh

# With options (note the `-s --` to pass args through curl | sh):
curl -fsSL https://raw.githubusercontent.com/zinger-labs/next-socks5/main/install.sh \
  | sh -s -- --method docker --auth --port 1080
```

Or clone and run locally:

```bash
./install.sh --help
./install.sh --method binary --no-auth --port 1080
./install.sh --method docker --auth --user alice --pass secret --port 1080
```

| Flag | Description | Default |
|---|---|---|
| `--method <binary\|docker>` | Native binary (systemd/OpenRC) or Docker Compose | `binary` |
| `--auth` / `--no-auth` | Enable username/password auth, or run open | `--auth` |
| `--user` / `--pass` | Credentials for auth mode (random if omitted) | random |
| `--port <port>` | Listen port (random free port if omitted) | random |
| `--listen <addr>` | Bind address | `0.0.0.0` |
| `--version <tag>` | Release version, e.g. `v0.1.0` | `latest` |
| `--no-service` | Install binary + config only; don't set up/start a service | off |

> Binary install targets Linux (musl x86_64 / aarch64) and sets up a **systemd**
> or **OpenRC** service. If neither init system is present, the binary and config
> are installed but **not started** (and won't auto-start on reboot) — start it
> manually or use `--method docker` for a self-restarting container. The
> installer is POSIX `sh` (no bash required).

### Option 2 — Docker

```bash
# No-auth, host networking (UDP ASSOCIATE works), listening on 1080:
docker run -d --name next-socks5 --network host \
  ghcr.io/zinger-labs/next-socks5:latest --listen 0.0.0.0:1080
```

With a config file (for auth):

```bash
docker run -d --name next-socks5 --network host \
  -v "$PWD/config.toml:/etc/next-socks5/config.toml:ro" \
  ghcr.io/zinger-labs/next-socks5:latest --config /etc/next-socks5/config.toml
```

Or with Compose (`docker-compose.yml`):

```yaml
services:
  next-socks5:
    image: ghcr.io/zinger-labs/next-socks5:latest
    container_name: next-socks5
    restart: unless-stopped
    network_mode: host
    volumes:
      - ./config.toml:/etc/next-socks5/config.toml:ro
    command: ["--config", "/etc/next-socks5/config.toml"]
```

```bash
docker compose up -d
```

Images are multi-arch (`linux/amd64`, `linux/arm64`) and tagged with both the
release version (e.g. `0.1.0`) and `latest`. The container always runs headless.

### Option 3 — Prebuilt binaries

Download a static musl build from the
[Releases](https://github.com/zinger-labs/next-socks5/releases) page:

```bash
curl -fL -o next-socks5.tar.gz \
  https://github.com/zinger-labs/next-socks5/releases/latest/download/next-socks5-x86_64-unknown-linux-musl.tar.gz
tar xzf next-socks5.tar.gz
./next-socks5-x86_64-unknown-linux-musl/next-socks5 --no-tui --listen 0.0.0.0:1080
```

(Replace `x86_64` with `aarch64` for ARM64.)

### Option 4 — Build from source

Requires a recent stable Rust toolchain.

```bash
git clone https://github.com/zinger-labs/next-socks5
cd next-socks5
cargo build --release
./target/release/next-socks5            # TUI dashboard
./target/release/next-socks5 --no-tui   # headless

# Headless-only build (drops the TUI deps):
cargo build --release --no-default-features
```

Or install straight from git:

```bash
cargo install --git https://github.com/zinger-labs/next-socks5
```

## Configuration

Configuration is a TOML file (see [`config.example.toml`](config.example.toml));
CLI flags override file values.

```toml
listen = "0.0.0.0:1080"

[auth]
method = "password"        # "none" | "password"
# One or more credentials — add a [[auth.users]] block per user.
[[auth.users]]
username = "alice"
password = "secret"

[[auth.users]]
username = "bob"
password = "hunter2"

[timeouts]
connect_ms = 10000
tcp_idle_ms = 300000
udp_idle_ms = 60000

[limits]
max_connections = 1024     # optional

[admin]
enabled = true             # local attach endpoint (default on)
# socket = "/run/next-socks5/admin.sock"   # override the socket path
```

**Multiple users.** With `method = "password"`, add a `[[auth.users]]` block per
credential — a client is accepted if its username/password matches **any** entry
in the list (RFC 1929). This is the recommended way to serve several users from a
single port; you do not need a separate port per user. With `method = "none"` the
proxy is open and the `users` list is ignored. (The dashboard logs each auth
attempt as `auth ok/failed for '<user>'`; per-user traffic accounting is not yet
shown in the connections table.)

### CLI

```
next-socks5 [OPTIONS]              Run the server (default)
next-socks5 attach [OPTIONS]       Attach to a running server's dashboard

Server options:
  --config <path>       Path to a TOML config file
  --listen <addr>       Override the listen address (e.g. 0.0.0.0:1080)
  --no-tui              Run headless (events to stdout) instead of the dashboard
  --no-admin            Disable the local admin/attach endpoint
  --admin-socket <path> Override the admin socket path
  -h, --help            Print help

attach options:
  --socket <path>       Admin socket to connect to
                        (default /run/next-socks5/admin.sock)
```

## Usage

```bash
# Test a no-auth proxy:
curl --socks5 127.0.0.1:1080 https://example.com

# Test a password-authenticated proxy:
curl --socks5 alice:secret@127.0.0.1:1080 https://example.com
```

### Dashboard (TUI)

The terminal dashboard is on by default — just run the server without
`--no-tui`, and press `q` (or Ctrl-C) to quit:

```bash
next-socks5 --listen 127.0.0.1:1080
```

### Attach to a running service

A service installed via systemd / OpenRC / Docker runs **headless** (no UI of
its own), but it still serves the live dashboard over a local Unix socket
(default `/run/next-socks5/admin.sock`). To watch a server that is **already
running**, attach to it from the same machine — there is nothing to restart and
no flag to enable; the endpoint is on by default.

```bash
# 1. SSH into the host where the service runs (as root for the default socket):
ssh root@your-server

# 2. Attach — default socket /run/next-socks5/admin.sock:
next-socks5 attach

# Docker: run attach inside the container instead:
docker exec -it next-socks5 next-socks5 attach
```

If the service uses a non-default socket path (e.g. a manual install on a custom
path), point `--socket` at it:

```bash
next-socks5 attach --socket /tmp/ns5.sock
```

The endpoint is local-only (no network exposure, no auth) and read-only — attach
clients observe but cannot control the server. Press `q` to detach; if the
server stops, the dashboard exits with `connection lost`.

> The default socket lives under `/run/next-socks5` (mode `0710`, owned by the
> service user). `root` can always attach; a non-root user can only attach to a
> socket it owns (e.g. a manual install under `/tmp` or `$XDG_RUNTIME_DIR`).

Disable the endpoint with `--no-admin` or `[admin] enabled = false`.

For a manual install (`--no-service`), the process runs as your user and the
default `/run` path is usually not writable. Start the server with a writable
socket and attach to the same path:

```bash
next-socks5 --no-tui --admin-socket /tmp/ns5.sock
next-socks5 attach --socket /tmp/ns5.sock
```

## License

See [LICENSE](LICENSE).
