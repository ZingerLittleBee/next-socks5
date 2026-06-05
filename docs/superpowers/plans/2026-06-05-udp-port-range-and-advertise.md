# UDP port range + bind/advertise decoupling — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional `[udp].port_range` to bind each UDP association's relay socket inside a known port range, and decouple the advertised `BND.ADDR` (`[udp].advertise`, formerly `public_addr`) from the bound address so servers behind NAT/Docker can advertise a client-reachable IP.

**Architecture:** Keep the per-association socket model untouched (one UDP socket per association, kernel demux, source filtering, lifetime tied to the TCP control connection). Only the bind-port selection and the advertised-address derivation change in `src/server/udp.rs`, driven by a new `[udp]` config section in `src/config.rs`. No new dependencies.

**Tech Stack:** Rust, tokio (`UdpSocket`), serde/toml, `std::sync::atomic::AtomicU32`.

**Spec:** `docs/superpowers/specs/2026-06-05-udp-port-range-and-advertise-design.md`

---

## File structure

| File | Responsibility | Change |
| --- | --- | --- |
| `src/config.rs` | config types + TOML parsing | add `PortRange` + `UdpConfig`; add `udp` to `Config`; remove `public_addr`; unit tests |
| `src/server/udp.rs` | UDP ASSOCIATE relay | bind on control-local IP within range; advertise decoupled; exhaustion reply |
| `tests/integration.rs` | full-path integration tests | swap construction site; 2 new UDP tests |
| `tests/reproductions.rs` | security regression tests | swap construction site only |
| `config.example.toml` | example config | document `[udp]` |
| `README.md` | operator docs | UDP/NAT/Docker subsection |
| `CHANGELOG.md` | release notes | `## [0.4.0]` entry |
| `install.sh` | installer | commented `[udp]` config + compose `ports:` example |

---

## Task 1: `PortRange` type with `"start-end"` parsing

**Files:**
- Modify: `src/config.rs` (add type before `Limits`, ~line 123; tests in the `tests` module ~line 348)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/config.rs` (after the last existing test, before the closing `}`):

```rust
#[test]
fn port_range_parses_valid() {
    assert_eq!(
        PortRange::parse("40000-40100"),
        Ok(PortRange { start: 40000, end: 40100 })
    );
}

#[test]
fn port_range_allows_single_port() {
    assert_eq!(
        PortRange::parse("40000-40000"),
        Ok(PortRange { start: 40000, end: 40000 })
    );
}

#[test]
fn port_range_rejects_malformed() {
    assert!(PortRange::parse("5000").is_err()); // no dash
    assert!(PortRange::parse("a-b").is_err()); // non-numeric
    assert!(PortRange::parse("5000-4000").is_err()); // start > end
    assert!(PortRange::parse("0-100").is_err()); // start port 0
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib port_range`
Expected: FAIL — compile error `cannot find type/function PortRange`.

- [ ] **Step 3: Implement `PortRange`**

Add to `src/config.rs` immediately before `pub struct Limits` (~line 123):

```rust
/// An inclusive UDP relay port range `[start, end]` for binding association
/// sockets. `start == end` is a single fixed port. Deserialized from a
/// `"start-end"` string (e.g. `"40000-40100"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    /// First port in the range (inclusive); must be >= 1.
    pub start: u16,
    /// Last port in the range (inclusive); must be >= `start`.
    pub end: u16,
}

impl PortRange {
    /// Parse an inclusive `"start-end"` range. Rejects a missing dash,
    /// non-numeric bounds, port 0 as `start`, and `start > end`.
    fn parse(s: &str) -> Result<PortRange, String> {
        let (start, end) = s
            .split_once('-')
            .ok_or_else(|| format!("expected 'start-end', got {s:?}"))?;
        let start: u16 = start
            .trim()
            .parse()
            .map_err(|_| format!("invalid start port {start:?}"))?;
        let end: u16 = end
            .trim()
            .parse()
            .map_err(|_| format!("invalid end port {end:?}"))?;
        if start == 0 {
            return Err("start port must be >= 1".to_string());
        }
        if start > end {
            return Err(format!("start {start} must be <= end {end}"));
        }
        Ok(PortRange { start, end })
    }
}

