# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.2] - 2026-06-05

The project moved to its own repository and now re-publishes its release
artifacts from there. There are no functional changes to the server.

### Changed

- The project now lives at `github.com/ZingerLittleBee/next-socks5`. The
  container image is published to `ghcr.io/zingerlittlebee/next-socks5`, and the
  `install.sh` one-liner, README links, and release/download URLs all point to
  the new location.
- CI build, image, and release jobs run on GitHub-hosted `ubuntu-latest`
  runners again.

## [0.3.1] - 2026-06-04

A bare `next-socks5` run on a host already running the service used to start a
second server that hijacked — and then deleted — the live service's admin
socket, leaving it with no reachable `attach` endpoint. This release fixes that
and makes starting the server explicit. Covered by regression tests and
validated on a live Linux deployment.

### Fixed

- Admin-socket hijack: a second `next-socks5` process no longer unlinks and
  rebinds an admin Unix socket that a live instance is already serving (which
  silently destroyed the running server's `attach` socket). The admin endpoint
  now probes the path with `connect()` and refuses to clobber a live socket,
  holds a lifetime advisory lock on a sidecar `<socket>.lock` to serialize
  racing starters, and still reclaims a stale socket left by a crashed instance.

### Changed

- A bare `next-socks5` (no arguments) now prints help instead of starting a
  server; run the server explicitly with the new `serve` subcommand. Legacy
  flag-only invocations (e.g. `next-socks5 --no-tui --config …`) still start the
  server with a one-time deprecation notice, so existing systemd / OpenRC /
  Docker deployments keep working unchanged.
- `install.sh` (systemd & OpenRC units, manual-start hints) and the Docker image
  entrypoint now launch the server via `serve`.

### Added

- `serve` subcommand (alias `run`) to run the SOCKS5 server.

## [0.3.0] - 2026-06-04

Security & robustness hardening from a full SOCKS5 audit. Every fix is covered
by a regression test (written test-first) and was validated on a live Linux
deployment.

### Security

- Verify username/password credentials in constant time, removing an auth
  timing side channel (RFC 1929).
- Egress filtering, **on by default**: refuse to relay to loopback, link-local
  (including the `169.254.169.254` cloud-metadata address), and private/RFC1918
  ranges — an SSRF / open-relay guard. Configurable via a new `[egress]` section.
- Bound the pre-relay handshake with `timeouts.handshake_ms` (default 10s) so a
  stalled client cannot pin a task and its file descriptor (pre-auth slowloris).
- Enforce connection limits at accept time, counting half-open/handshaking
  connections, with a new per-source-IP cap (`limits.max_per_ip`).
- Restrict the admin Unix socket to mode `0600` under a `0700` directory the
  server creates itself.

### Added

- New configuration options: `timeouts.handshake_ms`, the `[egress]` policy,
  `limits.max_per_ip`, `limits.udp_max_targets`, and `limits.udp_rate_pps`.
- Simplified-Chinese README (`README.zh-CN.md`) with a language switcher.

### Changed

- `limits.max_connections` is now enforced at accept time (replacing an
  ineffective post-request check that a half-open flood could bypass).

### Fixed

- Bound the CONNECT relay with write / idle / DNS-resolution timeouts so a stuck
  peer or a slow resolver cannot pin a relay forever.
- Harden the UDP relay: bounded known-target set, exact client `ip:port`
  locking, egress checks on targets, an optional pps rate cap, and a `send_to`
  timeout so a saturated send buffer cannot stall the relay loop.
- Forward graceful shutdown into in-flight CONNECT relays and UDP associations
  so active transfers wind down promptly instead of surviving until teardown.
- Relay bytes a client pipelines after the handshake instead of dropping them
  (no silent stream truncation).
- Send a best-effort RFC 1929 failure reply on malformed auth instead of a
  silent TCP close.
- Recover from a poisoned metrics registry mutex instead of cascading panics
  from a single task failure.

## [0.2.0] - 2026-06-04

### Added

- One-shot `install.sh` (binary or Docker) with auth/port options, systemd &
  OpenRC service setup, and a copy-friendly summary that shows the public IP.
- Remote TUI attach: connect to a running server over a local Unix socket and
  render its live dashboard (`next-socks5 attach`), configured via `[admin]`.
- Richer TUI dashboard — a merged up/down throughput trend chart, success rate,
  an error histogram, sortable/scrollable connections and log panels, and a
  top-error summary line.
- `--mock` flag to drive the dashboard with synthetic data for previews/testing.
- Multi-user password auth (multiple `[[auth.users]]` entries on one port).

### Fixed

- TUI: read key input on a dedicated thread so keystrokes are not dropped.
- Load the systemd config via `LoadCredential` so the `DynamicUser` can read it,
  and restart the service on reinstall so a new config actually applies.
- Make `install.sh` POSIX-`sh` compatible (no bash required).

## [0.1.0] - 2026-06-03

Initial release — a hand-written SOCKS5 server (RFC 1928 + RFC 1929).

### Added

- SOCKS5 `CONNECT` and `UDP ASSOCIATE` with IPv4/IPv6/domain address types and
  server-side DNS resolution.
- No-auth and username/password (RFC 1929) authentication.
- Full RFC reply-code mapping (`0x00`–`0x08`), including unsupported
  command/address-type and OS-error mapping.
- UDP relay with SOCKS5 encapsulation, `FRAG != 0` drop, source-IP filtering, a
  client-reachable `BND.ADDR`, and idle reclaim.
- Connect / TCP-idle / UDP-idle timeouts, an optional `max_connections` limit, a
  half-open-aware relay, and graceful shutdown.
- A ratatui terminal dashboard and a `--no-tui` headless mode (the TUI is an
  optional cargo feature).
- TOML configuration with CLI overrides.
- Release CI: multi-arch static musl binaries and a GHCR Docker image, cut on
  version tags.
