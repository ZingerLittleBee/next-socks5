//! Reproduction tests for the audit findings (P0/P1/P2).
//!
//! Each test asserts the DESIRED (post-fix) behavior. Run against the current
//! code they FAIL — that failure is the proof the vulnerability/bug is real.
//! After the fix they turn green and serve as regression guards.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use next_socks5::admin::{claim_socket, serve, EventRing};
use next_socks5::config::{AuthConfig, AuthMethod, Config, Egress, Limits, Timeouts, User};
use next_socks5::metrics::{Event, Metrics, MetricsSource};
use next_socks5::server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};

// ---------------------------------------------------------------------------
// Shared harness
// ---------------------------------------------------------------------------

fn no_auth_config() -> Config {
    Config {
        listen: "127.0.0.1:0".to_string(),
        auth: AuthConfig {
            method: AuthMethod::None,
            users: Vec::new(),
        },
        timeouts: Timeouts::default(),
        limits: Limits::default(),
        udp: Default::default(),
        admin: Default::default(),
        // Relay reproduction tests dial loopback helpers; allow it. The SSRF
        // test builds its own secure-egress config.
        egress: Egress::permissive(),
    }
}

fn password_config() -> Config {
    Config {
        listen: "127.0.0.1:0".to_string(),
        auth: AuthConfig {
            method: AuthMethod::Password,
            users: vec![User {
                username: "alice".to_string(),
                password: "secret".to_string(),
            }],
        },
        timeouts: Timeouts::default(),
        limits: Limits::default(),
        udp: Default::default(),
        admin: Default::default(),
        // Relay reproduction tests dial loopback helpers; allow it. The SSRF
        // test builds its own secure-egress config.
        egress: Egress::permissive(),
    }
}

/// Start the proxy and also hand back the shared `Metrics` so tests can observe
/// active-connection accounting.
async fn start_server_returning_metrics(cfg: Config) -> (std::net::SocketAddr, Arc<Metrics>) {
    let cfg = Arc::new(cfg);
    let listener = TcpListener::bind(&cfg.listen).await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let metrics = Metrics::new();
    let (events, _events_rx) = broadcast::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    std::mem::forget(shutdown_tx);
    std::mem::forget(_events_rx);

    tokio::spawn(server::run(
        listener,
        cfg.clone(),
        metrics.clone(),
        events.clone(),
        shutdown_rx,
    ));
    (proxy_addr, metrics)
}

async fn start_server(cfg: Config) -> std::net::SocketAddr {
    start_server_returning_metrics(cfg).await.0
}

/// Start the proxy and return the shutdown sender so a test can request a
/// graceful shutdown and observe in-flight relays winding down.
async fn start_server_with_shutdown(
    cfg: Config,
) -> (std::net::SocketAddr, Arc<Metrics>, watch::Sender<bool>) {
    let cfg = Arc::new(cfg);
    let listener = TcpListener::bind(&cfg.listen).await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let metrics = Metrics::new();
    let (events, _events_rx) = broadcast::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    std::mem::forget(_events_rx);
    tokio::spawn(server::run(
        listener,
        cfg.clone(),
        metrics.clone(),
        events.clone(),
        shutdown_rx,
    ));
    (proxy_addr, metrics, shutdown_tx)
}

