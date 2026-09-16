//! CONNECT command: target resolution, dial with timeout, success reply, and
//! a counted bidirectional relay.

use std::collections::{HashSet, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch};
use tokio::time::Instant;

use crate::config::Config;
use crate::error::Socks5Error;
use crate::metrics::{ConnKind, Event, Metrics};
use crate::protocol::address::Address;
use crate::protocol::reply::{encode_reply, REP_SUCCEEDED};

/// Handle a CONNECT request: dial `target` and relay bytes to/from `client`.
///
/// `initial` holds any bytes the client pipelined after the request; they are
/// written upstream before the relay. `shutdown` cancels an in-flight relay on
/// server shutdown.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut client: TcpStream,
    target: Address,
    initial: Vec<u8>,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    peer: SocketAddr,
    mut shutdown: watch::Receiver<bool>,
    resolver: Arc<crate::dns::DnsResolver>,
) {
    let target_str = address_to_string(&target);
    let started = Instant::now();
    let deadline = started + Duration::from_millis(cfg.timeouts.connect_ms);

    let upstream = match establish(&target, &cfg, &resolver, deadline).await {
        Ok(stream) => stream,
        Err(error) => {
            let (err, msg) = match error {
                EstablishError::Dns(detail) => (
                    Socks5Error::HostUnreachable,
                    format!("could not resolve {target_str}: {detail}"),
                ),
                EstablishError::Blocked => (
                    Socks5Error::NotAllowed,
                    format!("destination not allowed: {target_str}"),
                ),
                EstablishError::Dial(error) => (
                    Socks5Error::from_io(&error),
                    format!(
                        "connect to {target_str} failed after {} ms: {error}",
                        started.elapsed().as_millis()
                    ),
                ),
            };
            reply_failure(&mut client, err.clone()).await;
            metrics.record_error(err.reply_code());
            let _ = events.send(Event::Error {
                code: err.reply_code(),
                msg,
            });
            return;
        }
    };

    let mut upstream = upstream;
    // Nagle adds relay latency for request/response traffic; proxies
    // conventionally disable it on both legs.
    let _ = upstream.set_nodelay(true);

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

    // 5. Relay any pipelined initial payload upstream, then relay both
    //    directions until either side closes, the idle timeout fires, or
    //    shutdown is requested.
    let initial_ok = if initial.is_empty() {
        true
    } else {
        match upstream.write_all(&initial).await {
            Ok(()) => {
                metrics.add_up(id, initial.len() as u64);
                true
            }
            Err(_) => false,
        }
    };
    if initial_ok {
        let idle = Duration::from_millis(cfg.timeouts.tcp_idle_ms);
        let relay = copy_bidirectional_counted(&mut client, &mut upstream, idle, &metrics, id);
        tokio::select! {
            _ = relay => {}
            // Server shutdown: stop relaying; the streams close as run() returns.
            _ = wait_for_shutdown(&mut shutdown) => {}
        }
    }

    metrics.record_success();
    metrics.unregister(id);
    let _ = events.send(Event::Closed { id });
}

/// Resolve once `shutdown` is true (or its sender is dropped).
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
}

enum EstablishError {
    Dns(String),
    Blocked,
    Dial(io::Error),
}