impl<'de> serde::Deserialize<'de> for PortRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        PortRange::parse(&s).map_err(serde::de::Error::custom)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib port_range`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add PortRange type with start-end parsing"
```

---

## Task 2: `[udp]` config section; relocate `public_addr` → `[udp].advertise`

**Files:**
- Modify: `src/config.rs` (add `UdpConfig`; swap `Config.public_addr` → `Config.udp`; update `default_config`; update test at ~line 396; new tests)
- Modify: `src/server/udp.rs` (field path only: `cfg.public_addr` → `cfg.udp.advertise`)
- Modify: `tests/integration.rs` (2 construction sites)
- Modify: `tests/reproductions.rs` (2 construction sites)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `src/config.rs`:

```rust
#[test]
fn parses_udp_section() {
    let cfg = Config::from_toml_str(
        "listen = \"x\"\n[udp]\nport_range = \"40000-40100\"\nadvertise = \"203.0.113.42\"",
    )
    .expect("should parse");
    assert_eq!(
        cfg.udp.port_range,
        Some(PortRange { start: 40000, end: 40100 })
    );
    assert_eq!(cfg.udp.advertise.as_deref(), Some("203.0.113.42"));
}

#[test]
fn udp_section_defaults_empty() {
    let cfg = Config::from_toml_str("listen = \"x\"").expect("should parse");
    assert_eq!(cfg.udp, UdpConfig::default());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib udp_section parses_udp_section`
Expected: FAIL — compile error `no field udp on Config` / `cannot find UdpConfig`.

- [ ] **Step 3: Add `UdpConfig` and wire it into `Config`**

In `src/config.rs`, add the `UdpConfig` type immediately after the `PortRange` block from Task 1:

```rust
/// UDP relay transport/addressing configuration.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq, Default)]
pub struct UdpConfig {
    /// Bind each association's relay socket inside this inclusive port range.
    /// `None` => OS-assigned ephemeral port.
    #[serde(default)]
    pub port_range: Option<PortRange>,
    /// Advertised BND.ADDR IP for UDP ASSOCIATE replies (advertise-only; the
    /// advertised port is always the real bound port). `None` => advertise the
    /// bound address. Needed behind NAT/Docker.
    #[serde(default)]
    pub advertise: Option<String>,
}
```

In the `Config` struct, replace the `public_addr` field (currently around lines 23-25):

```rust
    /// Advertised BND address for UDP ASSOCIATE replies (optional).
    #[serde(default)]
    pub public_addr: Option<String>,
```

with:

```rust
    /// UDP relay transport/addressing configuration.
    #[serde(default)]
    pub udp: UdpConfig,
```

- [ ] **Step 4: Update `default_config` and the existing default test**

In `Config::default_config()` replace `public_addr: None,` with `udp: UdpConfig::default(),`.

In the existing test `defaults_fill_when_sections_omitted`, replace the line
`assert_eq!(cfg.public_addr, None);` with:

```rust
        assert_eq!(cfg.udp, UdpConfig::default());
```

- [ ] **Step 5: Update the UDP relay field path (keep it compiling)**

In `src/server/udp.rs`, function `resolve_bind_ip`, change only the matched field
from `&cfg.public_addr` to `&cfg.udp.advertise`:

```rust
    let ip = match &cfg.udp.advertise {
        Some(s) => parse_ip(s)?,
        None => control.local_addr().ok()?.ip(),
    };
```

(This is a transient one-line change; Task 3 rewrites this function away.)

- [ ] **Step 6: Update the test construction sites**

