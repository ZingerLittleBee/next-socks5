//! CONNECT command: target resolution, dial with timeout, success reply, and
//! a counted bidirectional relay.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::error::Socks5Error;
use crate::metrics::{ConnKind, Event, Metrics};
use crate::protocol::address::Address;
use crate::protocol::reply::{encode_reply, REP_SUCCEEDED};

/// Handle a CONNECT request: dial `target` and relay bytes to/from `client`.
pub async fn run(
    mut client: TcpStream,
    target: Address,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    peer: SocketAddr,
) {
    let target_str = address_to_string(&target);

    // 1. Resolve the target to a concrete SocketAddr.
    let addr = match resolve(&target).await {
        Some(addr) => addr,
        None => {
            reply_failure(&mut client, Socks5Error::HostUnreachable).await;
            metrics.record_error(Socks5Error::HostUnreachable.reply_code());
            let _ = events.send(Event::Error {
                code: Socks5Error::HostUnreachable.reply_code(),
                msg: format!("could not resolve {target_str}"),
            });
            return;
        }
    };

    // 2. Dial the upstream with a connect timeout.
    let connect_timeout = Duration::from_millis(cfg.timeouts.connect_ms);
    let upstream = match tokio::time::timeout(connect_timeout, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            let err = Socks5Error::from_io(&e);
            reply_failure(&mut client, err.clone()).await;
            metrics.record_error(err.reply_code());
            let _ = events.send(Event::Error {
                code: err.reply_code(),
                msg: format!("connect to {target_str} failed: {e}"),
            });
            return;
        }
        Err(_) => {
            // Timeout elapsed before the connection was established.
            reply_failure(&mut client, Socks5Error::TtlExpired).await;
            metrics.record_error(Socks5Error::TtlExpired.reply_code());
            let _ = events.send(Event::Error {
                code: Socks5Error::TtlExpired.reply_code(),
                msg: format!("connect to {target_str} timed out"),
            });
            return;
        }
    };

    let mut upstream = upstream;

    // 3. Reply success with the upstream's local address as BND.
    let bind = upstream
        .local_addr()
        .map(socket_addr_to_address)
        .unwrap_or(Address::V4(Ipv4Addr::UNSPECIFIED, 0));
    let mut out = Vec::with_capacity(22);
    encode_reply(REP_SUCCEEDED, &bind, &mut out);
    if client.write_all(&out).await.is_err() {
        return;
    }

    // Register the connection only after a successful reply.
    let id = metrics.register(peer, target_str.clone(), ConnKind::Connect);
    let _ = events.send(Event::Connect {
        id,
        src: peer,
        target: target_str,
        kind: ConnKind::Connect,
    });

    // 4. Relay until either side closes or the idle timeout fires.
    let idle = Duration::from_millis(cfg.timeouts.tcp_idle_ms);
    let _ = copy_bidirectional_counted(&mut client, &mut upstream, idle, &metrics, id).await;

    metrics.record_success();
    metrics.unregister(id);
    let _ = events.send(Event::Closed { id });
}

/// Resolve a SOCKS5 [`Address`] to a single [`SocketAddr`].
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

/// Render a target address as a `host:port` string for logging/metrics.
fn address_to_string(target: &Address) -> String {
    match target {
        Address::V4(ip, port) => format!("{ip}:{port}"),
        Address::V6(ip, port) => format!("[{ip}]:{port}"),
        Address::Domain(host, port) => format!("{host}:{port}"),
    }
}

/// Convert a [`SocketAddr`] into a SOCKS5 [`Address`] for the BND reply.
fn socket_addr_to_address(addr: SocketAddr) -> Address {
    match addr {
        SocketAddr::V4(v4) => Address::V4(*v4.ip(), v4.port()),
        SocketAddr::V6(v6) => Address::V6(*v6.ip(), v6.port()),
    }
}

/// Send a best-effort failure reply with a zeroed IPv4 BND address.
async fn reply_failure(client: &mut TcpStream, err: Socks5Error) {
    let mut out = Vec::with_capacity(10);
    let bind = Address::V4(Ipv4Addr::UNSPECIFIED, 0);
    encode_reply(err.reply_code(), &bind, &mut out);
    let _ = client.write_all(&out).await;
}

/// Relay bytes in both directions until one side closes or goes idle.
///
/// Each direction reads with an idle timeout: if no bytes arrive within `idle`,
/// the relay stops. `client -> upstream` bytes count as upload, the reverse as
/// download. When either side reaches EOF, the peer is shut down so it sees the
/// close too.
async fn copy_bidirectional_counted(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    idle: Duration,
    metrics: &Metrics,
    id: u64,
) -> std::io::Result<()> {
    let mut buf_up = [0u8; 16 * 1024];
    let mut buf_down = [0u8; 16 * 1024];
    let mut client_open = true;
    let mut upstream_open = true;

    while client_open && upstream_open {
        tokio::select! {
            // client -> upstream (upload)
            res = read_with_idle(client, &mut buf_up, idle) => {
                match res? {
                    // EOF or idle timeout on the client side: half-close upstream.
                    Some(0) | None => {
                        client_open = false;
                        let _ = upstream.shutdown().await;
                    }
                    Some(n) => {
                        upstream.write_all(&buf_up[..n]).await?;
                        metrics.add_up(id, n as u64);
                    }
                }
            }
            // upstream -> client (download)
            res = read_with_idle(upstream, &mut buf_down, idle) => {
                match res? {
                    // EOF or idle timeout on the upstream side: half-close client.
                    Some(0) | None => {
                        upstream_open = false;
                        let _ = client.shutdown().await;
                    }
                    Some(n) => {
                        client.write_all(&buf_down[..n]).await?;
                        metrics.add_down(id, n as u64);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Read into `buf` with an idle timeout. Returns `Ok(Some(n))` on a read of
/// `n` bytes (0 means EOF), or `Ok(None)` when the idle timeout elapsed.
async fn read_with_idle(
    stream: &mut TcpStream,
    buf: &mut [u8],
    idle: Duration,
) -> std::io::Result<Option<usize>> {
    match tokio::time::timeout(idle, stream.read(buf)).await {
        Ok(Ok(n)) => Ok(Some(n)),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(None),
    }
}
