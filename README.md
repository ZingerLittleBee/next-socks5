# next-socks5

**English** | [简体中文](README.zh-CN.md)

[![Build](https://img.shields.io/github/actions/workflow/status/ZingerLittleBee/next-socks5/build.yml?style=for-the-badge&cacheSeconds=3600)](https://github.com/ZingerLittleBee/next-socks5/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/ZingerLittleBee/next-socks5?style=for-the-badge&cacheSeconds=3600)](https://github.com/ZingerLittleBee/next-socks5/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/ZingerLittleBee/next-socks5/total?style=for-the-badge&cacheSeconds=3600)](https://github.com/ZingerLittleBee/next-socks5/releases)
[![Container](https://img.shields.io/badge/ghcr.io-next--socks5-2496ED?logo=docker&logoColor=white&style=for-the-badge)](https://github.com/ZingerLittleBee/next-socks5/pkgs/container/next-socks5)
[![License](https://img.shields.io/github/license/ZingerLittleBee/next-socks5?style=for-the-badge&cacheSeconds=3600)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built_with-Rust-000000?logo=rust&logoColor=white&style=for-the-badge)](https://www.rust-lang.org)

A lightweight, scalable **SOCKS5 server** written in Rust (RFC 1928 + RFC 1929),
with a live terminal dashboard and a headless mode for containers. The protocol
is hand-written; the dependency footprint is kept deliberately small.

![next-socks5 dashboard](snapshot.gif)

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
- **TUI dashboard** — real-time throughput with a trend chart, a sortable
  active-connection table, success/error stats, and a scrollable log with
  keyboard navigation (built on ratatui). A hidden `--mock` flag streams
  synthetic data for previewing/testing the UI without real traffic.
- **Headless mode** — `--no-tui` streams events to stdout, ideal for systemd /
  containers. The TUI is an optional cargo feature, so headless builds drop the
  ratatui/crossterm dependencies entirely.
- **Robustness** — connect / TCP-idle / UDP-idle timeouts, optional
  `max_connections` limit, half-open-aware relay, and graceful shutdown.
- **Configuration** — TOML file with CLI overrides.
- **Small & portable** — pure Rust, no C dependencies; ships as fully static
  musl binaries and a ~3.5 MB `scratch`-based container image.

## Installation

### Option 1 — One-line installer (recommended)

The installer picks **binary** or **docker**, generates credentials and a free
port automatically, and starts the service.

```bash
# Binary install, auth enabled with auto-generated user/password, random port:
curl -fsSL https://raw.githubusercontent.com/ZingerLittleBee/next-socks5/main/install.sh | sh

# With options (note the `-s --` to pass args through curl | sh):
curl -fsSL https://raw.githubusercontent.com/ZingerLittleBee/next-socks5/main/install.sh \
  | sh -s -- --port 1080
```

Or clone and run locally. Each example is annotated below; in auth mode with no
`--user` / `--pass`, the installer generates a username and a 20-character
password and prints them at the end, together with a ready-to-use `socks5://`
URL and a `curl` test command.

```bash
# Show every flag and exit
./install.sh --help

# Simplest run: binary install, auth ON (auto-generated user/password), random free port
./install.sh

# Docker instead of a native binary (host networking, so UDP ASSOCIATE works)
./install.sh --method docker

# Open proxy (no auth) on a fixed port — only on a trusted network
./install.sh --method binary --no-auth --port 1080

# Explicit credentials on a fixed port
./install.sh --method docker --auth --user alice --pass secret --port 1080

# Bind to a single interface instead of 0.0.0.0 (here: localhost only)
./install.sh --no-auth --listen 127.0.0.1 --port 1080

# Pin a specific release instead of `latest`
./install.sh --version v0.2.0 --port 1080

# Install the binary + config only — do NOT create or start a service
./install.sh --no-service --port 1080            # same as: NO_SERVICE=1 ./install.sh --port 1080

# Custom location: binary install dir (binary) / compose deploy dir (docker)
./install.sh --bin-dir /opt/bin --port 1080
./install.sh --method docker --dir ./ns5 --port 1080
```

| Flag | Description | Default |
|---|---|---|
| `--method <binary\|docker>` | Native binary (systemd/OpenRC) or Docker Compose | `binary` |
| `--auth` / `--no-auth` | Enable username/password auth, or run open | `--auth` |
| `--user` / `--pass` | Credentials for auth mode (random if omitted) | random |
| `--port <port>` | Listen port (random free port if omitted) | random |
| `--listen <addr>` | Bind address | `0.0.0.0` |
| `--version <tag>` | Release version, e.g. `v0.1.0` | `latest` |
| `--bin-dir <dir>` | Binary install directory (binary method) | `/usr/local/bin` |
| `--dir <dir>` | Docker deploy directory (docker method) | `./next-socks5-deploy` |
| `--no-service` | Install binary + config only; don't set up/start a service | off |

> Binary install targets Linux (musl x86_64 / aarch64) and sets up a **systemd**
> or **OpenRC** service. If neither init system is present, the binary and config
> are installed but **not started** (and won't auto-start on reboot) — start it
> manually or use `--method docker` for a self-restarting container. The
> installer is POSIX `sh` (no bash required).

### Option 2 — Docker

Fastest — let the installer generate `docker-compose.yml` + `config.toml` and
start the container for you (host networking; with `--auth` and no `--user` /
`--pass`, credentials are auto-generated and printed at the end):

```bash
curl -fsSL https://raw.githubusercontent.com/ZingerLittleBee/next-socks5/main/install.sh \
  | sh -s -- --method docker --auth --port 1080
```

This writes both files into `./next-socks5-deploy/` (override with `--dir`) and
runs `docker compose up -d`. To wire it up manually instead:

```bash
# No-auth, host networking (UDP ASSOCIATE works), listening on 1080:
docker run -d --name next-socks5 --network host \
  ghcr.io/zingerlittlebee/next-socks5:latest --listen 0.0.0.0:1080
```

With a config file (for auth):

```bash
docker run -d --name next-socks5 --network host \
  -v "$PWD/config.toml:/etc/next-socks5/config.toml:ro" \
  ghcr.io/zingerlittlebee/next-socks5:latest --config /etc/next-socks5/config.toml
```

Or with Compose (`docker-compose.yml`):

```yaml
services:
  next-socks5:
    image: ghcr.io/zingerlittlebee/next-socks5:latest
    container_name: next-socks5
    restart: unless-stopped
    network_mode: host
    volumes:
      - ./config.toml:/etc/next-socks5/config.toml:ro
    # Writable runtime dir for the admin/attach socket — the image runs as an
    # unprivileged user that can't create /run/next-socks5 on its own. Without
    # this, `docker exec ... next-socks5 attach` cannot connect.
    tmpfs:
      - /run/next-socks5
    command: ["--config", "/etc/next-socks5/config.toml"]
```

```bash
docker compose up -d
```

Images are multi-arch (`linux/amd64`, `linux/arm64`) and tagged with both the
release version (e.g. `0.1.0`) and `latest`. The container always runs headless.

### Option 3 — Prebuilt binaries

Download a static musl build from the
[Releases](https://github.com/ZingerLittleBee/next-socks5/releases) page:

```bash
curl -fL -o next-socks5.tar.gz \
  https://github.com/ZingerLittleBee/next-socks5/releases/latest/download/next-socks5-x86_64-unknown-linux-musl.tar.gz
tar xzf next-socks5.tar.gz
./next-socks5-x86_64-unknown-linux-musl/next-socks5 serve --no-tui --listen 0.0.0.0:1080
```

(Replace `x86_64` with `aarch64` for ARM64.)

### Option 4 — Build from source

Requires a recent stable Rust toolchain.

```bash
git clone https://github.com/ZingerLittleBee/next-socks5
cd next-socks5
cargo build --release
./target/release/next-socks5 serve            # TUI dashboard
./target/release/next-socks5 serve --no-tui   # headless

# Headless-only build (drops the TUI deps):
cargo build --release --no-default-features
```

Or install straight from git:

```bash
cargo install --git https://github.com/ZingerLittleBee/next-socks5
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
handshake_ms = 10000       # greeting+auth+request deadline (anti-slowloris)
connect_ms = 10000
tcp_idle_ms = 300000
udp_idle_ms = 60000

[limits]
max_connections = 2048     # optional: global concurrent cap (unbounded if unset)
max_per_ip = 64            # optional: per-source-IP concurrent cap (unbounded if unset)

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

**Connection limits.** Both caps under `[limits]` are **optional and unbounded by
default**; the server enforces them at accept time, so half-open/handshaking
connections count too. They are not set automatically — opt in via the config:

- `max_connections` — global cap on concurrent connections; a backstop against
  file-descriptor / task exhaustion. Size it to your host (the OS `RLIMIT_NOFILE`
  is the ultimate ceiling; each CONNECT relay uses ~2 fds).
- `max_per_ip` — concurrent connections from a single source IP. Stops one client
  from monopolizing the proxy or brute-forcing credentials at high concurrency. A
  generous value (e.g. 64–256) does not affect normal clients; lower it only if you
  do not expect many users behind a single NAT.

For an **internet-facing** deployment, set both. The proxy has no built-in auth
rate-limiting, so also front the listen port with a host firewall / fail2ban when
it is publicly exposed.

**Secure defaults.** Egress filtering is **on by default**: the proxy refuses to
relay to loopback, link-local (including the `169.254.169.254` cloud-metadata
address), and private/RFC1918 ranges (an SSRF / open-relay guard). If you genuinely
need to reach internal targets, relax it with an `[egress]` section — see
[`config.example.toml`](config.example.toml). The pre-relay handshake is bounded by
`timeouts.handshake_ms` (default 10s) to drop slowloris-style stalled clients.

### CLI

```
next-socks5                        Print help (a bare invocation never starts a server)
next-socks5 serve [OPTIONS]        Run the server (alias: run)
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
`--no-tui`:

```bash
next-socks5 serve --listen 127.0.0.1:1080
```

It shows live throughput (with a 30s trend chart), success/error stats, a
sortable **Active connections** table, and a scrolling **Log**. Keys:

| Key | Action |
|---|---|
| `Tab` | Move scroll focus between the connections table and the log (focused panel is highlighted) |
| `s` | Cycle the connection sort key: `ID` → `UP↓` → `DOWN↓` → `AGE↓` (shown in the table title) |
| `↑` / `↓` or `k` / `j` | Scroll the focused panel one line |
| `PgUp` / `PgDn` | Scroll the focused panel one screen |
| `q` / `Ctrl-C` | Quit |

#### Preview / test the dashboard with synthetic data

To exercise the dashboard without sending any real traffic, add `--mock`. It
drives the same metrics and event bus the proxy uses with a stream of synthetic
connections, throughput, and errors — handy for trying the sorting/scrolling
keys or taking screenshots. The fake activity stops as soon as you quit.

```bash
# Local preview: open the dashboard and continuously generate mock data.
cargo run --release -- serve --listen 127.0.0.1:1080 --mock

# Or with an installed binary:
next-socks5 serve --listen 127.0.0.1:1080 --mock
```

`--mock` is a demo/testing aid only; never enable it on a real proxy.

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
>
> **Docker:** the container runs as an unprivileged user (uid `65534`) and needs a
> writable `/run/next-socks5` for the admin socket. The installer's generated
> Compose (and the example above) provide it via `tmpfs`; a bare `docker run`
> needs `--tmpfs /run/next-socks5`. Without it the server logs
> `admin endpoint disabled: Permission denied` and `attach` cannot connect.

Disable the endpoint with `--no-admin` or `[admin] enabled = false`.

For a manual install (`--no-service`), the process runs as your user and the
default `/run` path is usually not writable. Start the server with a writable
socket and attach to the same path:

```bash
next-socks5 serve --no-tui --admin-socket /tmp/ns5.sock
next-socks5 attach --socket /tmp/ns5.sock
```

## Performance

On a single 4-core cloud VM (loopback), next-socks5 relays at **~2 GB/s** with
**~1.6 ms** of added per-request latency and **~6k new connections/s**, and
profiling shows the proxy is kernel/network-bound with no lock contention — i.e.
the proxy itself is not the bottleneck. See
[`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) for the methodology, the reproducible
harness ([`tests/scripts/`](tests/scripts/)), and full numbers.

## License

See [LICENSE](LICENSE).
