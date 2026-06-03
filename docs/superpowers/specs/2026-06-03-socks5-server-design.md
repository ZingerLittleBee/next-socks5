# SOCKS5 Server (next-socks5) — Design

**Date:** 2026-06-03
**Status:** Approved (pending spec review)

## 1. Goal

A lightweight, scalable SOCKS5 server written in Rust that implements the
selected RFC 1928 (SOCKS5) command/auth subset plus RFC 1929 (Username/Password
authentication), wrapped in a terminal dashboard (TUI) that shows live
throughput, connections, and statistics. A design priority is keeping the
dependency footprint small. ("Selected subset" = CONNECT + UDP ASSOCIATE
commands and No-Auth + Username/Password auth; BIND and GSSAPI are intentionally
out of scope — see §2.)

## 2. Scope

### In scope
- **Commands:** `CONNECT` and `UDP ASSOCIATE`. `BIND` is rejected with reply
  code `0x07` (Command not supported) for RFC-conformant behavior.
- **Auth methods:** No-Auth (`0x00`) and Username/Password (`0x02`, RFC 1929).
- **Address types:** IPv4, IPv6, and Domain name (ATYP `0x01` / `0x04` / `0x03`).
- **DNS:** Domain addresses are resolved server-side via Tokio's `lookup_host`,
  for both CONNECT targets and UDP relay datagram targets (same strategy).
- **RFC error mapping:** all reply codes `0x00`–`0x08` are produced where applicable.
- **Configuration:** TOML config file as the primary source, with CLI flags overriding.
- **TUI dashboard:** real-time rate + total traffic, active connection list,
  connection/error statistics, and a scrolling log panel with startup info.
- **Headless mode:** `--no-tui` runs without the dashboard, emitting events to stdout
  (for systemd/containers).
- **Robustness:** connect timeout, TCP idle timeout, UDP association idle timeout,
  graceful shutdown, optional max-connections limit.

### Out of scope (YAGNI)
- `BIND` command implementation (returns `0x07`).
- GSSAPI authentication (`0x01`).
- Hot-reload of config/users.
- Custom/upstream DNS resolvers (uses system resolver only).
- ACL by source IP (can be added later; not in v1).

## 3. Concurrency model

Single Tokio multi-threaded runtime handles all TCP and UDP work. Each accepted
TCP connection runs in its own spawned task. This is the de facto standard for
Rust network services and makes UDP ASSOCIATE and timeout handling
straightforward.

## 4. Project structure (single crate, lib + bin)

```
next-socks5/
├── Cargo.toml
├── config.example.toml
└── src/
    ├── main.rs          # Wiring: parse config -> start server -> TUI or headless
    ├── lib.rs           # Re-exports core for integration tests
    ├── config.rs        # Config file + CLI override (clap derive)
    ├── protocol/        # Pure protocol layer (no IO, easily unit-tested)
    │   ├── handshake.rs  # Method negotiation, username/password auth (RFC 1929)
    │   ├── request.rs    # Request parsing: CMD + ATYP + address
    │   ├── address.rs    # IPv4 / IPv6 / Domain encode + decode
    │   └── reply.rs      # Replies + RFC error-code mapping
    ├── server/
    │   ├── mod.rs        # TCP listener + graceful shutdown
    │   ├── connection.rs # Per-connection state machine: auth -> request -> dispatch
    │   ├── connect.rs    # CONNECT: dial + bidirectional copy (with byte counters)
    │   └── udp.rs        # UDP ASSOCIATE: relay + idle timeout
    ├── metrics.rs       # Shared atomic counters + connection registry + event bus
    └── tui/
        ├── mod.rs        # ratatui event loop + sampling
        └── widgets.rs    # Rate / connections / stats / log panels
```

Rationale: the protocol layer is pure (no IO) so it can be unit-tested
exhaustively; the server layer owns async IO; `metrics` is the single shared
bridge between the server and the TUI.

## 5. Dependencies (kept minimal)

| Crate | Purpose | Notes |
|---|---|---|
| `tokio` | Async runtime | Features: `net, rt-multi-thread, io-util, time, sync, macros, signal` |
| `ratatui` + `crossterm` | TUI | Dashboard rendering |
| `serde` + `toml` | Config parsing | `serde` with the `derive` feature |
| `clap` | CLI override | `derive` feature; `--help`, validation, ergonomics |
| `thiserror` | Error -> RFC code mapping | Compile-time only, zero runtime cost |

Deliberately **not** used:
- Any existing socks5 crate — the protocol is hand-written (it is the core of the project).
- `tracing` — logging flows through a self-built event bus straight to the TUI.
- `tokio-util` — shutdown signaling uses `tokio::sync::watch` directly.

## 6. Data flow

