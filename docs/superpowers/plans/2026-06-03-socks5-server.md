# SOCKS5 Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a lightweight, scalable SOCKS5 server in Rust (RFC 1928 CONNECT + UDP ASSOCIATE, RFC 1929 user/pass auth) with a ratatui TUI dashboard and a headless mode.

**Architecture:** Single crate (lib + bin). A pure, IO-free `protocol` layer (exhaustively unit-tested) sits under an async `server` layer (Tokio). `metrics` is the shared bridge (atomic counters + connection registry + event bus) feeding either the `tui` dashboard or stdout in headless mode.

**Tech Stack:** Rust 1.93, Tokio (multi-thread runtime), ratatui + crossterm, serde + toml, clap (derive), thiserror.

---

## File Structure

```
next-socks5/
├── Cargo.toml                  # deps + lib/bin targets
├── config.example.toml         # sample config
├── tests/
│   ├── integration.rs          # end-to-end client tests
│   └── scripts/                # reusable manual test scripts
│       ├── smoke_connect.sh
│       └── smoke_udp.sh
└── src/
    ├── main.rs                 # wiring: config -> server -> TUI/headless
    ├── lib.rs                  # re-exports for integration tests
    ├── config.rs               # Config (serde) + Cli (clap) + merge
    ├── error.rs                # Socks5Error -> RFC reply code
    ├── protocol/
    │   ├── mod.rs
    │   ├── address.rs          # Address encode/decode (v4/v6/domain)
    │   ├── handshake.rs        # method negotiation + RFC1929 auth
    │   ├── request.rs          # CMD + ATYP + address parse
    │   ├── reply.rs            # reply serialization
    │   └── udp.rs              # UDP datagram header encap/decap
    ├── server/
    │   ├── mod.rs              # listener + graceful shutdown
    │   ├── connection.rs       # per-conn state machine
    │   ├── connect.rs          # CONNECT dial + bidi copy
    │   └── udp.rs              # UDP ASSOCIATE relay
    ├── metrics.rs              # atomic counters + registry + event bus
    └── tui/
        ├── mod.rs              # event loop + sampling + terminal guard
        └── widgets.rs          # rate/conn/stats/log panels
```

---

## Task 1: Project scaffolding

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `config.example.toml`

- [ ] **Step 1: Write `Cargo.toml`** with package `next-socks5`, edition 2021, a `[lib]` (name `next_socks5`) and `[[bin]]` (name `next-socks5`, path `src/main.rs`). Deps: `tokio = { version = "1", features = ["net","rt-multi-thread","io-util","time","sync","macros","signal"] }`, `ratatui = "0.29"`, `crossterm = "0.28"`, `serde = { version = "1", features = ["derive"] }`, `toml = "0.8"`, `clap = { version = "4", features = ["derive"] }`, `thiserror = "2"`.
- [ ] **Step 2:** Minimal `src/lib.rs` declaring `pub mod` for each module to be created (stub modules empty for now is fine, add as tasks land). Minimal `src/main.rs` with `#[tokio::main] async fn main()` that prints a banner.
- [ ] **Step 3:** Run `cargo build`. Expected: compiles.
- [ ] **Step 4: Commit** `chore: scaffold next-socks5 crate`.

---

## Task 2: Protocol — Address codec (`src/protocol/address.rs`)

Pure, no IO. This is the foundation reused by request, reply, and UDP.

**Files:** Create `src/protocol/address.rs`, `src/protocol/mod.rs`.

```rust
// Address parsed from the wire. Domain kept as String (resolved later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    V4(std::net::Ipv4Addr, u16),
    V6(std::net::Ipv6Addr, u16),
    Domain(String, u16),
}
```

API (all pure):
- `Address::decode(buf: &[u8]) -> Result<(Address, usize), AddrError>` — reads ATYP+addr+port, returns the address and bytes consumed. ATYP is the first byte.
- `Address::encode(&self, out: &mut Vec<u8>)` — writes ATYP+addr+port.
- `pub enum AddrError { Truncated, BadAtyp(u8), BadDomain }`.