/// Spawn a one-shot TCP echo server. Returns its bound address.
async fn spawn_echo(addr_out: &str) -> std::net::SocketAddr {
    let listener = TcpListener::bind(addr_out).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut b = [0u8; 1024];
            loop {
                match sock.read(&mut b).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock.write_all(&b[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    addr
}

/// Drive a no-auth greeting and assert the server selects NO-AUTH.
async fn no_auth_handshake(client: &mut TcpStream) {
    client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut method_reply = [0u8; 2];
    client.read_exact(&mut method_reply).await.unwrap();
    assert_eq!(method_reply, [0x05, 0x00]);
}

fn connect_v4_request(addr: std::net::SocketAddr) -> Vec<u8> {
    let v4 = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(_) => panic!("expected v4 addr"),
    };
    let mut req = vec![0x05, 0x01, 0x00, 0x01];
    req.extend_from_slice(&v4.octets());
    req.extend_from_slice(&addr.port().to_be_bytes());
    req
}

/// A unique temp path under the OS temp dir (no external deps).
fn unique_temp_path(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("next-socks5-test-{tag}-{pid}-{n}"))
}

// ---------------------------------------------------------------------------
// P0 — hang / resource-pinning
// ---------------------------------------------------------------------------

/// H1 (slowloris): a client that completes the TCP handshake but sends only a
/// partial greeting must be dropped within the handshake deadline. Today there
/// is no handshake timeout, so the connection is held open forever.
#[tokio::test]
async fn stalled_handshake_is_closed_within_deadline() {
    let mut cfg = no_auth_config();
    cfg.timeouts.handshake_ms = 300; // small deadline for a fast test
    let proxy_addr = start_server(cfg).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    // Partial greeting: VER + NMETHODS=1 but the method byte never arrives.
    client.write_all(&[0x05, 0x01]).await.unwrap();

    // The server must close the stalled handshake well within ~2s.
    let mut buf = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    match closed {
        Ok(Ok(0)) => {}        // EOF: server dropped us — correct
        Ok(Err(_)) => {}       // reset: also closed — correct
        Ok(Ok(n)) => panic!("expected close, server sent {n} bytes"),
        Err(_) => panic!("BUG: stalled handshake was NOT closed within 2s (no handshake timeout)"),
    }
}

/// H2/H3: a stuck/slow reader must not pin a relay forever. With a small TCP
/// idle timeout, a client that triggers a large download and then stops reading
/// (and stops sending) must see the relay reclaimed. Today `write_all` has no
/// timeout and `join!` waits for both halves, so `active` stays 1 forever.
#[tokio::test]
async fn stuck_reader_does_not_pin_relay_forever() {
    // Upstream that floods data and keeps its socket open (ignores its read end).
    let flood = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let flood_addr = flood.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = flood.accept().await {
            let chunk = vec![0u8; 64 * 1024];
            // ~8 MiB: far more than client recv + kernel send buffers, so the
            // proxy's write_all to the non-reading client blocks.
            for _ in 0..128 {
                if sock.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            // Keep the connection open so the download half cannot reach EOF.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    let mut cfg = no_auth_config();
    cfg.timeouts.tcp_idle_ms = 300;
    let (proxy_addr, metrics) = start_server_returning_metrics(cfg).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    no_auth_handshake(&mut client).await;
    client
        .write_all(&connect_v4_request(flood_addr))
        .await
        .unwrap();
    let mut reply = [0u8; 10];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00, "CONNECT should succeed");

    // The client now neither reads the flood nor sends anything upstream.
    // Hold the client open but idle.
    let _client = client;

    // After the upload half idles out, the download half is stuck in write_all.
    // A correct relay reclaims the connection; poll active() for up to ~4s.
    let mut active_final = u64::MAX;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        active_final = metrics.active();
        if active_final == 0 {
            break;
        }
    }
    assert_eq!(
        active_final, 0,
        "BUG: relay pinned forever by a stuck writer (write_all/join! have no deadline)"
    );
}

// ---------------------------------------------------------------------------
// P1 — stream interruption + exposure
// ---------------------------------------------------------------------------

/// S2: an active download with an idle upload half must NOT be truncated. With
/// a small idle timeout, the upload half (client->upstream, idle) currently
/// half-closes the upstream mid-download; an upstream that stops on that EOF
/// truncates the transfer.
#[tokio::test]
async fn idle_upload_does_not_truncate_active_download() {
    const CHUNKS: usize = 8;
    // Upstream that streams CHUNKS over ~1.2s but stops early if it observes its
    // read side close (i.e. the proxy half-closed it due to upload idleness).
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((sock, _)) = target.accept().await {
            let (mut rd, mut wr) = sock.into_split();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_r = stop.clone();
            tokio::spawn(async move {
                let mut b = [0u8; 64];
                loop {
                    match rd.read(&mut b).await {
                        Ok(0) | Err(_) => {
                            stop_r.store(true, Ordering::SeqCst);
                            break;
                        }
                        Ok(_) => {}
                    }
                }
            });
            for _ in 0..CHUNKS {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if wr.write_all(b"DATA").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let _ = wr.shutdown().await;
        }
    });

    let mut cfg = no_auth_config();
    cfg.timeouts.tcp_idle_ms = 400; // shorter than the ~1.2s stream
    let proxy_addr = start_server(cfg).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    no_auth_handshake(&mut client).await;
    client
        .write_all(&connect_v4_request(target_addr))
        .await
        .unwrap();
    let mut reply = [0u8; 10];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00, "CONNECT should succeed");

    // Client never sends upstream (idle upload). It must still receive the full
    // download despite the upload half going idle.
    let mut received = Vec::new();
    let read_all = tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut received))
        .await
        .expect("read_to_end timed out");
    read_all.unwrap();
    assert_eq!(
        received.len(),
        CHUNKS * 4,
        "BUG: active download truncated by idle-upload half-close (got {} of {} bytes)",
        received.len(),
        CHUNKS * 4
    );
}

