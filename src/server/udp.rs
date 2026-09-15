//! UDP ASSOCIATE relay (RFC 1928 section 7).
//!
//! The association rides on a TCP control connection: the server binds an
//! ephemeral UDP socket reachable by the client, advertises it in the SOCKS
//! reply, then relays datagrams between the client and arbitrary targets until
//! the control connection closes or the association goes idle.

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, watch};

use crate::config::{AdvertiseHost, Config};
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

/// Handle a UDP ASSOCIATE request that arrived on the TCP control connection.
///
/// Binds a server-side UDP socket reachable by the client, replies on the
/// control stream with the client-reachable BND.ADDR/PORT, then relays
/// datagrams until the control connection closes or the UDP idle timeout fires.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut control: TcpStream,
    client_peer: SocketAddr,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
    resolver: Arc<crate::dns::DnsResolver>,
) {
    // 1. Bind a per-association UDP relay socket on the control connection's
    //    local IP — a local interface the TCP handshake already succeeded on.
    let bind_ip = match control.local_addr() {
        // Dual-stack listeners report IPv4 peers through IPv4-mapped IPv6
        // addresses. Bind an IPv4 relay for those clients so DNS advertise
        // selection and the actual UDP socket use the same address family.
        Ok(addr) => addr.ip().to_canonical(),
        Err(_) => return,
    };

    let udp_sock = match bind_with_retry(bind_ip, cfg.udp.port_range).await {
        Ok(sock) => sock,
        // Range exhausted or a fatal bind error: tell the client instead of
        // dropping the request silently.
        Err(_) => {
            reply_failure(&mut control, Socks5Error::General).await;
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

    // 2. Advertise BND.ADDR/PORT: the configured IP or per-association DNS
    //    result (for NAT/Docker), else the bound IP. The advertised PORT is
    //    always the real bound port — where the client sends its datagrams.
    let advertise_ip = match resolve_advertise_ip(&cfg, bnd_local.ip(), &resolver).await {
        Ok(Some(ip)) => ip,
        Ok(None) => bnd_local.ip(),
        Err(error) => {
            reply_failure(&mut control, Socks5Error::HostUnreachable).await;
            metrics.record_error(Socks5Error::HostUnreachable.reply_code());
            let _ = events.send(Event::Error {
                code: Socks5Error::HostUnreachable.reply_code(),
                msg: format!("could not resolve UDP advertise host: {error}"),
            });
            return;
        }
    };
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
    let client_ip = client_peer.ip().to_canonical();
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
    // with shared, bounded caches that respect the DNS record TTL.
    let resolve_timeout = Duration::from_millis(cfg.timeouts.connect_ms);
    // Limit error events per association to avoid a failing DNS packet flood.
    let mut last_dns_error: Option<Instant> = None;

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
                            match resolver.lookup(host, *port, Instant::now() + resolve_timeout, bind_ip.is_ipv4()).await {
                                Ok(addresses) => match addresses.into_iter().find(|address| {
                                    address.ip().is_ipv4() == bind_ip.is_ipv4() && !cfg.egress.is_blocked(address.ip())
                                }) {
                                    Some(address) => address,
                                    None => continue,
                                },
                                Err(error) => {
                                    let now = Instant::now();
                                    if last_dns_error.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1)) {
                                        let _ = events.send(Event::Error {
                                            code: Socks5Error::HostUnreachable.reply_code(),
                                            msg: format!("could not resolve UDP target {host}:{port}: {error}"),
                                        });
                                        last_dns_error = Some(now);
                                    }
                                    continue;
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
                std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied => continue,
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

/// Send a best-effort SOCKS5 failure reply with a zeroed IPv4 BND.
async fn reply_failure(control: &mut TcpStream, error: Socks5Error) {
    let bnd = Address::V4(Ipv4Addr::UNSPECIFIED, 0);
    let mut out = Vec::with_capacity(10);
    encode_reply(error.reply_code(), &bnd, &mut out);
    let _ = control.write_all(&out).await;
}

/// Resolve the configured UDP advertise host for a new association. Domain
/// names are resolved here, rather than at startup, so DDNS changes take effect
/// without restarting the server. The result must match the relay socket's
/// address family; advertising an address in another family would be unusable.
async fn resolve_advertise_ip(
    cfg: &Config,
    bind_ip: IpAddr,
    resolver: &crate::dns::DnsResolver,
) -> Result<Option<IpAddr>, String> {
    match cfg.udp.advertise.as_ref() {
        None => Ok(None),
        Some(AdvertiseHost::Ip(ip)) if ip.is_unspecified() => Ok(None),
        Some(AdvertiseHost::Ip(ip)) => Ok(Some(*ip)),
        Some(AdvertiseHost::Domain(host)) => {
            let deadline = Instant::now() + Duration::from_millis(cfg.timeouts.connect_ms);
            let addresses = resolver
                .lookup(host, 0, deadline, bind_ip.is_ipv4())
                .await
                .map_err(|error| error.to_string())?;
            addresses
                .into_iter()
                .map(|address| address.ip())
                .find(|ip| !ip.is_unspecified() && ip.is_ipv4() == bind_ip.is_ipv4())
                .map(Some)
                .ok_or_else(|| format!("{host}: no addresses matching the UDP socket family"))
        }
    }
}

/// Build a SOCKS5 [`Address`] from a [`SocketAddr`].
fn addr_from_socket(sa: SocketAddr) -> Address {
    match sa {
        SocketAddr::V4(v4) => Address::V4(*v4.ip(), v4.port()),
        SocketAddr::V6(v6) => Address::V6(*v6.ip(), v6.port()),
    }
}
