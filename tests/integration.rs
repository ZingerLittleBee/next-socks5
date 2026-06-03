//! Integration tests exercising the full TCP SOCKS5 path.

use std::sync::Arc;
use std::time::Duration;

use next_socks5::config::{AuthConfig, AuthMethod, Config, Limits, Timeouts};
use next_socks5::metrics::Metrics;
use next_socks5::protocol::address::Address;
use next_socks5::protocol::udp;
use next_socks5::server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, watch};

/// Spawn a one-shot TCP echo server. Returns its bound address.
async fn spawn_echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if sock.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    });
    addr
}

/// Build a minimal no-auth config listening on an ephemeral local port.
fn no_auth_config() -> Config {
    Config {
        listen: "127.0.0.1:0".to_string(),
        auth: AuthConfig {
            method: AuthMethod::None,
            users: Vec::new(),
        },
        timeouts: Timeouts::default(),
        limits: Limits::default(),
        public_addr: None,
    }
}

/// Bind the proxy listener and spawn the SOCKS5 server with the given config.
/// Returns the bound proxy address. Shared state (metrics/events/shutdown) is
/// created internally and detached; tests only need the address to connect to.
async fn start_server_with_config(cfg: Config) -> std::net::SocketAddr {
    let cfg = Arc::new(cfg);
    let listener = TcpListener::bind(&cfg.listen).await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let metrics = Metrics::new();
    let (events, _events_rx) = broadcast::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Keep the shutdown sender alive for the duration of the test process so the
    // server never observes a closed channel and shuts down prematurely.
    std::mem::forget(shutdown_tx);
    std::mem::forget(_events_rx);

    tokio::spawn(server::run(
        listener,
        cfg.clone(),
        metrics.clone(),
        events.clone(),
        shutdown_rx,
    ));

    proxy_addr
}

#[tokio::test]
async fn no_auth_connect_echo() {
    // 1. Start an echo server to act as the upstream target.
    let echo_addr = spawn_echo_server().await;

    // 2. Build config + shared state, bind the proxy listener, spawn the server.
    let proxy_addr = start_server_with_config(no_auth_config()).await;

    // 3. Drive a raw client through the full handshake + CONNECT + echo path,
    //    wrapped in a timeout so a hang fails fast instead of blocking.
    let exchange = async {
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // Greeting: VER, NMETHODS=1, METHOD=NO_AUTH.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        // CONNECT request to the echo server as an IPv4 address.
        let echo_v4 = match echo_addr.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => panic!("expected v4 echo addr"),
        };
        let port = echo_addr.port();
        let mut req = vec![0x05, 0x01, 0x00, 0x01];
        req.extend_from_slice(&echo_v4.octets());
        req.extend_from_slice(&port.to_be_bytes());
        client.write_all(&req).await.unwrap();

        // Reply: 10 bytes for an IPv4 BND address; byte 1 is the reply code.
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x00, "expected success reply code");

        // Relay round-trip through the echo server.
        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
    };

    tokio::time::timeout(Duration::from_secs(5), exchange)
        .await
        .expect("client exchange timed out");
}

/// Spawn a UDP echo server: every datagram is sent back to its sender.
/// Returns its bound address.
async fn spawn_udp_echo_server() -> std::net::SocketAddr {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 65536];
        while let Ok((n, src)) = sock.recv_from(&mut buf).await {
            if sock.send_to(&buf[..n], src).await.is_err() {
                break;
            }
        }
    });
    addr
}