/// SSRF: with the default egress policy a CONNECT to a loopback address must be
/// refused (reply 0x02), not relayed. Today there is no egress filtering, so the
/// proxy happily dials 127.0.0.1.
#[tokio::test]
async fn connect_to_loopback_is_blocked_by_default() {
    // A local listener the proxy must NOT be allowed to reach.
    let internal = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let internal_addr = internal.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = internal.accept().await; // would accept if the proxy dialed it
    });

    // Use the secure-by-default egress policy (block internal destinations).
    let mut cfg = no_auth_config();
    cfg.egress = Egress::default();
    let proxy_addr = start_server(cfg).await;
    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    no_auth_handshake(&mut client).await;
    client
        .write_all(&connect_v4_request(internal_addr))
        .await
        .unwrap();

    let mut reply = [0u8; 10];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[0], 0x05);
    assert_eq!(
        reply[1], 0x02,
        "BUG: loopback (SSRF) target was not blocked; got reply {:#04x}",
        reply[1]
    );
}

/// Admin Unix socket must not be group/other-accessible (it streams client IPs,
/// targets, byte counts, usernames). Today bind relies on umask only.
#[tokio::test]
async fn admin_socket_is_not_group_or_world_accessible() {
    use std::os::unix::fs::PermissionsExt;

    let dir = unique_temp_path("adminperm");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("admin.sock");

    let source: Arc<dyn MetricsSource> = Metrics::new();
    let (events, _rx) = broadcast::channel::<Event>(16);
    let (_sd_tx, sd_rx) = watch::channel(false);
    let sock_for_task = sock.clone();
    tokio::spawn(async move {
        let _ = serve(&sock_for_task, source, events, EventRing::new(), sd_rx, None).await;
    });

    // Give serve() time to bind.
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(sock.exists(), "admin socket was never created");

    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        mode & 0o077,
        0,
        "BUG: admin socket is group/other-accessible (mode {:#o})",
        mode
    );
}

/// The admin endpoint must create its parent directory; today it only binds, so
/// the default /run/next-socks5/admin.sock path fails on a fresh system.
#[tokio::test]
async fn admin_serve_creates_missing_parent_dir() {
    let base = unique_temp_path("admindir");
    let sock = base.join("nested").join("admin.sock"); // parent does not exist

    let source: Arc<dyn MetricsSource> = Metrics::new();
    let (events, _rx) = broadcast::channel::<Event>(16);
    let (_sd_tx, sd_rx) = watch::channel(false);
    let sock_for_task = sock.clone();
    tokio::spawn(async move {
        let _ = serve(&sock_for_task, source, events, EventRing::new(), sd_rx, None).await;
    });

    let mut bound = false;
    for _ in 0..50 {
        if sock.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        bound,
        "BUG: admin serve did not create the missing parent dir, bind failed"
    );
}

