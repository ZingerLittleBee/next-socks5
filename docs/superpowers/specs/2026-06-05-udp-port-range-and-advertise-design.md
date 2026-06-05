# Design: configurable UDP port range + bind/advertise decoupling

- Status: draft (decisions locked; pending final review)
- Date: 2026-06-05
- Target version: 0.4.0
- Scope: `next-socks5` UDP ASSOCIATE relay (`src/server/udp.rs`, `src/config.rs`) + docs
- Related: RFC 1928 §6/§7; CLAUDE.md UDP notes; prior plan `docs/superpowers/plans/2026-06-03-socks5-server.md`

## 1. Motivation

Two real deployment problems with the current UDP ASSOCIATE relay:

1. **Unpredictable UDP port.** Each association binds an OS-assigned *ephemeral*
   port (`UdpSocket::bind((bind_ip, 0))`, `udp.rs:51`). Operators who only open
   the TCP listen port in a firewall find UDP broken, because the client must
   send datagrams to a random high port the firewall never opened.

2. **Cannot advertise a NAT/public address.** `public_addr` is currently used as
   **both** the UDP bind IP **and** the advertised `BND.ADDR` (`resolve_bind_ip`,
   `udp.rs:245-255`). Setting it to a public/NAT IP that is not a local interface
   makes `UdpSocket::bind` fail with `EADDRNOTAVAIL`, and the association dies
   silently (`udp.rs:51-53`). So behind NAT/Docker there is no working way to bind
   a local socket while advertising a client-reachable public address.

This design adds a **configurable UDP port range** and **decouples the advertised
address from the bound address**, the two changes a NAT/Docker deployment needs.

### Prior art (validates the approach)

From a survey of 14 SOCKS5 implementations: UDP relays split into *per-association
ephemeral sockets* (Dante default, 3proxy, gost, sing-box, Xray, things-go,
asyncio-socks-server — the camp `next-socks5` is in) and *single shared socket =
same-as-TCP* (mihomo, glider, trojan-go, shadowsocks). Only **Dante** exposes a
real UDP-port knob for a pure SOCKS5 server, and it is a **port range**
(`udp.portrange`), keeping per-association sockets. shadowsocks-rust offers a
single configurable port plus a separate `--udp-associate-addr` (advertise) knob.

We adopt Dante's model (range, per-association sockets preserved) plus
shadowsocks-rust's separation of bind vs advertise — without collapsing to a
shared socket, which would reintroduce cross-association reply-routing ambiguity.

## 2. Goals / non-goals

**Goals**

- Optional `[udp].port_range`; when set, each association binds a port inside that
  inclusive range instead of an OS ephemeral port.
- Preserve the per-association socket model (one socket per association, kernel
  demux, source filtering, lifetime tied to the TCP control connection) — no
  shared-socket rewrite.
- Decouple bind from advertise: bind on the control connection's local IP;
  advertise `[udp].advertise` (IP) when set, else the bound address. The advertised
  **port is always the real bound port**.
- Backward compatible for the default path: with no `[udp]` section, behavior is
  unchanged (ephemeral bind, advertise the bound address).
- Document detailed configuration + a NAT/Docker deployment guide in README,
  `config.example.toml`, `install.sh`, and `CHANGELOG.md`.

**Non-goals**

- No shared-socket / single-port-for-all-associations relay model.
- No advertised-**port** override (advertised port = real bound port; NAT must
  forward the range port-preservingly). Full PAT/port-remap is out of scope except
  the documented size-1 workaround.
- No new CLI flag (TOML-only, matching `udp_max_targets` / `udp_rate_pps`).
- No new crate dependency (no `rand`).

## 3. Configuration surface (`src/config.rs`)

### 3.1 `PortRange` type

New type placed before `Limits` (around `config.rs:123`):