Wire format: ATYP `0x01`=IPv4 (4 bytes + 2 port), `0x04`=IPv6 (16 + 2), `0x03`=Domain (1 len byte + len bytes + 2 port). Port is big-endian. Domain must be valid UTF-8.

- [ ] **Step 1: Write failing tests** in `#[cfg(test)] mod tests`:
  - v4 round-trip `127.0.0.1:1080`
  - v6 round-trip `[::1]:443`
  - domain round-trip `example.com:80`
  - truncated buffer (e.g. ATYP=v4 but only 2 bytes) → `Err(Truncated)`
  - bad ATYP `0x09` → `Err(BadAtyp(9))`
  - max domain length 255 round-trips; decode returns correct consumed length
  - decode returns correct `consumed` so trailing bytes are untouched (append `0xAA` and assert it's left over)
- [ ] **Step 2:** Run `cargo test address` → FAIL (not implemented).
- [ ] **Step 3:** Implement `decode`/`encode`.
- [ ] **Step 4:** `cargo test address` → PASS.
- [ ] **Step 5: Commit** `feat: add SOCKS5 address codec`.

---

## Task 3: Protocol — Handshake & auth (`src/protocol/handshake.rs`)

Pure parsing/serialization of the greeting and RFC 1929 auth. No socket IO — functions take/return bytes.

**Files:** Create `src/protocol/handshake.rs`.

```rust
pub const VERSION: u8 = 0x05;
pub const METHOD_NO_AUTH: u8 = 0x00;
pub const METHOD_USERPASS: u8 = 0x02;
pub const METHOD_NONE_ACCEPTABLE: u8 = 0xFF;

// Client greeting: VER NMETHODS METHODS...
pub fn parse_greeting(buf: &[u8]) -> Result<Vec<u8>, HandshakeError>; // returns methods list
pub fn select_method(offered: &[u8], require_userpass: bool) -> u8;   // returns chosen method or 0xFF
pub fn method_reply(method: u8) -> [u8; 2];                            // VER, METHOD

// RFC 1929: VER(0x01) ULEN UNAME PLEN PASSWD
pub fn parse_userpass(buf: &[u8]) -> Result<(String, String), HandshakeError>;
pub fn userpass_reply(ok: bool) -> [u8; 2];                            // 0x01, 0x00|0x01

pub enum HandshakeError { Truncated, BadVersion(u8), BadDomain }
```

- [ ] **Step 1: Write failing tests:** greeting parse with [no-auth, userpass]; `select_method` picks userpass when required and offered, picks no-auth when not required, returns `0xFF` when required-but-not-offered; userpass parse round-trip; userpass parse truncated → Err; bad auth version → Err; `method_reply`/`userpass_reply` byte values.
- [ ] **Step 2:** Run `cargo test handshake` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test handshake` → PASS.
- [ ] **Step 5: Commit** `feat: add SOCKS5 handshake and RFC1929 auth codec`.

---

## Task 4: Protocol — Request parse + Reply + Error mapping (`request.rs`, `reply.rs`, `src/error.rs`)

**Files:** Create `src/protocol/request.rs`, `src/protocol/reply.rs`, `src/error.rs`.

```rust
// request.rs — VER CMD RSV ATYP DST.ADDR DST.PORT
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command { Connect, Bind, UdpAssociate }
pub struct Request { pub command: Command, pub address: Address }
pub fn parse_request(buf: &[u8]) -> Result<Request, RequestError>;
pub enum RequestError { Truncated, BadVersion(u8), BadCommand(u8), Addr(AddrError) }

// error.rs
#[derive(thiserror::Error, Debug)]
pub enum Socks5Error {
    #[error("general failure")] General,            // 0x01
    #[error("not allowed by ruleset")] NotAllowed,  // 0x02
    #[error("network unreachable")] NetworkUnreachable, // 0x03
    #[error("host unreachable")] HostUnreachable,   // 0x04
    #[error("connection refused")] ConnectionRefused, // 0x05
    #[error("ttl expired")] TtlExpired,             // 0x06
    #[error("command not supported")] CommandNotSupported, // 0x07
    #[error("address type not supported")] AddressNotSupported, // 0x08
}
impl Socks5Error { pub fn reply_code(&self) -> u8 { ... } }       // 0x01..=0x08
impl Socks5Error { pub fn from_io(e: &std::io::Error) -> Self }   // ErrorKind mapping

// reply.rs — VER REP RSV ATYP BND.ADDR BND.PORT
pub fn encode_reply(code: u8, bind: &Address, out: &mut Vec<u8>);
pub const REP_SUCCEEDED: u8 = 0x00;
```

`from_io` mapping: `ConnectionRefused`→ConnectionRefused; `TimedOut`→TtlExpired; `HostUnreachable`(if available, else fallback)→HostUnreachable; `NetworkUnreachable`→NetworkUnreachable; otherwise→General. Use string/kind matching that compiles on stable (some ErrorKinds are unstable — match on the stable ones and default the rest to `General`/`HostUnreachable`).

- [ ] **Step 1: Write failing tests:** request parse CONNECT/v4, UDP/domain, BIND→`Command::Bind`; bad version; bad command byte → `BadCommand`; truncated; `reply_code()` for each variant; `from_io(ConnectionRefused)` → ConnectionRefused/0x05; `encode_reply` byte layout for success with v4 bind.
- [ ] **Step 2:** `cargo test` (request/reply/error) → FAIL.
- [ ] **Step 3:** Implement all three.
- [ ] **Step 4:** Tests → PASS.
- [ ] **Step 5: Commit** `feat: add request parse, reply codec, RFC error mapping`.

---

## Task 5: Protocol — UDP datagram header (`src/protocol/udp.rs`)

**Files:** Create `src/protocol/udp.rs`.

Wire: `RSV(2=0x0000) FRAG(1) ATYP(1) DST.ADDR DST.PORT DATA`.

```rust
pub struct UdpDatagram { pub frag: u8, pub address: Address, pub data: Vec<u8> }
pub fn decap(buf: &[u8]) -> Result<UdpDatagram, UdpError>;  // FRAG!=0 is parsed; caller drops it
pub fn encap(address: &Address, data: &[u8], out: &mut Vec<u8>); // FRAG=0
pub enum UdpError { Truncated, Addr(AddrError) }
```

- [ ] **Step 1: Write failing tests:** encap/decap round-trip with domain target; decap with `FRAG=3` returns `frag==3` (caller policy drops); truncated header → Err; encap output begins with `00 00 00`.
- [ ] **Step 2:** `cargo test udp` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5: Commit** `feat: add SOCKS5 UDP datagram header codec`.

---

## Task 6: Configuration (`src/config.rs` + `config.example.toml`)

**Files:** Create `src/config.rs`, `config.example.toml`.

```rust
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub listen: String,                 // "127.0.0.1:1080"
    #[serde(default)] pub auth: AuthConfig,
    #[serde(default)] pub timeouts: Timeouts,
    #[serde(default)] pub limits: Limits,
    #[serde(default)] pub public_addr: Option<String>, // advertised BND addr for UDP
}
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct AuthConfig { pub method: AuthMethod, #[serde(default)] pub users: Vec<User> }
pub enum AuthMethod { None, Password }     // serde rename "none"/"password", default None
pub struct User { pub username: String, pub password: String }
pub struct Timeouts { connect_ms, tcp_idle_ms, udp_idle_ms }   // defaults 10000/300000/60000
pub struct Limits { pub max_connections: Option<usize> }

#[derive(clap::Parser)]
pub struct Cli {
    #[arg(long)] pub config: Option<PathBuf>,
    #[arg(long)] pub listen: Option<String>,
    #[arg(long)] pub no_tui: bool,
}
impl Config {
    pub fn load(cli: &Cli) -> Result<Config, ConfigError>; // read toml (if --config), apply overrides; sane defaults if no file
}
```

- [ ] **Step 1: Write failing tests:** parse the example TOML string → expected struct; defaults fill when sections omitted; CLI `--listen` overrides file value; `AuthMethod` serde rename works.
- [ ] **Step 2:** `cargo test config` → FAIL.
- [ ] **Step 3:** Implement + write `config.example.toml` matching spec §10.
- [ ] **Step 4:** PASS.
- [ ] **Step 5: Commit** `feat: add TOML config with CLI override`.

---

## Task 7: Metrics & event bus (`src/metrics.rs`)

**Files:** Create `src/metrics.rs`.

```rust
pub struct Metrics {
    pub bytes_up: AtomicU64, pub bytes_down: AtomicU64,
    pub total_conns: AtomicU64, pub active_conns: AtomicU64,
    pub successes: AtomicU64, pub failures: AtomicU64,
    pub error_codes: [AtomicU64; 9],          // index by reply code 0..=8
    registry: Mutex<HashMap<u64, ConnInfo>>,
    next_id: AtomicU64,
}
pub struct ConnInfo { pub id: u64, pub src: SocketAddr, pub target: String, pub kind: ConnKind, pub up: u64, pub down: u64 /* start time omitted: Instant set at register */ }
pub enum ConnKind { Connect, Udp }
pub enum Event { Connect{..}, Closed{id}, Error{code,msg}, Auth{ok,user}, Log(String) }

impl Metrics {
    pub fn new() -> Arc<Self>;
    pub fn register(&self, src, target, kind) -> u64;   // ++total, ++active, insert registry
    pub fn unregister(&self, id);                       // --active, remove
    pub fn add_up(&self, id, n); pub fn add_down(&self, id, n);  // atomic + per-conn
    pub fn record_error(&self, code: u8);               // ++failures, ++error_codes[code]
    pub fn record_success(&self);
    pub fn snapshot(&self) -> Snapshot;                 // for TUI sampling
}
// Event bus: tokio::sync::broadcast channel created in main, Sender cloned into server, Receiver into TUI/headless.
```

- [ ] **Step 1: Write failing tests:** register increments total+active and returns unique ids; unregister decrements active; add_up/add_down accumulate global + per-conn; record_error bumps the right code index; snapshot reflects counters.
- [ ] **Step 2:** `cargo test metrics` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5: Commit** `feat: add shared metrics and event bus`.

---

## Task 8: Server — connection state machine + CONNECT (`server/mod.rs`, `connection.rs`, `connect.rs`)

**Files:** Create `src/server/mod.rs`, `src/server/connection.rs`, `src/server/connect.rs`.

Behavior:
- `server::run(listener, cfg, metrics, events, shutdown: watch::Receiver<bool>)`: accept loop; on each `TcpStream` spawn `connection::handle`; respect `max_connections` (reply `0x01` and close when over); stop accepting when shutdown fires; await in-flight via task tracking (a simple `JoinSet` or counter).
- `connection::handle`: read greeting → `select_method` → if userpass, read+verify against `cfg.auth.users` (reply auth ok/fail, emit `Auth` event) → parse request → dispatch: CONNECT → `connect::run`; UDP ASSOCIATE → `udp::run` (Task 9); BIND → reply `0x07`.
- `connect::run`: locally resolve domain via `tokio::net::lookup_host` (map failure → Host/NetworkUnreachable), dial with `tokio::time::timeout(connect_ms)` (timeout → `TtlExpired` 0x06), on success reply `0x00` with the local bound address, then `bidirectional copy` updating `metrics.add_up/add_down`, with TCP idle timeout disconnect.

Write a small `copy_bidirectional_counted` helper (two `tokio::io::copy`-style loops with byte counters and idle timeout via `tokio::time::timeout` on each read).

- [ ] **Step 1: Write failing integration test** in `tests/integration.rs`: start server (no-auth) on ephemeral port in a spawned task; start a local echo TCP server; connect a raw client, do the SOCKS5 handshake + CONNECT to the echo server, send `b"ping"`, assert `b"ping"` echoes back. (This drives the whole TCP path.)
- [ ] **Step 2:** Run `cargo test --test integration` → FAIL.
- [ ] **Step 3:** Implement `server::run`, `connection::handle`, `connect::run` + helper. Wire modules into `lib.rs`.
- [ ] **Step 4:** Test → PASS.
- [ ] **Step 5: Commit** `feat: add TCP server, connection state machine, CONNECT relay`.

---

## Task 9: Server — UDP ASSOCIATE relay (`src/server/udp.rs`)

**Files:** Create `src/server/udp.rs`.

Behavior (spec §6 UDP details):
- On UDP ASSOCIATE: bind a `UdpSocket` on the server (port 0 on the same local IP as the TCP control conn, or `public_addr`), reply `0x00` with BND.ADDR/PORT = a client-reachable address (never `0.0.0.0`).
- Record the client's IP from the TCP control connection. Relay loop: recv datagram; drop if source IP != client IP (source filtering); `decap`; drop if `frag != 0`; resolve target (server-side DNS); forward `DATA` to target via a per-target or shared upstream socket; on reply, `encap` with responder address and send back to the last client address.
- Lifetime: association lives while the TCP control conn is open; reclaim socket on control close OR udp idle timeout. Update `metrics.add_up/add_down`.

- [ ] **Step 1: Write failing integration test:** start server; UDP ASSOCIATE handshake over TCP; bind a local UDP echo server; send an encapsulated datagram (`encap` to echo target) to the relay BND addr; assert the echoed payload comes back correctly encapsulated. Add a second assertion: a datagram sent from a *different* source socket/IP is dropped (no reply) — if same-host IP filtering can't differ by IP in test, assert via port-bound association behavior and note the limitation.
- [ ] **Step 2:** `cargo test --test integration udp` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** PASS.
- [ ] **Step 5: Commit** `feat: add UDP ASSOCIATE relay with source filtering`.

---

## Task 10: Auth + error-code integration tests

**Files:** Modify `tests/integration.rs`.

- [ ] **Step 1: Write failing tests:** (a) password auth success then CONNECT works; (b) wrong password → auth reply `0x01` and connection closed; (c) CONNECT to a refused port → reply `0x05`; (d) BIND command → reply `0x07`.
- [ ] **Step 2:** Run → FAIL (if behavior gaps) / verify.
- [ ] **Step 3:** Fix any gaps in `connection.rs`/`connect.rs`.
- [ ] **Step 4:** PASS.
- [ ] **Step 5: Commit** `test: add auth and error-code integration tests`.

---

## Task 11: TUI dashboard (`src/tui/mod.rs`, `widgets.rs`)

**Files:** Create `src/tui/mod.rs`, `src/tui/widgets.rs`.

- Terminal-restore guard (RAII `Drop`) that disables raw mode + leaves alternate screen, also on panic (set a panic hook or rely on guard drop during unwind).
- Event loop (~250 ms tick): sample `metrics.snapshot()`, compute KB/s deltas, drain event-bus receiver into a bounded log ring buffer, render panels via `widgets`; handle key events: `q` and Ctrl-C → broadcast shutdown via the `watch` sender.
- `widgets`: rate panel (up/down KB/s + totals), active-connections table, stats panel (totals/success/fail/per-error-code), scrolling log panel.

This layer is hard to unit-test headlessly; keep logic (rate computation, ring buffer) in small pure functions that ARE unit-tested.

- [ ] **Step 1: Write failing unit tests** for the pure helpers: rate computation `(bytes_now - bytes_prev) / dt` → KB/s; log ring buffer caps at N and drops oldest.
- [ ] **Step 2:** `cargo test tui` → FAIL.
- [ ] **Step 3:** Implement helpers + render code + terminal guard.
- [ ] **Step 4:** `cargo test tui` → PASS; `cargo build` ok.
- [ ] **Step 5: Commit** `feat: add ratatui dashboard`.

---

## Task 12: Wiring, headless mode, graceful shutdown (`src/main.rs`)

**Files:** Modify `src/main.rs`, `src/lib.rs`.

- Parse `Cli`, `Config::load`. Build `Arc<Metrics>`, `broadcast` event bus, `watch::channel(false)` shutdown.
- Bind `TcpListener`. Spawn `server::run`.
- If `--no-tui`: spawn a task draining event-bus → stdout lines; `tokio::signal::ctrl_c()` → set shutdown=true. Else: run TUI loop (Task 11), which owns shutdown on `q`/Ctrl-C.
- On shutdown: stop accepting, drain active conns, restore terminal.

- [ ] **Step 1:** Implement wiring for both modes.
- [ ] **Step 2:** Run `cargo build` and `cargo run -- --no-tui --listen 127.0.0.1:0` briefly (or rely on Task 13 scripts). Expected: starts, prints startup info, Ctrl-C exits cleanly.
- [ ] **Step 3:** `cargo clippy --all-targets` clean (no warnings ideally).
- [ ] **Step 4: Commit** `feat: wire main, headless mode, graceful shutdown`.

---

## Task 13: Reusable test scripts + full verification

**Files:** Create `tests/scripts/smoke_connect.sh`, `tests/scripts/smoke_udp.sh`, `tests/scripts/run_all.sh`.

- `smoke_connect.sh`: build release/debug, start server headless on `127.0.0.1:1080` in background, use `curl --socks5 127.0.0.1:1080 http://example.com` (or `curl --socks5-hostname`) and assert HTTP 200; with-auth variant using `--socks5 user:pass@`. Clean up the server PID at the end.
- `smoke_udp.sh`: start server, use a small client (curl supports SOCKS5 for DNS; for raw UDP relay use `nc`/a tiny python client) to exercise UDP ASSOCIATE against a local UDP echo. Document expected output.
- `run_all.sh`: `cargo test` + run both smoke scripts; print PASS/FAIL summary.
- Make scripts executable (`chmod +x`). Each script `set -euo pipefail`, configurable `BIN`/`PORT` via env.

- [ ] **Step 1:** Write the three scripts.
- [ ] **Step 2:** Run `cargo test` (all unit + integration) → all PASS. Capture output.
- [ ] **Step 3:** Run `tests/scripts/run_all.sh` → PASS (fix failures via systematic-debugging, re-run).
- [ ] **Step 4:** `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check`.
- [ ] **Step 5: Commit** `test: add reusable smoke scripts and verify full suite`.

---

## Self-Review (spec coverage)

- CONNECT (Task 8), UDP ASSOCIATE (Task 9), BIND→0x07 (Tasks 4/10) ✓
- No-Auth + RFC1929 user/pass (Tasks 3, 8, 10) ✓
- IPv4/IPv6/Domain ATYP (Task 2) ✓
- Server-side DNS for CONNECT + UDP (Tasks 8, 9) ✓
- Full RFC error mapping 0x00–0x08 (Task 4) ✓
- TOML config + CLI override (Task 6) ✓
- TUI dashboard: rate/conns/stats/log (Task 11) ✓
- Headless mode (Task 12) ✓
- Timeouts (connect/tcp idle/udp idle), graceful shutdown, max-connections (Tasks 8, 9, 12) ✓
- UDP encapsulation, FRAG drop, source filtering, client-reachable BND (Tasks 5, 9) ✓
- Protocol unit tests + integration tests + reusable scripts (Tasks 2–5, 8–10, 13) ✓
