//! UDP ASSOCIATE relay (RFC 1928 section 7).
//!
//! The association rides on a TCP control connection: the server binds an
//! ephemeral UDP socket reachable by the client, advertises it in the SOCKS
//! reply, then relays datagrams between the client and arbitrary targets until
//! the control connection closes or the association goes idle.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, watch};

use crate::config::Config;
use crate::error::Socks5Error;
use crate::metrics::{ConnKind, Event, Metrics};
use crate::protocol::address::Address;
use crate::protocol::reply::{encode_reply, REP_SUCCEEDED};
use crate::protocol::udp::decap_ref;

/// Bound on a single relay `send_to`, so a saturated local send buffer cannot
/// stall the select loop (and thus control-EOF detection / teardown).
const UDP_SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum size of a single UDP datagram we are willing to buffer (64 KiB).
const UDP_BUF: usize = 65536;

/// How long a resolved domain target stays cached. `lookup_host` does not
/// expose real record TTLs, so this is a conservative fixed window.
const DNS_CACHE_TTL: Duration = Duration::from_secs(30);

/// Maximum cached domain resolutions per association, so a client spraying
/// many distinct hostnames cannot grow the cache without bound.
const DNS_CACHE_CAP: usize = 256;

/// Bounded, TTL-based cache of resolved domain targets, so a client streaming
/// datagrams to the same hostname does not pay a resolver round trip (a
/// `spawn_blocking` `getaddrinfo` call) per datagram.
///
/// Insertion-order eviction once `cap` is reached. `order` may briefly hold
/// duplicate keys when an expired entry is re-inserted; eviction tolerates
/// that (a popped key no longer in `map` is skipped), at worst evicting a
/// fresh entry slightly early.
struct DnsCache {
    map: HashMap<String, (IpAddr, Instant)>,
    order: VecDeque<String>,
    cap: usize,
    ttl: Duration,
}

impl DnsCache {
    fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
            ttl,
        }
    }

    /// Return the cached IP for `host` if present and fresh; expired entries
    /// are removed on access.
    fn get(&mut self, host: &str, now: Instant) -> Option<IpAddr> {
        match self.map.get(host) {
            Some((ip, at)) if now.duration_since(*at) < self.ttl => Some(*ip),
            Some(_) => {
                self.map.remove(host);
                None
            }
            None => None,
        }
    }

    fn insert(&mut self, host: String, ip: IpAddr, now: Instant) {
        while self.map.len() >= self.cap {
            match self.order.pop_front() {
                // Skip keys already removed (expired or overwritten).
                Some(old) => {
                    self.map.remove(&old);
                }
                None => break,
            }
        }
        if self.map.insert(host.clone(), (ip, now)).is_none() {
            self.order.push_back(host);
        }
    }
}