/// A second instance must NOT hijack an admin socket a live peer is serving.
/// The original incident: a bare `next-socks5` started a second server whose
/// admin endpoint unlinked + rebound the running service's `/run/next-socks5/
/// admin.sock`, then deleted it on exit — leaving the live service with no
/// reachable socket. The fix probes liveness with `connect()` and refuses.
#[tokio::test]
async fn live_admin_socket_is_not_hijacked_by_second_instance() {
    let dir = unique_temp_path("hijack");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("admin.sock");

    // First instance claims and holds the socket + its advisory lock.
    let (listener1, _lock1) = claim_socket(&sock).await.expect("first claim binds");
    assert!(sock.exists(), "first claim should create the socket");

    // Second instance must refuse rather than unlink/rebind the live socket.
    let err = claim_socket(&sock)
        .await
        .expect_err("BUG: second claim hijacked a live admin socket");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AddrInUse,
        "expected AddrInUse refusal, got: {err}"
    );

    // The original endpoint is untouched: a fresh connect still succeeds.
    tokio::net::UnixStream::connect(&sock)
        .await
        .expect("BUG: original admin socket no longer reachable after second claim");

    drop(listener1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stale socket left by a crashed instance (file on disk, nobody listening)
/// must be reclaimed by the next start — `connect()` returns ECONNREFUSED, so it
/// is safe to unlink and rebind.
#[tokio::test]
async fn stale_admin_socket_is_reclaimed() {
    let dir = unique_temp_path("stale");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("admin.sock");

    // Simulate a crash: bind then drop, leaving the socket file but no listener.
    {
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(l);
    }
    assert!(sock.exists(), "stale socket file should remain after drop");
    assert!(
        std::os::unix::net::UnixStream::connect(&sock).is_err(),
        "stale socket must refuse connections (proves it is not live)"
    );

    // A fresh claim reclaims the stale path and binds a working socket.
    let (_listener, _lock) = claim_socket(&sock)
        .await
        .expect("BUG: stale admin socket was not reclaimed");
    tokio::net::UnixStream::connect(&sock)
        .await
        .expect("reclaimed socket should be live");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The sidecar `<socket>.lock` advisory lock serializes racing starters, closing
/// the TOCTOU window where two processes both decide the socket is stale. While
/// one holder keeps the lock, a second claim must be refused even if the socket
/// file itself is gone.
#[tokio::test]
async fn concurrent_claim_is_serialized_by_lock() {
    let dir = unique_temp_path("lockrace");
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("admin.sock");

    // First claim binds the socket AND holds the advisory lock for its lifetime.
    let (_l1, _lock1) = claim_socket(&sock).await.expect("first claim");

    // Remove the socket file out from under the holder (it keeps the lock fd):
    // a racing starter now sees no live socket (connect ENOENT) yet must still be
    // blocked by the held lock instead of rebinding.
    std::fs::remove_file(&sock).unwrap();
    let err = claim_socket(&sock)
        .await
        .expect_err("BUG: lock did not serialize a second binder");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::WouldBlock,
        "expected WouldBlock from the held lock, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// P2 — RFC 1929 best-effort failure reply
// ---------------------------------------------------------------------------

/// A malformed/bad-version auth sub-negotiation must get a best-effort failure
/// reply [0x01,0x01] before close, not a silent TCP close.
#[tokio::test]
async fn malformed_auth_gets_failure_reply_before_close() {
    let proxy_addr = start_server(password_config()).await;
    let mut client = TcpStream::connect(proxy_addr).await.unwrap();

    // Greeting offering USERPASS; server selects it.
    client.write_all(&[0x05, 0x02, 0x00, 0x02]).await.unwrap();
    let mut method_reply = [0u8; 2];
    client.read_exact(&mut method_reply).await.unwrap();
    assert_eq!(method_reply, [0x05, 0x02]);

    // Malformed auth: bad sub-negotiation version (0x02 instead of 0x01).
    client
        .write_all(&[0x02, 0x01, b'a', 0x01, b'b'])
        .await
        .unwrap();

    let mut auth_reply = [0u8; 2];
    let got = tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut auth_reply)).await;
    match got {
        Ok(Ok(_)) => assert_eq!(
            auth_reply,
            [0x01, 0x01],
            "expected RFC 1929 failure reply, got {auth_reply:?}"
        ),
        Ok(Err(_)) => panic!("BUG: malformed auth closed with no failure reply"),
        Err(_) => panic!("BUG: malformed auth produced no reply within 2s"),
    }
}

// ---------------------------------------------------------------------------
// #22 / #16 — accept-time admission control
// ---------------------------------------------------------------------------

/// A connection still in the handshake (half-open) must count toward
/// max_connections, so a half-open flood cannot bypass the cap. With a cap of 1
/// and one half-open connection holding the slot, a second connection is dropped
/// at accept (no greeting reply).
#[tokio::test]
async fn half_open_handshake_counts_toward_connection_limit() {
    let mut cfg = no_auth_config();
    cfg.limits.max_connections = Some(1);
    cfg.timeouts.handshake_ms = 5_000; // keep the half-open alive during the test
    let proxy_addr = start_server(cfg).await;

    // conn#1: send only a partial greeting and stall, holding the single slot.
    let mut c1 = TcpStream::connect(proxy_addr).await.unwrap();
    c1.write_all(&[0x05, 0x01]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // conn#2: over the cap, dropped at accept; its greeting gets no reply.
    let mut c2 = TcpStream::connect(proxy_addr).await.unwrap();
    let _ = c2.write_all(&[0x05, 0x01, 0x00]).await;
    let mut reply = [0u8; 2];
    match tokio::time::timeout(Duration::from_secs(2), c2.read_exact(&mut reply)).await {
        Ok(Err(_)) => {} // EOF/reset — half-open was counted, c2 dropped
        Ok(Ok(_)) => panic!("BUG: half-open connection not counted; over-limit conn got a reply"),
        Err(_) => panic!("BUG: half-open connection not counted; over-limit conn left hanging"),
    }
    drop(c1);
}

/// A per-IP cap limits concurrent connections from one source independently of
/// the global cap.
#[tokio::test]
async fn per_ip_limit_caps_connections_from_one_source() {
    let mut cfg = no_auth_config();
    cfg.limits.max_per_ip = Some(1);
    cfg.timeouts.handshake_ms = 5_000;
    let proxy_addr = start_server(cfg).await;

    let mut c1 = TcpStream::connect(proxy_addr).await.unwrap();
    c1.write_all(&[0x05, 0x01]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Same source IP (127.0.0.1); the second connection is over the per-IP cap.
    let mut c2 = TcpStream::connect(proxy_addr).await.unwrap();
    let _ = c2.write_all(&[0x05, 0x01, 0x00]).await;
    let mut reply = [0u8; 2];
    match tokio::time::timeout(Duration::from_secs(2), c2.read_exact(&mut reply)).await {
        Ok(Err(_)) => {}
        _ => panic!("BUG: per-IP limit not enforced"),
    }
    drop(c1);
}

// ---------------------------------------------------------------------------
// #7 — graceful shutdown aborts in-flight relays
// ---------------------------------------------------------------------------

/// On shutdown, an established but idle relay must wind down promptly rather than
/// surviving until process teardown.
#[tokio::test]
async fn shutdown_aborts_in_flight_relay() {
    // A target that accepts and holds the connection open, idle.
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((sock, _)) = target.accept().await {
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(sock);
        }
    });

    let (proxy_addr, metrics, shutdown) = start_server_with_shutdown(no_auth_config()).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    no_auth_handshake(&mut client).await;
    client
        .write_all(&connect_v4_request(target_addr))
        .await
        .unwrap();
    let mut reply = [0u8; 10];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00, "CONNECT should succeed");

    // Wait until the relay is registered active (default idle is 5min, so it
    // will not self-close).
    let mut active = 0;
    for _ in 0..40 {
        active = metrics.active();
        if active == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(active, 1, "relay should be active before shutdown");

    // Request shutdown; the in-flight relay must wind down.
    shutdown.send(true).unwrap();
    let mut ended = u64::MAX;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        ended = metrics.active();
        if ended == 0 {
            break;
        }
    }
    assert_eq!(
        ended, 0,
        "BUG: shutdown did not abort the in-flight relay (active stayed {ended})"
    );
}