In `tests/integration.rs`, in BOTH `no_auth_config()` and `password_config()`,
replace the line `public_addr: None,` with (matching the adjacent
`admin: Default::default(),` style):

```rust
        udp: Default::default(),
```

In `tests/reproductions.rs`, in BOTH `no_auth_config()` and `password_config()`,
make the same replacement: `public_addr: None,` → `udp: Default::default(),`.

- [ ] **Step 7: Run the new tests and the full suite**

Run: `cargo test --lib udp_section parses_udp_section`
Expected: PASS.

Run: `cargo test`
Expected: PASS — all existing unit + integration + reproduction tests still green (pure refactor, no behavior change).

- [ ] **Step 8: Commit**

```bash
git add src/config.rs src/server/udp.rs tests/integration.rs tests/reproductions.rs
git commit -m "feat(config)!: add [udp] section; move public_addr to udp.advertise"
```

---

## Task 3: Decouple advertised BND address from the bound address

**Files:**
- Modify: `src/server/udp.rs` (`run` bind/advertise block; remove `resolve_bind_ip`; add `resolve_advertise_ip`)
- Test: `tests/integration.rs` (new `udp_advertised_addr_uses_udp_advertise`)

- [ ] **Step 1: Write the failing test**

Add to `tests/integration.rs` (after the `udp_associate_echo` test, ~line 212):

```rust
#[tokio::test]
async fn udp_advertised_addr_uses_udp_advertise() {
    let scenario = async {
        // Advertise a non-local public IP. Binding must still succeed (on the
        // control connection's local IP), and the reply must carry the
        // advertised IP — proving advertise is decoupled from bind.
        let mut cfg = no_auth_config();
        cfg.udp.advertise = Some("203.0.113.9".to_string());
        let proxy_addr = start_server_with_config(cfg).await;

        let mut control = TcpStream::connect(proxy_addr).await.unwrap();
        control.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        control.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        control
            .write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        control.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0x00, "expected success reply code");

        let bnd_ip = std::net::Ipv4Addr::new(reply[4], reply[5], reply[6], reply[7]);
        assert_eq!(
            bnd_ip,
            std::net::Ipv4Addr::new(203, 0, 113, 9),
            "BND.ADDR must be the configured advertise IP, not the bound IP"
        );

        drop(control);
    };
    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("udp advertise scenario timed out");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test integration udp_advertised_addr_uses_udp_advertise`
Expected: FAIL — with Task 2's code the server tries to **bind** `203.0.113.9`
(not a local interface), bind fails, no success reply is sent, and
`read_exact` errors / times out.

- [ ] **Step 3: Rewrite the bind/advertise block in `run`**

In `src/server/udp.rs`, replace the current block (the comment "1. Determine the
IP the client can reach us on…" through the `control.write_all(&out)` reply, i.e.
the lines that call `resolve_bind_ip`, `UdpSocket::bind((bind_ip, 0))`, build
`bnd_local`, and send the success reply) with:

```rust
    // 1. Bind a per-association UDP relay socket on the control connection's
    //    local IP — a local interface the TCP handshake already succeeded on.
    let bind_ip = match control.local_addr() {
        Ok(addr) => addr.ip(),
        Err(_) => return,
    };

    let udp_sock = match UdpSocket::bind((bind_ip, 0)).await {
        Ok(sock) => sock,
        Err(_) => return,
    };
    let bnd_local = match udp_sock.local_addr() {
        Ok(addr) => addr,
        Err(_) => return,
    };

    // 2. Advertise BND.ADDR/PORT: the configured advertise IP (for NAT/Docker)
    //    when set, else the bound IP. The advertised PORT is always the real
    //    bound port — where the client must send its datagrams.
    let advertise_ip = resolve_advertise_ip(&cfg).unwrap_or_else(|| bnd_local.ip());
    let bnd_address = addr_from_socket(SocketAddr::new(advertise_ip, bnd_local.port()));
    let mut out = Vec::with_capacity(22);
    encode_reply(REP_SUCCEEDED, &bnd_address, &mut out);
    if control.write_all(&out).await.is_err() {
        return;
    }
```