/// Handle a UDP ASSOCIATE request that arrived on the TCP control connection.
///
/// Binds a server-side UDP socket reachable by the client, replies on the
/// control stream with the client-reachable BND.ADDR/PORT, then relays
/// datagrams until the control connection closes or the UDP idle timeout fires.
pub async fn run(
    mut control: TcpStream,
    client_peer: SocketAddr,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    // 1. Bind a per-association UDP relay socket on the control connection's
    //    local IP — a local interface the TCP handshake already succeeded on.
    let bind_ip = match control.local_addr() {
        Ok(addr) => addr.ip(),
        Err(_) => return,
    };

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

    // Register the association only after a successful reply.
    let id = metrics.register(client_peer, "udp-associate".into(), ConnKind::Udp);
    let _ = events.send(Event::Connect {
        id,
        src: client_peer,
        target: "udp-associate".into(),
        kind: ConnKind::Udp,
    });

    // 3. Relay state.
    let client_ip = client_peer.ip();
    // The client's actual UDP source, learned from its first datagram.
    let mut client_udp_addr: Option<SocketAddr> = None;
    // Targets we have forwarded to. Used to disambiguate inbound datagrams from
    // the client (src.ip() == client_ip) versus replies from a target: when the
    // target shares the client's IP (e.g. everything on 127.0.0.1), the source
    // IP alone is ambiguous, so a known-target match takes precedence.
    //
    // Bounded to `udp_max_targets`: `known_order` keeps insertion order so the
    // oldest target is evicted once the cap is hit, so a client spraying many
    // distinct destinations cannot grow this set without bound.
    let max_targets = cfg.limits.udp_max_targets.max(1);
    let mut known_targets: HashSet<SocketAddr> = HashSet::new();
    let mut known_order: VecDeque<SocketAddr> = VecDeque::new();

    // DNS for a domain target is bounded so a slow resolver cannot stall the
    // whole association (it shares this select loop with control-EOF detection),
    // and cached so a datagram stream to one hostname resolves once per TTL.
    let resolve_timeout = Duration::from_millis(cfg.timeouts.connect_ms);
    let mut dns_cache = DnsCache::new(DNS_CACHE_CAP, DNS_CACHE_TTL);

    // Optional outbound rate cap (datagrams/sec) via a 1-second fixed window.
    let rate_pps = cfg.limits.udp_rate_pps;
    let mut window_start = tokio::time::Instant::now();
    let mut window_count: u32 = 0;

    let idle = Duration::from_millis(cfg.timeouts.udp_idle_ms);
    let mut buf = vec![0u8; UDP_BUF];
    // Reusable scratch for re-encapsulating target replies (this loop is the
    // association's only task, so one buffer suffices — no per-datagram alloc).
    let mut framed: Vec<u8> = Vec::with_capacity(UDP_BUF + 32);
    // Scratch buffer for the control-channel read; its contents are ignored,
    // we only watch for EOF/error to detect the client tearing down.
    let mut ctrl_buf = [0u8; 1];

    loop {
        tokio::select! {
            // Branch A: a UDP datagram arrived on the relay socket.
            recv = tokio::time::timeout(idle, udp_sock.recv_from(&mut buf)) => {
                let (n, src) = match recv {
                    // Idle timeout elapsed: reclaim the association.
                    Err(_) => break,
                    Ok(Ok((n, src))) => (n, src),
                    // Socket error: tear down.
                    Ok(Err(_)) => break,
                };

                // Classify the source. The client's full ip:port is locked on
                // first contact; afterwards only that exact address counts as the
                // client, so another host sharing the client IP cannot hijack the
                // association or inject outbound datagrams.
                let is_client = match client_udp_addr {
                    Some(addr) => src == addr,
                    None => src.ip() == client_ip,
                };

                if is_client {
                    // Client -> target datagram. Lock the client's UDP source.
                    if client_udp_addr.is_none() {
                        client_udp_addr = Some(src);
                    }
                    let datagram = match decap_ref(&buf[..n]) {
                        Ok(dg) => dg,
                        // Malformed datagram: drop it.
                        Err(_) => continue,
                    };
                    // Fragmentation is not supported: drop FRAG != 0.
                    if datagram.frag != 0 {
                        continue;
                    }
                    // Optional outbound rate limit (reflection/flood guard).
                    // Counts successful sends, so dropped/blocked datagrams do
                    // not consume the budget; the over-limit check still gates
                    // before the resolve work.
                    if let Some(limit) = rate_pps {
                        if window_start.elapsed() >= Duration::from_secs(1) {
                            window_start = tokio::time::Instant::now();
                            window_count = 0;
                        }
                        if window_count >= limit {
                            continue;
                        }
                    }
                    // IP literals need no resolution (and no timer); domains
                    // consult the cache before a timeout-bounded lookup.
                    let target = match &datagram.address {
                        Address::V4(ip, port) => SocketAddr::new(IpAddr::V4(*ip), *port),
                        Address::V6(ip, port) => SocketAddr::new(IpAddr::V6(*ip), *port),
                        Address::Domain(host, port) => {
                            let now = Instant::now();
                            match dns_cache.get(host, now) {
                                Some(ip) => SocketAddr::new(ip, *port),
                                None => {
                                    let resolved = tokio::time::timeout(
                                        resolve_timeout,
                                        tokio::net::lookup_host((host.as_str(), *port)),
                                    )
                                    .await;
                                    match resolved {
                                        Ok(Ok(mut it)) => match it.next() {
                                            Some(sa) => {
                                                dns_cache.insert(host.clone(), sa.ip(), now);
                                                sa
                                            }
                                            None => continue,
                                        },
                                        // Unresolvable target or DNS too slow:
                                        // drop the datagram (never cached).
                                        _ => continue,
                                    }
                                }
                            }
                        }
                    };
                    // Egress policy: never relay to internal/metadata addresses.
                    if cfg.egress.is_blocked(target.ip()) {
                        continue;
                    }
                    let sent = matches!(
                        tokio::time::timeout(
                            UDP_SEND_TIMEOUT,
                            udp_sock.send_to(datagram.data, target),
                        )
                        .await,
                        Ok(Ok(_))
                    );
                    if sent {
                        // Count only successful sends against the rate budget.
                        if rate_pps.is_some() {
                            window_count += 1;
                        }
                        // Track the target (bounded; evict the oldest over cap).
                        if known_targets.insert(target) {
                            known_order.push_back(target);
                            if known_order.len() > max_targets {
                                if let Some(old) = known_order.pop_front() {
                                    known_targets.remove(&old);
                                }
                            }
                        }
                        metrics.add_up(id, datagram.data.len() as u64);
                    }
                } else if known_targets.contains(&src) {
                    // Target -> client reply. Re-encapsulate and forward to the
                    // client's learned UDP source (if any).
                    if let Some(dst) = client_udp_addr {
                        framed.clear();
                        crate::protocol::udp::encap(&addr_from_socket(src), &buf[..n], &mut framed);
                        // Bounded so a saturated local send buffer cannot stall
                        // the select loop (and control-EOF detection).
                        let _ =
                            tokio::time::timeout(UDP_SEND_TIMEOUT, udp_sock.send_to(&framed, dst))
                                .await;
                        metrics.add_down(id, n as u64);
                    }
                }
                // Otherwise: source is neither the client nor a known target
                // (injection / spoofing). Drop it. This is source filtering.
            }

            // Branch B: the TCP control connection produced data or closed. A
            // read of 0 bytes (EOF) or an error means the client tore down the
            // association, so we stop relaying.
            res = control.read(&mut ctrl_buf) => {
                match res {
                    Ok(0) | Err(_) => break,
                    // Unexpected data on the control channel is ignored; keep
                    // relaying as long as the connection stays open.
                    Ok(_) => {}
                }
            }

            // Branch C: server shutdown requested; tear the association down.
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    // 4. Tear down: drop the UDP socket (auto-reclaims the port) and record the
    //    association as finished.
    metrics.record_success();
    metrics.unregister(id);
    let _ = events.send(Event::Closed { id });
}

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

/// Advertised BND IP for UDP ASSOCIATE replies: the configured `[udp].advertise`
/// IP when set and usable, else `None` (the caller falls back to the bound IP).
/// The value is validated to a real IP at config load; an unspecified address
/// (`0.0.0.0` / `::`) is rejected here — never advertised.
fn resolve_advertise_ip(cfg: &Config) -> Option<IpAddr> {
    let ip = cfg.udp.advertise?;
    if ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}

/// Build a SOCKS5 [`Address`] from a [`SocketAddr`].
fn addr_from_socket(sa: SocketAddr) -> Address {
    match sa {
        SocketAddr::V4(v4) => Address::V4(*v4.ip(), v4.port()),
        SocketAddr::V6(v6) => Address::V6(*v6.ip(), v6.port()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn dns_cache_hit_within_ttl() {
        let mut c = DnsCache::new(4, Duration::from_secs(30));
        let t0 = Instant::now();
        c.insert("example.com".into(), ip("93.184.216.34"), t0);
        assert_eq!(
            c.get("example.com", t0 + Duration::from_secs(29)),
            Some(ip("93.184.216.34"))
        );
    }

    #[test]
    fn dns_cache_expires_after_ttl() {
        let mut c = DnsCache::new(4, Duration::from_secs(30));
        let t0 = Instant::now();
        c.insert("example.com".into(), ip("93.184.216.34"), t0);
        assert_eq!(c.get("example.com", t0 + Duration::from_secs(30)), None);
        // The expired entry was removed on access, not just hidden.
        assert!(c.map.is_empty());
    }

    #[test]
    fn dns_cache_misses_unknown_host() {
        let mut c = DnsCache::new(4, Duration::from_secs(30));
        assert_eq!(c.get("nope.invalid", Instant::now()), None);
    }

    #[test]
    fn dns_cache_evicts_oldest_at_cap() {
        let mut c = DnsCache::new(2, Duration::from_secs(30));
        let t0 = Instant::now();
        c.insert("a.com".into(), ip("192.0.2.1"), t0);
        c.insert("b.com".into(), ip("192.0.2.2"), t0);
        c.insert("c.com".into(), ip("192.0.2.3"), t0);
        assert_eq!(c.get("a.com", t0), None);
        assert_eq!(c.get("b.com", t0), Some(ip("192.0.2.2")));
        assert_eq!(c.get("c.com", t0), Some(ip("192.0.2.3")));
        assert!(c.map.len() <= 2);
    }

    #[test]
    fn dns_cache_reinsert_after_expiry_tolerates_duplicate_order_keys() {
        let mut c = DnsCache::new(2, Duration::from_secs(30));
        let t0 = Instant::now();
        c.insert("a.com".into(), ip("192.0.2.1"), t0);
        // Expire and re-insert the same host: `order` now holds "a.com" twice.
        assert_eq!(c.get("a.com", t0 + Duration::from_secs(31)), None);
        c.insert("a.com".into(), ip("192.0.2.9"), t0 + Duration::from_secs(31));
        c.insert("b.com".into(), ip("192.0.2.2"), t0 + Duration::from_secs(31));
        c.insert("c.com".into(), ip("192.0.2.3"), t0 + Duration::from_secs(31));
        // Never exceeds the cap and stays consistent.
        assert!(c.map.len() <= 2);
        assert_eq!(
            c.get("c.com", t0 + Duration::from_secs(31)),
            Some(ip("192.0.2.3"))
        );
    }
}