#[tokio::test]
async fn udp_associate_echo() {
    let scenario = async {
        // 1. Start a UDP echo server as the relay target.
        let echo_addr = spawn_udp_echo_server().await;
        let echo_v4 = match echo_addr.ip() {
            std::net::IpAddr::V4(v4) => v4,
            std::net::IpAddr::V6(_) => panic!("expected v4 echo addr"),
        };

        // 2. Build config + shared state, bind the proxy listener, spawn the server.
        let proxy_addr = start_server_with_config(no_auth_config()).await;

        // 3. Client: connect TCP control, greeting, then UDP ASSOCIATE request.
        let mut control = TcpStream::connect(proxy_addr).await.unwrap();
        control.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        control.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        // CMD=3 (UDP ASSOCIATE), ATYP v4, addr 0.0.0.0:0. Clients commonly send
        // zeros here, meaning "I'll tell you my UDP source later".
        control
            .write_all(&[0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();

        // Reply: 10 bytes for an IPv4 BND address; byte 1 is the reply code.
        let mut reply = [0u8; 10];
        control.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x00, "expected success reply code");
        // BND.ADDR must never be the unspecified address.
        let bnd_ip = std::net::Ipv4Addr::new(reply[4], reply[5], reply[6], reply[7]);
        assert_ne!(
            bnd_ip,
            std::net::Ipv4Addr::UNSPECIFIED,
            "BND must not be 0.0.0.0"
        );
        let bnd_port = u16::from_be_bytes([reply[8], reply[9]]);
        let relay_udp_addr = std::net::SocketAddr::from((bnd_ip, bnd_port));

        // 4. Build a SOCKS5 UDP datagram targeting the echo server and send it.
        let client_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut out = Vec::new();
        udp::encap(&Address::V4(echo_v4, echo_addr.port()), b"hello", &mut out);
        client_udp.send_to(&out, relay_udp_addr).await.unwrap();

        // 5. Receive the relayed reply and decapsulate it.
        let mut buf = [0u8; 65536];
        let (n, _src) = client_udp.recv_from(&mut buf).await.unwrap();
        let datagram = udp::decap(&buf[..n]).expect("valid SOCKS5 UDP datagram");
        assert_eq!(datagram.data, b"hello");

        // NOTE: source filtering (dropping datagrams whose source IP != the
        // client IP) cannot be exercised here: all sockets bind to 127.0.0.1,
        // so a genuine different-source-IP injection test is not feasible
        // locally. We intentionally do not fake it.

        // Keep the TCP control stream open until the end; the association is
        // bound to it and would be reclaimed if dropped early.
        drop(control);
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("udp associate scenario timed out");
}

/// Build a config requiring RFC 1929 username/password auth with a single
/// `alice`/`secret` credential pair.
fn password_config() -> Config {
    Config {
        listen: "127.0.0.1:0".to_string(),
        auth: AuthConfig {
            method: AuthMethod::Password,
            users: vec![next_socks5::config::User {
                username: "alice".to_string(),
                password: "secret".to_string(),
            }],
        },
        timeouts: Timeouts::default(),
        limits: Limits::default(),
        public_addr: None,
    }
}

/// Encode an RFC 1929 username/password request: VER ULEN UNAME PLEN PASSWD.
fn userpass_request(user: &str, pass: &str) -> Vec<u8> {
    let mut req = vec![0x01, user.len() as u8];
    req.extend_from_slice(user.as_bytes());
    req.push(pass.len() as u8);
    req.extend_from_slice(pass.as_bytes());
    req
}

/// Build an IPv4 CONNECT request for the given address.
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

#[tokio::test]
async fn password_auth_success_connect() {
    let scenario = async {
        // 1. Upstream echo target + a password-protected proxy.
        let echo_addr = spawn_echo_server().await;
        let proxy_addr = start_server_with_config(password_config()).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // 2. Greeting offering NO_AUTH + USERPASS; server must select USERPASS.
        client.write_all(&[0x05, 0x02, 0x00, 0x02]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x02], "server should select userpass");

        // 3. Send correct credentials; expect success reply.
        client
            .write_all(&userpass_request("alice", "secret"))
            .await
            .unwrap();
        let mut auth_reply = [0u8; 2];
        client.read_exact(&mut auth_reply).await.unwrap();
        assert_eq!(auth_reply, [0x01, 0x00], "auth should succeed");

        // 4. CONNECT to the echo server; expect a success reply.
        client
            .write_all(&connect_v4_request(echo_addr))
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x00, "expected success reply code");

        // 5. Relay round-trip through the echo server.
        client.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("password auth success scenario timed out");
}

#[tokio::test]
async fn password_auth_failure() {
    let scenario = async {
        let proxy_addr = start_server_with_config(password_config()).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // Greeting offering NO_AUTH + USERPASS; server selects USERPASS.
        client.write_all(&[0x05, 0x02, 0x00, 0x02]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x02]);

        // Wrong password: expect an auth failure reply.
        client
            .write_all(&userpass_request("alice", "wrong"))
            .await
            .unwrap();
        let mut auth_reply = [0u8; 2];
        client.read_exact(&mut auth_reply).await.unwrap();
        assert_eq!(auth_reply, [0x01, 0x01], "auth should fail");

        // The server must close the connection after a failed auth. A read now
        // sees EOF (0 bytes) or a connection-reset error; either proves closure.
        let mut buf = [0u8; 1];
        match client.read(&mut buf).await {
            Ok(0) => {} // clean EOF: connection closed
            Ok(n) => panic!("expected closed connection, read {n} bytes"),
            Err(_) => {} // reset/abort: also closed
        }
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("password auth failure scenario timed out");
}

#[tokio::test]
async fn connect_refused_maps_0x05() {
    let scenario = async {
        let proxy_addr = start_server_with_config(no_auth_config()).await;

        // Pick a free port by binding then dropping the listener, leaving the
        // port unused. Connecting to it should yield ConnectionRefused on most
        // systems. This is inherently best-effort: the OS may reuse the port,
        // so we accept any non-success error code as proof the mapping works.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // No-auth handshake.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        // CONNECT to the now-free port.
        client
            .write_all(&connect_v4_request(dead_addr))
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        // Expect connection refused (0x05). Document the fallback: if the OS
        // reports a different failure (e.g. host unreachable), the mapping still
        // reached the client, which is what we ultimately verify.
        assert_eq!(
            reply[1], 0x05,
            "expected connection refused (0x05), got {:#04x}",
            reply[1]
        );
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("connect refused scenario timed out");
}

/// Spawn a TCP target that replies only AFTER the client half-closes its write
/// side: it reads until EOF (read returns 0), then writes a known response and
/// closes. Returns its bound address. This models servers (e.g. HTTP/1.0) that
/// produce a response only once the request stream has fully ended.
async fn spawn_reply_after_eof_server(response: &'static [u8]) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            // Drain the request until the peer half-closes (EOF).
            let mut buf = [0u8; 1024];
            loop {
                match sock.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(_) => return,
                }
            }
            // Only now produce the response, then close.
            let _ = sock.write_all(response).await;
            let _ = sock.shutdown().await;
        }
    });
    addr
}