```rust
/// An inclusive UDP relay port range `[start, end]` for binding association
/// sockets. `start == end` is a single fixed port. Deserialized from a
/// `"start-end"` string (e.g. `"40000-40100"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}
```

Custom `Deserialize` that accepts a `"start-end"` string and validates:

- exactly one `-` separating two `u16`s,
- `start >= 1` (port 0 means "any/ephemeral" and would defeat the feature),
- `start <= end` (`start == end` is allowed = a single fixed port).

Rationale for string form: matches Dante's `udp.portrange: 40000-50000`, reads
naturally in TOML, centralizes validation in one `Deserialize` impl.

### 3.2 New `[udp]` section (decision: dedicated section)

UDP transport/addressing config (bind range + advertised address) gets its own
`[udp]` section rather than scattering across `[limits]`. New `UdpConfig`:

```rust
/// UDP relay transport/addressing configuration.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq, Default)]
pub struct UdpConfig {
    /// Bind each association's relay socket inside this inclusive port range.
    /// `None` => OS-assigned ephemeral port (current behavior).
    #[serde(default)]
    pub port_range: Option<PortRange>,
    /// Advertised BND.ADDR IP for UDP ASSOCIATE replies (advertise-only; the
    /// advertised port is always the real bound port). `None` => advertise the
    /// bound address. Needed behind NAT/Docker.
    #[serde(default)]
    pub advertise: Option<String>,
}
```

added to `Config` as `#[serde(default)] pub udp: UdpConfig` (`config.rs:9-32`),
and to `default_config()` (`config.rs:319-329`) as `udp: UdpConfig::default()`.

TOML:

```toml
[udp]
port_range = "40000-40100"   # optional bind range; omit for ephemeral
advertise  = "203.0.113.42"  # optional advertised BND IP (NAT/Docker)
```

The existing `udp_max_targets` / `udp_rate_pps` stay in `[limits]` and
`udp_idle_ms` stays in `[timeouts]` — they are genuinely limits/timeouts, not
transport addressing, so they are not moved.

### 3.3 `public_addr` → `[udp].advertise` (breaking rename, decision)

The current top-level `public_addr` (`config.rs:25`) is **removed**; its role moves
to `[udp].advertise` with clarified advertise-only semantics:

- overrides the advertised `BND.ADDR` **IP** only; a port in `"ip:port"` form is
  parsed away and ignored (advertised port = real bound port);
- an unspecified IP (`0.0.0.0` / `::`) is ignored (never advertise unspecified).

`public_addr` is undocumented (absent from README / `config.example.toml`), so the
rename's blast radius is limited to internal code/tests; no compatibility alias is
kept (pre-1.0, 0.4.0). The rename is recorded in `CHANGELOG.md`.

## 4. Relay bind/advertise (`src/server/udp.rs`)

### 4.1 New flow in `run()` (replaces `udp.rs:44-66`)

```
1. bind_ip = control.local_addr()?.ip()          // always a local interface IP
2. udp_sock = bind_with_retry(bind_ip, cfg.udp.port_range)?
      - None        => UdpSocket::bind((bind_ip, 0))         // unchanged ephemeral path
      - Some(range) => scan range, retry on AddrInUse/PermissionDenied
      - on exhaustion / fatal error: send SOCKS5 general failure (REP=0x01)
        reply + emit an event, then return                  // (was: silent return)
3. bnd_local = udp_sock.local_addr()?
4. advertise_ip = resolve_advertise_ip(cfg)               // [udp].advertise IP, else None
5. advertise = SocketAddr::new(advertise_ip.unwrap_or(bnd_local.ip()), bnd_local.port())
6. encode_reply(REP_SUCCEEDED, addr_from_socket(advertise), ...) -> control
7. ... relay loop unchanged (udp.rs:69-238) ...
```

Key invariant: **the advertised port is `bnd_local.port()` in every case** — the
real bound port the client must reach. `[udp].advertise` only swaps the IP.

`resolve_bind_ip` (`udp.rs:245-255`) is removed; its `public_addr`-as-bind logic
and the unspecified→loopback fallback are no longer needed (the control local IP
is always a concrete bindable interface IP). `parse_ip` (`udp.rs:257-263`) and
`addr_from_socket` (`udp.rs:265-271`) are kept.

### 4.2 New helpers

```rust
/// Advertised IP from `cfg.udp.advertise`: parsed IP, or None if unset /
/// unparseable / unspecified (we never advertise 0.0.0.0 or ::).
fn resolve_advertise_ip(cfg: &Config) -> Option<IpAddr>;

/// Bind within an inclusive port range, scanning from a rotating cursor and
/// retrying on AddrInUse / PermissionDenied; Err(AddrInUse) when the range is
/// exhausted. `None` range => single ephemeral bind (port 0).
async fn bind_with_retry(bind_ip: IpAddr, range: Option<PortRange>)
    -> std::io::Result<UdpSocket>;
```