```
main -> load Config -> build Arc<Metrics> + shutdown watch channel
      -> spawn server::run(listener)
      -> TUI render loop  (or headless: events to stdout)

Per TCP connection -> spawn connection::handle:
   method negotiation -> (optional) username/password auth -> parse request
     ├─ CONNECT -> resolve domain locally -> dial (connect timeout) -> bidirectional copy
     │             byte counts accumulate into Metrics; idle timeout disconnects
     ├─ UDP ASSOCIATE -> bind UDP socket -> reply BND.ADDR/PORT (client-reachable)
     │             decapsulate/relay datagrams; reclaim on TCP control close or idle timeout
     └─ BIND -> reply 0x07 (Command not supported)
```

### UDP ASSOCIATE details (RFC 1928 §7)

- **Encapsulation:** datagrams from the client are NOT raw payload. Each carries the
  SOCKS5 UDP request header: `RSV(2 bytes, 0x0000) + FRAG(1) + ATYP(1) + DST.ADDR + DST.PORT + DATA`.
  The relay parses this header, resolves the target (domain targets use the same
  server-side DNS strategy as CONNECT), and forwards `DATA` to the target. Replies
  from the target are re-encapsulated with the same header (ATYP/address of the
  responder) before being sent back to the client.
- **Fragmentation:** `FRAG != 0` is unsupported; such datagrams are silently dropped.
- **Source filtering:** the relay records the client's IP from the authenticated TCP
  control connection. Inbound UDP datagrams whose source IP does not match are
  dropped, preventing unauthenticated injection into an established association.
- **BND.ADDR/PORT:** the reply advertises a client-reachable bind address (derived
  from the TCP control connection's local address / configured public address),
  never `0.0.0.0`.
- **Lifetime:** the association is bound to the TCP control connection. When that TCP
  connection closes, or after the UDP idle timeout, the UDP socket is reclaimed.

**Rate calculation:** byte totals are kept in `AtomicU64` counters updated on the
hot path. The TUI samples the deltas each tick (~250 ms) to compute KB/s, keeping
the relay path free of per-byte rate bookkeeping.

## 7. Metrics & shared state

- `Arc<Metrics>` holds global `AtomicU64` counters: bytes up/down, total
  connections, active connections, successes, failures, and a per-RFC-error-code
  counter array.
- A connection registry (`Mutex<HashMap<ConnId, ConnInfo>>`) tracks active
  connections: source addr, target addr, command type (CONNECT/UDP), start time,
  and per-connection up/down byte counters.
- An event bus (a bounded `tokio::sync::mpsc` or `broadcast` channel) carries log
  events (connect/error/auth) to either the TUI log panel or stdout in headless mode.

## 8. Error mapping (RFC 1928 §6, full coverage)

`enum Socks5Error` maps to reply codes:

| Code | Meaning |
|---|---|
| `0x00` | succeeded |
| `0x01` | general SOCKS server failure |
| `0x02` | connection not allowed by ruleset |
| `0x03` | network unreachable |
| `0x04` | host unreachable |
| `0x05` | connection refused |
| `0x06` | TTL expired |
| `0x07` | command not supported |
| `0x08` | address type not supported |

Dial errors are mapped precisely from `io::ErrorKind` (e.g.
`ConnectionRefused` -> `0x05`, `NetworkUnreachable`/host resolution failure ->
`0x03`/`0x04`).

## 9. Robustness defaults

- **Timeouts:** connect timeout, TCP idle timeout, and UDP association idle
  timeout — all configurable.
- **Graceful shutdown:** two paths feed the same `watch` shutdown channel.
  In headless mode `tokio::signal` catches Ctrl-C. In TUI mode crossterm raw mode
  delivers Ctrl-C (and the `q` quit key) as keyboard events, so the TUI event loop
  also broadcasts shutdown. On shutdown: stop accepting, drain active connections,
  and restore the terminal (disable raw mode / leave alternate screen) — including
  on panic, via a terminal-restore guard.
- **Headless mode:** `--no-tui` skips the dashboard and writes events to stdout.
- **Max connections:** optional `max_connections` config; over the limit replies `0x02` (connection not allowed by ruleset).

## 10. Configuration (TOML + CLI override)

Example `config.toml`:

```toml
listen = "127.0.0.1:1080"

[auth]
method = "password"        # "none" | "password"
[[auth.users]]
username = "alice"
password = "secret"

[timeouts]
connect_ms = 10000
tcp_idle_ms = 300000
udp_idle_ms = 60000

[limits]
max_connections = 1024     # optional
```

CLI flags override config values, e.g. `--listen`, `--no-tui`, `--config <path>`.

## 11. Testing strategy

- **Protocol unit tests:** encode/decode and edge cases for handshake, request,
  address, and reply (truncated input, invalid ATYP, max domain length, auth
  success/failure). Includes the SOCKS5 UDP header: encap/decap round-trip,
  `FRAG != 0` drop, and domain ATYP in UDP datagrams.
- **Integration tests:** start a local server and use a real client to exercise
  No-Auth CONNECT (against a local echo server), password auth success/failure,
  UDP ASSOCIATE round-trip (with correct encapsulation), UDP source-IP filtering
  (datagram from a non-client IP is dropped), and triggering of individual error codes.

## 12. Open questions

None outstanding. All major decisions are settled.