/// Resolve both families while connecting. A failed early candidate must not
/// cancel a slower DNS answer; only an established connection ends the search.
async fn establish(
    target: &Address,
    cfg: &Config,
    resolver: &crate::dns::DnsResolver,
    deadline: Instant,
) -> Result<TcpStream, EstablishError> {
    let (host, port) = match target {
        Address::Domain(host, port) => (host, *port),
        literal => {
            let address = match literal {
                Address::V4(ip, port) => SocketAddr::new((*ip).into(), *port),
                Address::V6(ip, port) => SocketAddr::new((*ip).into(), *port),
                Address::Domain(_, _) => return Err(EstablishError::Dns("invalid target".into())),
            };
            if cfg.egress.is_blocked(address.ip()) {
                return Err(EstablishError::Blocked);
            }
            return dial_one(address, deadline)
                .await
                .map_err(EstablishError::Dial);
        }
    };
    let started = Instant::now();
    let a = resolver.lookup(host, port, deadline, true);
    let aaaa = resolver.lookup(host, port, deadline, false);
    tokio::pin!(a, aaaa);
    let mut done = [false; 2];
    let mut errors = [String::new(), String::new()];
    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();
    let mut blocked = false;
    let mut active = None;
    let mut attempt_deadline = deadline;
    let mut last_error = None;
    loop {
        if active.is_none() {
            if let Some(address) = queue.pop_front() {
                // Reserve time for actual alternatives. A still-pending DNS
                // query is not a candidate and must not shorten a lone dial.
                let divisor = u32::try_from(queue.len() + 1).unwrap_or(u32::MAX);
                let now = Instant::now();
                attempt_deadline = now + deadline.saturating_duration_since(now) / divisor;
                active = Some(Box::pin(TcpStream::connect(address)));
            } else if done.iter().all(|finished| *finished) {
                return Err(match last_error {
                    Some(error) => EstablishError::Dial(error),
                    None if blocked => EstablishError::Blocked,
                    None => EstablishError::Dns(format!(
                        "failed after {} ms: A: {}; AAAA: {}",
                        started.elapsed().as_millis(),
                        errors[0],
                        errors[1]
                    )),
                });
            }
        }
        tokio::select! {
            result = &mut a, if !done[0] => {
                done[0] = true;
                match result {
                    Ok(addresses) => add_candidates(addresses, cfg, &mut queue, &mut seen, &mut blocked),
                    Err(error) => errors[0] = error.to_string(),
                }
                if active.is_some() && !queue.is_empty() {
                    let now = Instant::now();
                    let divisor = u32::try_from(queue.len() + 1).unwrap_or(u32::MAX);
                    attempt_deadline = attempt_deadline.min(now + deadline.saturating_duration_since(now) / divisor);
                }
            }
            result = &mut aaaa, if !done[1] => {
                done[1] = true;
                match result {
                    Ok(addresses) => add_candidates(addresses, cfg, &mut queue, &mut seen, &mut blocked),
                    Err(error) => errors[1] = error.to_string(),
                }
                if active.is_some() && !queue.is_empty() {
                    let now = Instant::now();
                    let divisor = u32::try_from(queue.len() + 1).unwrap_or(u32::MAX);
                    attempt_deadline = attempt_deadline.min(now + deadline.saturating_duration_since(now) / divisor);
                }
            }
            result = async {
                match &mut active {
                    Some(attempt) => attempt.await,
                    None => std::future::pending().await,
                }
            }, if active.is_some() => {
                active = None;
                match result {
                    Ok(stream) => return Ok(stream),
                    Err(error) => last_error = Some(error),
                }
            }
            _ = tokio::time::sleep_until(attempt_deadline), if active.is_some() => {
                active = None;
                last_error = Some(io::Error::new(io::ErrorKind::TimedOut, "connection attempt timed out"));
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Err(if active.is_some() || last_error.is_some() {
                    EstablishError::Dial(io::Error::new(io::ErrorKind::TimedOut, "DNS and connection budget exhausted"))
                } else if blocked {
                    EstablishError::Blocked
                } else {
                    EstablishError::Dns(format!("timed out after {} ms; A: {}; AAAA: {}",
                        started.elapsed().as_millis(), errors[0], errors[1]))
                });
            }
        }
    }
}

fn add_candidates(
    addresses: Vec<SocketAddr>,
    cfg: &Config,
    queue: &mut VecDeque<SocketAddr>,
    seen: &mut HashSet<SocketAddr>,
    blocked: &mut bool,
) {
    for address in addresses {
        if cfg.egress.is_blocked(address.ip()) {
            *blocked = true;
        } else if seen.insert(address) {
            queue.push_back(address);
        }
    }
}

async fn dial_one(address: SocketAddr, deadline: Instant) -> io::Result<TcpStream> {
    tokio::time::timeout_at(deadline, TcpStream::connect(address))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect to {address} timed out"),
            )
        })?
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
    // TcpStream::split is the lock-free borrowed split; the generic
    // tokio::io::split would take a Mutex on every poll.
    let (mut client_rd, mut client_wr) = client.split();
    let (mut upstream_rd, mut upstream_wr) = upstream.split();

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
    // 64 KiB won a loopback sweep of 8/16/64/256 KiB (+15-20% bulk throughput
    // vs 16 KiB; 256 KiB regressed under cache pressure). Idle connections do
    // not keep these pages resident; only actively relayed bytes do. See
    // docs/PERFORMANCE.md "Relay buffer size".
    let mut buf = [0u8; 64 * 1024];
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