Port selection (dependency-free): a module-level `static CURSOR: AtomicU32` gives
a rotating start offset so concurrent associations spread across the range instead
of all probing `start` first. Iterate at most `width = end - start + 1` candidate
ports; on success store the next cursor. `ErrorKind::AddrInUse` and
`ErrorKind::PermissionDenied` (ports < 1024 without `CAP_NET_BIND_SERVICE`) →
try next; any other error is fatal and returned.

### 4.3 Exhaustion behavior (small improvement)

Today a failed bind is a silent `return` (`udp.rs:53`). With a configured range,
exhaustion is a meaningful operator condition, so on bind failure we send a
best-effort SOCKS5 reply with `REP = 0x01` (general failure) and a zeroed BND
(same shape as `connection.rs::reply_failure`), and emit an `Event` (Log/Error),
then return. `encode_reply` is already imported in `udp.rs`.

## 5. Edge cases

- **IPv4/IPv6**: `PortRange`, `bind_with_retry`, and `resolve_advertise_ip` are
  IP-version agnostic (`IpAddr`). The bind IP follows the control connection's
  family; the advertised IP follows `[udp].advertise`'s family.
- **`start == end`**: a single fixed port. Only one association can bind it at a
  time; a second concurrent association fails to bind → exhaustion reply. This is
  the intended size-1 / PAT-NAT path (§6) and is documented, not forbidden.
- **`[udp].advertise` set but unparseable / unspecified**: ignored; advertise the
  bound address (graceful degradation, not a hard failure).
- **No `[udp]` section**: identical to current behavior (ephemeral bind, advertise
  bound addr). Zero behavior change.
- **Cursor races**: `AtomicU32` with `Relaxed` ordering; worst case is a slightly
  uneven scan start — never a correctness issue.

## 6. NAT / Docker behavior (drives the docs)

- Advertised port = real bound port ⇒ any NAT/forward must be **port-preserving
  (1:1)**: forward external `40000-40100/udp` → internal `40000-40100/udp`
  unchanged, plus the TCP control port. Set `[udp].advertise` to the public IP.
- **PAT / symmetric NAT** (public port ≠ internal port) cannot work with a
  multi-port range (the advertised internal port is wrong after translation). The
  only robust path is **range size 1** (`"40000-40000"`) + a 1:1 forward of that
  one port, or hosting on a directly reachable public IP.