// ---------------------------------------------------------------------------
// #0 — pipelined bytes are preserved
// ---------------------------------------------------------------------------

/// A client that pipelines greeting + CONNECT request + first payload in a
/// single TCP segment must have the payload relayed, not discarded.
#[tokio::test]
async fn pipelined_greeting_request_and_payload_are_relayed() {
    let echo_addr = spawn_echo("127.0.0.1:0").await;
    let proxy_addr = start_server(no_auth_config()).await;

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    // One write: greeting, then CONNECT request, then the first payload.
    let mut pipelined = vec![0x05, 0x01, 0x00];
    pipelined.extend_from_slice(&connect_v4_request(echo_addr));
    pipelined.extend_from_slice(b"ping");
    client.write_all(&pipelined).await.unwrap();

    let mut method_reply = [0u8; 2];
    client.read_exact(&mut method_reply).await.unwrap();
    assert_eq!(method_reply, [0x05, 0x00]);
    let mut connect_reply = [0u8; 10];
    tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut connect_reply))
        .await
        .expect("no connect reply — pipelined request was dropped")
        .unwrap();
    assert_eq!(connect_reply[1], 0x00, "CONNECT should succeed");

    // The pipelined "ping" must have reached the echo target and come back.
    let mut echoed = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut echoed))
        .await
        .expect("BUG: pipelined payload after the request was dropped")
        .unwrap();
    assert_eq!(&echoed, b"ping");
}