- [ ] **Step 4: Replace `resolve_bind_ip` with `resolve_advertise_ip`**

In `src/server/udp.rs`, delete the entire `resolve_bind_ip` function (its doc
comment + body) and add, next to `parse_ip`:

```rust
/// Advertised BND IP for UDP ASSOCIATE replies: the configured `[udp].advertise`
/// IP when set and usable, else `None` (the caller falls back to the bound IP).
/// An unspecified address (`0.0.0.0` / `::`) is rejected — never advertised.
fn resolve_advertise_ip(cfg: &Config) -> Option<IpAddr> {
    let ip = parse_ip(cfg.udp.advertise.as_deref()?)?;
    if ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}
```

`parse_ip` and `addr_from_socket` are unchanged. (`Ipv4Addr` may now be unused in
imports; if the compiler warns, it is re-used in Task 4 — leave the import.)

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --test integration udp_advertised_addr_uses_udp_advertise`
Expected: PASS.

Run: `cargo test --test integration udp_associate_echo`
Expected: PASS — the no-advertise path still binds the control-local IP
(127.0.0.1) and advertises it (BND ≠ 0.0.0.0).

- [ ] **Step 6: Commit**

```bash
git add src/server/udp.rs tests/integration.rs
git commit -m "feat(udp): decouple advertised BND address from bound address"
```

---

## Task 4: Configurable UDP port range with bind-and-retry

**Files:**
- Modify: `src/server/udp.rs` (use `bind_with_retry`; add cursor + helpers; exhaustion reply; import `Socks5Error`)
- Test: `tests/integration.rs` (new `udp_bnd_port_within_configured_range`)

- [ ] **Step 1: Write the failing test**

Add to `tests/integration.rs` (after the Task 3 test):

```rust
#[tokio::test]
async fn udp_bnd_port_within_configured_range() {
    let scenario = async {
        let echo_addr = spawn_udp_echo_server().await;
        let echo_v4 = match echo_addr.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => panic!("expected v4 echo addr"),
        };

        let mut cfg = no_auth_config();
        cfg.udp.port_range = Some(next_socks5::config::PortRange { start: 41000, end: 41050 });
        let proxy_addr = start_server_with_config(cfg).await;

        let mut control = TcpStream::connect(proxy_addr).await.unwrap();
        control.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        control.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        control
            .write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        control.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], 0x00, "expected success reply code");

        let bnd_ip = std::net::Ipv4Addr::new(reply[4], reply[5], reply[6], reply[7]);
        let bnd_port = u16::from_be_bytes([reply[8], reply[9]]);
        assert!(
            (41000..=41050).contains(&bnd_port),
            "BND.PORT {bnd_port} not in configured range"
        );
        let relay_udp_addr = std::net::SocketAddr::from((bnd_ip, bnd_port));

        // The relay still functions on a ranged port.
        let client_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut out = Vec::new();
        udp::encap(&Address::V4(echo_v4, echo_addr.port()), b"hello", &mut out);
        client_udp.send_to(&out, relay_udp_addr).await.unwrap();
        let mut buf = [0u8; 65536];
        let (n, _src) = client_udp.recv_from(&mut buf).await.unwrap();
        let datagram = udp::decap(&buf[..n]).expect("valid SOCKS5 UDP datagram");
        assert_eq!(datagram.data, b"hello");

        drop(control);
    };
    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("udp port range scenario timed out");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --test integration udp_bnd_port_within_configured_range`
Expected: FAIL — the bind still uses port `0` (ephemeral), so `bnd_port` is a
random high port outside `41000..=41050` and the `contains` assertion fails.

- [ ] **Step 3: Add the `Socks5Error` import**

In `src/server/udp.rs`, add to the imports near the other `use crate::...` lines:

```rust
use crate::error::Socks5Error;
```

- [ ] **Step 4: Use `bind_with_retry` and reply on exhaustion**

In `run`, replace the ephemeral bind from Task 3:

```rust
    let udp_sock = match UdpSocket::bind((bind_ip, 0)).await {
        Ok(sock) => sock,
        Err(_) => return,
    };
