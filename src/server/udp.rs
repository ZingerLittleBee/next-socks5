//! UDP ASSOCIATE relay (RFC 1928 section 7).
//!
//! The association rides on a TCP control connection: the server binds an
//! ephemeral UDP socket reachable by the client, advertises it in the SOCKS
//! reply, then relays datagrams between the client and arbitrary targets until
//! the control connection closes or the association goes idle.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::broadcast;

use crate::config::Config;
use crate::metrics::{ConnKind, Event, Metrics};
use crate::protocol::address::Address;
use crate::protocol::reply::{encode_reply, REP_SUCCEEDED};
use crate::protocol::udp::decap;

/// Maximum size of a single UDP datagram we are willing to buffer (64 KiB).
const UDP_BUF: usize = 65536;

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
) {
    // 1. Determine the IP the client can reach us on, and bind a UDP socket on
    //    an ephemeral port of that IP. Never advertise an unspecified address.
    let bind_ip = match resolve_bind_ip(&cfg, &control) {
        Some(ip) => ip,
        None => return,
    };

    let udp_sock = match UdpSocket::bind((bind_ip, 0)).await {
        Ok(sock) => sock,
        Err(_) => return,
    };
    let bnd_local = match udp_sock.local_addr() {
        Ok(addr) => addr,
        Err(_) => return,
    };

    // 2. Reply success with the bound UDP address as BND.ADDR/PORT.
    let bnd_address = addr_from_socket(bnd_local);
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
    let mut known_targets: HashSet<SocketAddr> = HashSet::new();

    let idle = Duration::from_millis(cfg.timeouts.udp_idle_ms);
    let mut buf = vec![0u8; UDP_BUF];
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

                if known_targets.contains(&src) {
                    // Target -> client reply. Re-encapsulate and forward to the
                    // client's learned UDP source (if any).
                    if let Some(dst) = client_udp_addr {
                        let mut framed = Vec::with_capacity(n + 22);
                        crate::protocol::udp::encap(&addr_from_socket(src), &buf[..n], &mut framed);
                        let _ = udp_sock.send_to(&framed, dst).await;
                        metrics.add_down(id, n as u64);
                    }
                } else if src.ip() == client_ip {
                    // Client -> target datagram. Learn the client's UDP source.
                    client_udp_addr = Some(src);
                    let datagram = match decap(&buf[..n]) {
                        Ok(dg) => dg,
                        // Malformed datagram: drop it.
                        Err(_) => continue,
                    };
                    // Fragmentation is not supported: drop FRAG != 0.
                    if datagram.frag != 0 {
                        continue;
                    }
                    let target = match resolve(&datagram.address).await {
                        Some(t) => t,
                        // Unresolvable target: drop the datagram.
                        None => continue,
                    };
                    if udp_sock.send_to(&datagram.data, target).await.is_ok() {
                        known_targets.insert(target);
                        metrics.add_up(id, datagram.data.len() as u64);
                    }
                }
                // Otherwise: source IP is neither the client nor a known target
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
        }
    }

    // 4. Tear down: drop the UDP socket (auto-reclaims the port) and record the
    //    association as finished.
    metrics.record_success();
    metrics.unregister(id);
    let _ = events.send(Event::Closed { id });
}

/// Determine the client-reachable IP to bind the relay UDP socket on.
///
/// Prefers `cfg.public_addr` (parsed as an IP) when set, otherwise the control
/// connection's local IP. An unspecified address (`0.0.0.0` / `::`) is never
/// advertised, so it falls back to loopback.
fn resolve_bind_ip(cfg: &Config, control: &TcpStream) -> Option<IpAddr> {
    let ip = match &cfg.public_addr {
        Some(s) => parse_ip(s)?,
        None => control.local_addr().ok()?.ip(),
    };
    if ip.is_unspecified() {
        Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
    } else {
        Some(ip)
    }
}

/// Parse an IP from a `public_addr` value that may be a bare IP or `ip:port`.
fn parse_ip(s: &str) -> Option<IpAddr> {
    if let Ok(ip) = s.parse::<IpAddr>() {
        return Some(ip);
    }
    s.parse::<SocketAddr>().ok().map(|sa| sa.ip())
}

/// Build a SOCKS5 [`Address`] from a [`SocketAddr`].
fn addr_from_socket(sa: SocketAddr) -> Address {
    match sa {
        SocketAddr::V4(v4) => Address::V4(*v4.ip(), v4.port()),
        SocketAddr::V6(v6) => Address::V6(*v6.ip(), v6.port()),
    }
}

/// Resolve a SOCKS5 [`Address`] to a single [`SocketAddr`] (server-side DNS for
/// domain targets, mirroring the CONNECT path).
async fn resolve(target: &Address) -> Option<SocketAddr> {
    match target {
        Address::V4(ip, port) => Some(SocketAddr::new(IpAddr::V4(*ip), *port)),
        Address::V6(ip, port) => Some(SocketAddr::new(IpAddr::V6(*ip), *port)),
        Address::Domain(host, port) => tokio::net::lookup_host((host.as_str(), *port))
            .await
            .ok()
            .and_then(|mut it| it.next()),
    }
}
