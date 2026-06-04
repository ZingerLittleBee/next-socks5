//! CONNECT command: target resolution, dial with timeout, success reply, and
//! a counted bidirectional relay.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::time::Instant;

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
    let connect_timeout = Duration::from_millis(cfg.timeouts.connect_ms);

    // 1. Resolve the target to a concrete SocketAddr, bounding DNS by the
    //    connect timeout so a slow/blackholed resolver cannot stall the task.
    let addr = match tokio::time::timeout(connect_timeout, resolve(&target)).await {
        Ok(Some(addr)) => addr,
        Ok(None) | Err(_) => {
            reply_failure(&mut client, Socks5Error::HostUnreachable).await;
            metrics.record_error(Socks5Error::HostUnreachable.reply_code());
            let _ = events.send(Event::Error {
                code: Socks5Error::HostUnreachable.reply_code(),
                msg: format!("could not resolve {target_str}"),
            });
            return;
        }
    };

    // 2. Egress policy: refuse internal/metadata destinations (SSRF guard). The
    //    check runs after resolution so domains pointing at internal IPs are
    //    blocked too.
    if cfg.egress.is_blocked(addr.ip()) {
        reply_failure(&mut client, Socks5Error::NotAllowed).await;
        metrics.record_error(Socks5Error::NotAllowed.reply_code());
        let _ = events.send(Event::Error {
            code: Socks5Error::NotAllowed.reply_code(),
            msg: format!("destination not allowed: {target_str}"),
        });
        return;
    }

    // 3. Dial the upstream with a connect timeout.
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

    // 4. Reply success with the upstream's local address as BND.
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

    // 5. Relay until either side closes or the idle timeout fires.
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

/// Relay bytes in both directions until BOTH sides close (or the relay winds
/// down on a coupled idle timeout / write timeout / error).
///
/// Each `TcpStream` is split into independent read/write halves so the two
/// directions run concurrently via [`tokio::join!`]. This supports half-open
/// connections: when one direction reaches EOF, only that direction stops and
/// the writer it feeds is shut down (propagating the FIN) — the other direction
/// keeps relaying until it too reaches EOF. `client -> upstream` bytes count as
/// upload, the reverse as download.
///
/// The two directions share a single "last activity" instant so an idle
/// direction does not half-close while the other is actively transferring;
/// teardown on idleness only happens when NEITHER direction has moved bytes for
/// the idle window. Writes are bounded by the same window so a stuck reader
/// (full receive window) cannot pin a direction forever.
async fn copy_bidirectional_counted(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    idle: Duration,
    metrics: &Metrics,
    id: u64,
) -> std::io::Result<()> {
    let (mut client_rd, mut client_wr) = tokio::io::split(client);
    let (mut upstream_rd, mut upstream_wr) = tokio::io::split(upstream);

    // Shared across both directions; updated on every successful relayed write.
    let last_activity = Mutex::new(Instant::now());

    // client -> upstream (upload)
    let up = copy_half(
        &mut client_rd,
        &mut upstream_wr,
        idle,
        Direction::Up,
        metrics,
        id,
        &last_activity,
    );
    // upstream -> client (download)
    let down = copy_half(
        &mut upstream_rd,
        &mut client_wr,
        idle,
        Direction::Down,
        metrics,
        id,
        &last_activity,
    );

    // Drive both directions to completion. Either may end first (on its source
    // EOF, coupled idle timeout, write timeout, or error); the relay returns
    // only once both are done.
    let (up_res, down_res) = tokio::join!(up, down);
    up_res.and(down_res)
}

/// Which direction a [`copy_half`] relays, used to pick the byte counter.
#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

/// Copy one direction: read from `src` with an idle timeout and write to `dst`
/// with a write timeout, counting bytes per `dir`. On source EOF, `dst` is shut
/// down (half-close) so the peer observes the FIN. On idle timeout the direction
/// only winds down when the OTHER direction (via `last_activity`) has also been
/// idle for the window, so an active transfer is never truncated.
#[allow(clippy::too_many_arguments)]
async fn copy_half<R, W>(
    src: &mut R,
    dst: &mut W,
    idle: Duration,
    dir: Direction,
    metrics: &Metrics,
    id: u64,
    last_activity: &Mutex<Instant>,
) -> std::io::Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = [0u8; 16 * 1024];
    loop {
        match read_with_idle(src, &mut buf, idle).await? {
            // Genuine EOF: half-close the writer so the destination sees the
            // close, then finish this direction only.
            Some(0) => {
                let _ = dst.shutdown().await;
                return Ok(());
            }
            // Idle window elapsed for this direction's read. Only tear down if
            // the other direction has ALSO been idle that long; otherwise it is
            // actively transferring and half-closing here would truncate it.
            None => {
                let stale = last_activity.lock().unwrap().elapsed() >= idle;
                if stale {
                    let _ = dst.shutdown().await;
                    return Ok(());
                }
                continue;
            }
            Some(n) => {
                write_all_with_timeout(dst, &buf[..n], idle).await?;
                *last_activity.lock().unwrap() = Instant::now();
                match dir {
                    Direction::Up => metrics.add_up(id, n as u64),
                    Direction::Down => metrics.add_down(id, n as u64),
                }
            }
        }
    }
}

/// Read into `buf` with an idle timeout. Returns `Ok(Some(n))` on a read of
/// `n` bytes (0 means EOF), or `Ok(None)` when the idle timeout elapsed.
async fn read_with_idle<R>(
    stream: &mut R,
    buf: &mut [u8],
    idle: Duration,
) -> std::io::Result<Option<usize>>
where
    R: AsyncReadExt + Unpin,
{
    match tokio::time::timeout(idle, stream.read(buf)).await {
        Ok(Ok(n)) => Ok(Some(n)),
        Ok(Err(e)) => Err(e),
        Err(_) => Ok(None),
    }
}

/// Write all of `data` to `dst`, bounded by `timeout`. A peer that stops
/// draining its socket (full receive window) would otherwise block this write —
/// and thus the whole relay — forever; the timeout turns that into an error so
/// the relay tears down.
async fn write_all_with_timeout<W>(dst: &mut W, data: &[u8], timeout: Duration) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    match tokio::time::timeout(timeout, dst.write_all(data)).await {
        Ok(res) => res,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "relay write timed out (peer not draining)",
        )),
    }
}