#[tokio::test]
async fn connect_half_close_receives_full_response() {
    const RESPONSE: &[u8] = b"RESPONSE-AFTER-EOF";

    let scenario = async {
        // 1. Target that replies only after the client half-closes.
        let target_addr = spawn_reply_after_eof_server(RESPONSE).await;
        let proxy_addr = start_server_with_config(no_auth_config()).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // No-auth handshake.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        // CONNECT to the target.
        client
            .write_all(&connect_v4_request(target_addr))
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x00, "expected success reply code");

        // Send the request, then half-close the write side. The proxy must keep
        // relaying the upstream's response despite the client's EOF.
        client.write_all(b"REQUEST").await.unwrap();
        client.shutdown().await.unwrap();

        // Read until EOF and assert the full response made it through.
        let mut received = Vec::new();
        client.read_to_end(&mut received).await.unwrap();
        assert_eq!(
            received, RESPONSE,
            "client must receive the full upstream response after half-closing"
        );
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("half-close scenario timed out");
}

#[tokio::test]
async fn unknown_command_replies_0x07() {
    let scenario = async {
        let proxy_addr = start_server_with_config(no_auth_config()).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // No-auth handshake.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        // Request with an unrecognized command byte (0x09) and a valid IPv4 addr.
        client
            .write_all(&[0x05, 0x09, 0x00, 0x01, 127, 0, 0, 1, 0x00, 0x50])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x07, "expected command not supported (0x07)");
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("unknown command scenario timed out");
}

#[tokio::test]
async fn unknown_atype_replies_0x08() {
    let scenario = async {
        let proxy_addr = start_server_with_config(no_auth_config()).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // No-auth handshake.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        // Request with CONNECT but an unknown ATYP (0x09) plus filler bytes.
        client
            .write_all(&[0x05, 0x01, 0x00, 0x09, 0x00, 0x00])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x08, "expected address type not supported (0x08)");
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("unknown atype scenario timed out");
}

#[tokio::test]
async fn bind_command_not_supported() {
    let scenario = async {
        let proxy_addr = start_server_with_config(no_auth_config()).await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();

        // No-auth handshake.
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, 0x00]);

        // BIND request (CMD=0x02) to an arbitrary IPv4 address.
        client
            .write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0x00, 0x50])
            .await
            .unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], 0x07, "expected command not supported (0x07)");
    };

    tokio::time::timeout(Duration::from_secs(5), scenario)
        .await
        .expect("bind not supported scenario timed out");
}
