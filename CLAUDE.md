# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`next-socks5` — a lightweight async SOCKS5 server in Rust (binary `next-socks5`, lib crate `next_socks5`). Supports CONNECT and UDP ASSOCIATE (RFC 1928 / 1929), optional username/password auth, an opt-in local admin socket, and a built-in ratatui dashboard. Ships as fully-static musl binaries (x86_64 / aarch64) and a ~2 MB scratch-based Docker image.

## Commands

```bash
cargo build --release                          # release build (TUI included by default)
cargo build --release --no-default-features    # headless-only build (drops ratatui + crossterm)
cargo run --release -- serve --listen 127.0.0.1:1080        # run with TUI dashboard
cargo run --release -- serve --no-tui                       # run headless (events to stdout)
cargo run --release -- serve --listen 127.0.0.1:1080 --mock # demo dashboard with synthetic traffic

cargo test                       # all tests (lib unit tests + integration suites)
cargo test --lib                 # unit tests only
cargo test --test integration    # one integration suite (also: admin_attach, reproductions)
cargo test no_auth_connect_echo  # a single test by name
```

CLI shape: `serve` (alias `run`) and `attach` subcommands. Server flags `--config <path>`, `--listen <addr>`, `--no-tui`, `--no-admin`, `--admin-socket <path>`. Config comes from a TOML file with CLI flags overriding it; see `config.example.toml` for the full schema (`listen`, `[auth]` + `[[auth.users]]`, `[timeouts]`, `[limits]`, `[egress]`, `[admin]`).

## Architecture

Three swappable concerns: pure protocol codecs, an async concurrent server core, and an opt-in observability layer (admin socket + TUI). `main.rs` wires them with shared `Arc<Metrics>`, a `broadcast` event bus, and a `watch` shutdown channel.

- **`src/protocol/`** — pure, IO-free SOCKS5 codecs (handshake, auth, request/reply, address, udp framing). All unit-testable in isolation; keep IO out of here.
- **`src/server/`** — the proxy core, all async (tokio):
  - `mod.rs` — accept loop (`tokio::select!` on accept / shutdown / task reaping).
  - `admission.rs` — admission control **at accept time** (before handshake) so pre-auth floods can't bypass `max_connections` / `max_per_ip`. Holds a `Permit` for the connection's lifetime.
  - `connection.rs` — per-connection state machine: greeting → auth → request, all bounded by a **single `handshake_ms` deadline** (anti-slowloris). Pipelined post-request bytes are passed to the relay as `initial`.
  - `connect.rs` — CONNECT: resolve → egress check → dial (timeout-bounded) → success reply with BND → bidirectional relay with idle timeout + per-byte metrics.
  - `udp.rs` — UDP ASSOCIATE: bind ephemeral socket, advertise a **client-reachable BND address** (control stream's local addr, else `public_addr`), relay loop tied to control-connection lifetime, with idle timeout, per-association rate cap, and LRU target cap.
- **`src/config.rs`** — `Config`/`Cli`/`Command`, TOML+CLI merge, and the **egress filter** (SSRF guard: blocks loopback/link-local/private/CGNAT by default — `[egress]`).
- **`src/auth.rs`** — constant-time credential verification (no timing side-channels).
- **`src/metrics.rs`** — atomic counters (hot path, per-byte) + Mutex-guarded connection registry (cold path, per open/close) + serializable snapshot/event types. `format_event()` is the single source of human-readable event text for both TUI and headless modes.
- **`src/admin/`** — opt-in Unix socket for live observability. `server.rs` does race-free socket claim via `libc::flock` on a `.lock` file (handles stale sockets / TOCTOU), then streams a `Hello` frame, replays the `EventRing` (`ring.rs`, ~500 events), and pushes `Stats` every 250ms plus live events. Wire format (`mod.rs`): postcard frames with 4-byte BE length prefix, 1 MiB cap. `client.rs` is the `attach` side.
- **`src/tui/`** — ratatui + crossterm dashboard (only built with the `tui` feature). 250ms tick samples metrics, drains the event bus, redraws. `TerminalGuard` (RAII) restores the terminal even on panic. `attach` drives this same UI from a `RemoteState` fed by the admin socket.

## Conventions & gotchas

- **`tui` is a default feature.** Headless builds (`--no-default-features`) compile out ratatui/crossterm and the `attach`/dashboard paths. The Docker image always runs headless (`serve --no-tui`).
- **Egress filtering is secure-by-default** — loopback/link-local/private/CGNAT targets are blocked unless explicitly relaxed in `[egress]`. Don't loosen it casually; it's the open-relay / SSRF guard.
- **Security regression tests live in `tests/reproductions.rs`** (P0/P1/P2 findings from a security audit). When touching auth, handshake timeouts, egress, admin socket claiming, or UDP rate/target caps, re-run and extend these.
- **Releases are tag-driven.** `.github/workflows/build.yml` triggers on `v*` tags: cross-compiles musl x86_64/aarch64 binaries, publishes a multi-arch image to `ghcr.io/zingerlittlebee/next-socks5`, and cuts a GitHub Release whose notes are extracted from the matching `## [x.y.z]` section in `CHANGELOG.md`. macOS/Windows targets are present but commented out. Use the `release-version` skill to bump the version (`Cargo.toml` + `Cargo.lock`) and validate the changelog entry before tagging.
- **`install.sh`** is a POSIX-sh one-shot installer (binary+systemd/OpenRC or Docker Compose). It always references the binary by absolute path `${BIN_DIR}/${BIN_NAME}` rather than relying on `$PATH`.