```

with the range-aware bind plus a failure reply on exhaustion:

```rust
    let udp_sock = match bind_with_retry(bind_ip, cfg.udp.port_range).await {
        Ok(sock) => sock,
        // Range exhausted or a fatal bind error: tell the client instead of
        // dropping the request silently.
        Err(_) => {
            reply_general_failure(&mut control).await;
            let _ = events.send(Event::Error {
                code: Socks5Error::General.reply_code(),
                msg: "udp relay bind failed (port range exhausted?)".to_string(),
            });
            return;
        }
    };
```

- [ ] **Step 5: Add the bind/retry helpers**

In `src/server/udp.rs`, add (near `resolve_advertise_ip`):

```rust
/// Rotating start offset so concurrent associations spread across the configured
/// port range instead of all probing the first port.
static PORT_CURSOR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Bind a UDP relay socket on `bind_ip`. With no range, the OS assigns an
/// ephemeral port. With a range, scan the inclusive `[start, end]` from a
/// rotating cursor, skipping in-use / privileged ports, and error only when
/// every port in the range is unavailable.
async fn bind_with_retry(
    bind_ip: IpAddr,
    range: Option<crate::config::PortRange>,
) -> std::io::Result<UdpSocket> {
    use std::sync::atomic::Ordering;
    let range = match range {
        None => return UdpSocket::bind((bind_ip, 0)).await,
        Some(r) => r,
    };
    let width = (range.end - range.start) as u32 + 1;
    let base = PORT_CURSOR.fetch_add(1, Ordering::Relaxed);
    for i in 0..width {
        let port = range.start + (base.wrapping_add(i) % width) as u16;
        match UdpSocket::bind((bind_ip, port)).await {
            Ok(sock) => return Ok(sock),
            Err(e) => match e.kind() {
                // Port taken, or privileged (<1024 without CAP_NET_BIND_SERVICE):
                // try the next candidate.
                std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied => {
                    continue
                }
                // Anything else (e.g. address not available) is fatal.
                _ => return Err(e),
            },
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        "udp_port_range exhausted",
    ))
}

/// Send a best-effort SOCKS5 general-failure reply (REP=0x01) with a zeroed IPv4
/// BND, used when the relay socket cannot be bound.
async fn reply_general_failure(control: &mut TcpStream) {
    let bnd = Address::V4(Ipv4Addr::UNSPECIFIED, 0);
    let mut out = Vec::with_capacity(10);
    encode_reply(Socks5Error::General.reply_code(), &bnd, &mut out);
    let _ = control.write_all(&out).await;
}
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test --test integration udp_bnd_port_within_configured_range`
Expected: PASS.

- [ ] **Step 7: Run the full suite**

Run: `cargo test`
Expected: PASS — all tests green.

- [ ] **Step 8: Commit**

```bash
git add src/server/udp.rs tests/integration.rs
git commit -m "feat(udp): add configurable udp port range with bind retry"
```

---

## Task 5: Documentation (`config.example.toml`, README, CHANGELOG, install.sh)

**Files:**
- Modify: `config.example.toml`
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `install.sh`

- [ ] **Step 1: Document `[udp]` in `config.example.toml`**

Insert a `[udp]` section between the `[limits]` block and the egress comment.
Replace:

```toml
# udp_rate_pps = 5000      # optional: outbound datagrams/sec per association

# Egress policy guards against SSRF / open-relay abuse. Secure by default: all
```

with:

```toml
# udp_rate_pps = 5000      # optional: outbound datagrams/sec per association