- **Docker**: publish the whole UDP range + the TCP control port. `docker run -p
  1080:1080/tcp -p 40000-40100:40000-40100/udp`. In compose use **short syntax**
  (long syntax does not support port ranges — docker/compose#5613). Large ranges
  with the default userland proxy cost ~1 MB RAM and a `docker-proxy` process per
  port (moby#11185/#14288) — keep ranges small, or use `userland-proxy=false`, or
  `network_mode: host` (the install.sh compose already uses host networking).
- **Sizing**: range size ≥ expected concurrent UDP associations (each binds its
  own socket); size 1 serializes UDP.

## 7. Backward compatibility

| Surface | Impact |
| --- | --- |
| Existing configs without `[udp]` section | None — ephemeral bind, advertise bound addr, as today |
| `public_addr` config **key** | **Removed**; renamed to `[udp].advertise` (path change). Undocumented field, no alias kept — breaking, noted in CHANGELOG |
| Configs that set `public_addr` = a **local** IP | Now expressed as `[udp].advertise`; binds control local IP, advertises that IP (port = bound). Same observable result for reachable setups |
| Configs that set `public_addr` = a **non-local/NAT** IP | Previously failed (`EADDRNOTAVAIL`, UDP dead); now (`[udp].advertise`) works (binds local, advertises NAT IP). Strictly an improvement |
| Test/struct construction sites (`tests/integration.rs:48,228`, `tests/reproductions.rs:33,53`, `config.rs:325` default, `config.rs:396` assertion) set `public_addr: None` | Must change to `udp: UdpConfig::default()` (and drop the assertion or assert on `cfg.udp`) |

This is a **behavioral + config-path change** and must be called out in
`CHANGELOG.md` (0.4.0). It is fail-safe: broken NAT setups start working; working
default setups are unchanged.

## 8. Testing plan

Harness reuse: `start_server_with_config(cfg)`, `no_auth_config()`, the UDP echo
target and the `udp_associate_echo` flow at `tests/integration.rs:131-212`
(greeting `[5,1,0]` → `[5,0]`; ASSOCIATE `[5,3,0,1,0,0,0,0,0,0]`; BND parsed from
reply bytes `[4..8]` IP / `[8..10]` port).

**Unit (`src/config.rs` tests)**
- parse `[udp] port_range = "40000-40100"` → `Some((40000, 40100))`.
- default `None` / empty `UdpConfig` when `[udp]` omitted.
- reject malformed (`"5000"`, `"a-b"`, `"5000-4000"`, `"0-100"`).
- accept `"40000-40000"` (size 1).
- parse `[udp] advertise = "203.0.113.42"`.

**Integration (`tests/integration.rs`)**
- `udp_bnd_port_within_configured_range`: range `"40000-40100"`, assert parsed
  `bnd_port ∈ [40000, 40100]`.
- `udp_advertised_addr_uses_udp_advertise`: `[udp].advertise` set to an IP
  different from the listen IP; assert reply `BND.ADDR` == advertise while traffic
  still relays (echo) — advertise decoupled from bind.
- `udp_port_range_and_advertise_combined`: both set; BND.PORT in range, BND.ADDR
  == advertise, echo round-trips.
- Existing `udp_associate_echo` (ephemeral path) and all reproductions UDP tests
  remain unchanged and must pass (per-association isolation / source filtering /
  rate + target caps are untouched).

## 9. Documentation plan

- **`config.example.toml`**: add a `[udp]` section documenting `port_range`
  (range syntax, sizing, exhaustion → general failure) and `advertise`
  (advertise-only, NAT/Docker, port-ignored note).
- **`README.md`**: add a "UDP relay & NAT / Docker" subsection after the
  Configuration section (before CLI, ~line 265): bind-vs-advertise model, when to
  set `[udp].advertise`, `[udp].port_range` semantics + sizing, Docker publish
  (range + control port, short-syntax, host-networking note), 1:1 NAT forwarding,
  the size-1/PAT limitation, firewall snippets (ufw/iptables/nftables/AWS SG/GCP),
  and a troubleshooting checklist (advertised addr unreachable is the #1 cause).
- **`CHANGELOG.md`**: new `## [0.4.0]` entry — Added (`[udp].port_range`), Changed
  (`public_addr` → `[udp].advertise`, advertise-only / NAT-capable). Cut via the
  `release-version` skill.
- **`install.sh`** (decision: include): add a commented `[udp]` block to the
  rendered config (`render_config`, ~lines 152-157) and a bridge-mode `ports:`
  comment (short-syntax range + control port) in the generated
  `docker-compose.yml` (~lines 356-373); the compose keeps `network_mode: host`
  as the default.
- **CLAUDE.md / plan doc**: clarify the relay binds the control-conn local IP and
  `[udp].advertise` is advertise-only.

## 10. Change list (implementation anchors)

| File | Change |
| --- | --- |
| `src/config.rs` | add `PortRange` + custom `Deserialize`; add `UdpConfig`; add `udp: UdpConfig` to `Config` + `default_config()`; **remove** top-level `public_addr`; update the `public_addr: None` construction/assertion sites; unit tests |
| `src/server/udp.rs` | bind to control local IP; add `bind_with_retry` + rotating `AtomicU32` cursor; add `resolve_advertise_ip` (reads `cfg.udp.advertise`); advertise IP = advertise else bound, port = bound; failure reply on exhaustion; remove `resolve_bind_ip` |
| `tests/integration.rs` | swap `public_addr: None` → `udp: UdpConfig::default()`; 3 new UDP integration tests |
| `tests/reproductions.rs` | swap `public_addr: None` → `udp: UdpConfig::default()` |
| `config.example.toml` | add `[udp]` section (`port_range`, `advertise`) |
| `README.md` | new UDP/NAT/Docker deployment subsection |
| `CHANGELOG.md` | new `## [0.4.0]` entry |
| `install.sh` | commented `[udp]` config + compose `ports:` example |
| `Cargo.toml` | version bump to 0.4.0 (via `release-version`); no new deps |

## 11. Resolved decisions

1. **Config value form** — `"40000-40100"` string. ✓
2. **Placement** — dedicated `[udp]` section (consequently `public_addr` relocates
   to `[udp].advertise`). ✓
3. **`install.sh` edits** — included this change. ✓
4. **Target version** — 0.4.0. ✓