[udp]
# Bind each UDP association's relay socket inside an inclusive port range instead
# of an OS-assigned ephemeral port, so a firewall/NAT only needs that range open.
# Size the range >= expected concurrent UDP associations (each binds its own
# socket); "40000-40000" is a single port and serializes UDP. When the range is
# exhausted, UDP ASSOCIATE replies with a general failure (0x01).
# port_range = "40000-40100"
#
# Advertised BND.ADDR (IP only) returned in UDP ASSOCIATE replies. By default the
# server advertises the IP the client's TCP control connection arrived on.
# Override it when that IP is not client-reachable (behind NAT, or Docker with
# bridge networking): set the public/forwarded IP clients will use. The advertised
# PORT is always the real bound port, so any NAT/forward must be port-preserving
# (1:1). A port in "ip:port" form is ignored.
# advertise = "203.0.113.42"

# Egress policy guards against SSRF / open-relay abuse. Secure by default: all
```

- [ ] **Step 2: Add the README subsection**

In `README.md`, insert the following immediately before the `### CLI` heading
(after the "Secure defaults." paragraph):

````markdown
### UDP relay & NAT / Docker

`CONNECT` works over the single TCP listen port, but **UDP ASSOCIATE** uses a
separate UDP relay socket. By default each association binds an OS-assigned
ephemeral UDP port and the server advertises a `BND.ADDR:BND.PORT` that the client
**must** send its datagrams to (RFC 1928). Two `[udp]` options make this work
through firewalls and NAT:

```toml
[udp]
port_range = "40000-40100"   # bind UDP relay sockets to this inclusive range
advertise  = "203.0.113.42"  # advertised BND IP (a client-reachable address)
```

- **`port_range`** — bind each association's UDP socket inside a known range
  instead of a random ephemeral port, so a firewall/NAT only needs that range
  opened. Each association binds its own socket, so size the range **≥ your
  expected concurrent UDP clients**; `"40000-40000"` is a single port and
  serializes UDP. When the range is exhausted, UDP ASSOCIATE returns a general
  failure.
- **`advertise`** — the IP put in the UDP ASSOCIATE reply. By default the server
  advertises the IP the client's TCP connection arrived on; override it when that
  IP is not client-reachable (behind NAT, or Docker bridge networking). The
  advertised **port is always the real bound port**, so any NAT/forward must be
  **port-preserving (1:1)**. An unreachable advertised address is the #1 cause of
  "TCP works but UDP doesn't".

**Docker.** The provided compose uses `network_mode: host` (Linux), which needs no
port mapping. For bridge networking, publish the TCP control port and the UDP
range with **short syntax** (Compose long syntax has no range support) and set
`advertise` to the host's public IP:

```yaml
ports:
  - "1080:1080/tcp"
  - "40000-40100:40000-40100/udp"
```

Keep the range small with the default userland proxy (Docker spawns one
`docker-proxy` process per published port); for large ranges set
`userland-proxy=false` or use host networking.

**Firewall** (range `40000-40100/udp` + control `1080/tcp`):

```bash
# ufw
ufw allow 1080/tcp && ufw allow 40000:40100/udp
# nftables
nft add rule inet filter input udp dport 40000-40100 accept
# iptables
iptables -A INPUT -p udp --dport 40000:40100 -j ACCEPT
```

**Port-remapping (PAT / symmetric) NAT** cannot work with a multi-port range (the
advertised internal port is wrong after translation). Use a single fixed port
(`"40000-40000"`) with a 1:1 forward of that one port, or host on a directly
reachable public IP.

````

- [ ] **Step 3: Add the CHANGELOG entry**

In `CHANGELOG.md`, insert immediately above the `## [0.3.2] - 2026-06-05` heading:

```markdown
## [0.4.0] - 2026-06-05

### Added

- `[udp].port_range` config option (e.g. `port_range = "40000-40100"`) to bind
  each UDP association's relay socket inside an inclusive port range instead of an
  OS-assigned ephemeral port — useful behind firewalls/NAT that only forward a
  known range. When the range is exhausted, UDP ASSOCIATE returns a general
  failure reply instead of silently dropping the request.

### Changed

- UDP ASSOCIATE now binds the relay socket on the TCP control connection's local
  IP and advertises a separate address, decoupling bind from advertise. The former
  top-level `public_addr` option is renamed to `[udp].advertise` and is now
  advertise-only (it no longer affects which IP the socket binds), so a server
  behind NAT/Docker can advertise a client-reachable public IP while binding a
  local one. The advertised port is always the real bound port.

```

(The `release-version` skill finalizes the version/date when cutting the tag.)

- [ ] **Step 4: Add a commented `[udp]` block to the installer's rendered config**

In `install.sh`, in `render_config()`, after the `echo "udp_idle_ms = 60000"`
line and before the closing `}`, add:

```sh
  echo ""
  echo "[udp]"
  echo "# port_range = \"40000-40100\"      # bind UDP relay sockets to this range"
  echo "# advertise = \"YOUR_PUBLIC_IP\"    # advertised BND IP for clients behind NAT"
```

- [ ] **Step 5: Add a bridge-mode `ports:` example to the generated compose**

In `install.sh`, inside the `docker-compose.yml` heredoc, after the
`network_mode: host` line, add the commented alternative:

```sh
    # Alternative — bridge networking (e.g. Docker Desktop, where host mode is
    # limited): comment out network_mode above, publish the TCP control port plus
    # the configured UDP range (short syntax; long syntax has no range support),
    # and set [udp].advertise in config.toml to the host's public IP. Keep the UDP
    # range identical on both sides (port-preserving).
    #ports:
    #  - "${PORT}:${PORT}/tcp"
    #  - "40000-40100:40000-40100/udp"
```

- [ ] **Step 6: Verify docs build / installer still lints**

Run: `cargo build` (ensures nothing references the removed `public_addr`)
Expected: PASS.

Run: `sh -n install.sh`
Expected: no output (POSIX-sh syntax OK).

- [ ] **Step 7: Commit**

```bash
git add config.example.toml README.md CHANGELOG.md install.sh
git commit -m "docs: document udp port range and NAT/Docker deployment"
```

---

## Task 6: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Run the complete test suite**

Run: `cargo test`
Expected: PASS — all unit, integration, and reproduction tests green.

- [ ] **Step 2: Verify the headless (no-default-features) build**

Run: `cargo build --release --no-default-features`
Expected: PASS — confirms the change does not depend on the `tui` feature.

- [ ] **Step 3: Lint and format**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: PASS — no warnings.

Run: `cargo fmt --check`
Expected: PASS — already formatted (run `cargo fmt` and re-commit if not).

- [ ] **Step 4: Final commit (only if fmt/clippy required changes)**

```bash
git add -A
git commit -m "style: apply cargo fmt"
```

---

## Self-review notes (author)

- **Spec coverage:** `[udp].port_range` (Task 1+4), `[udp].advertise` rename + decouple (Task 2+3), exhaustion reply (Task 4), per-association model preserved (no shared-socket change), docs incl. NAT/Docker (Task 5), version 0.4.0 + CHANGELOG (Task 5). All spec §3–§10 items map to a task.
- **Type consistency:** `PortRange { start, end }` (Task 1) used identically in Tasks 2/4; `UdpConfig { port_range, advertise }` (Task 2) read as `cfg.udp.port_range` / `cfg.udp.advertise` in Tasks 3/4; `bind_with_retry` / `resolve_advertise_ip` / `reply_general_failure` defined and called consistently in Task 3/4.
- **No placeholders:** every code/test/command step contains complete content.
- **Version bump/tag:** the Cargo.toml version bump to 0.4.0 + git tag is handled by the `release-version` skill at release time (per CLAUDE.md), not hand-edited here; the CHANGELOG `## [0.4.0]` section it validates is added in Task 5.
