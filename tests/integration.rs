//! Integration tests exercising the full TCP SOCKS5 path.

use std::sync::Arc;
use std::time::Duration;

use next_socks5::config::{AuthConfig, AuthMethod, Config, Limits, Timeouts};
use next_socks5::metrics::Metrics;
use next_socks5::server;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
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

#[tokio::test]
async fn no_auth_connect_echo() {
    // 1. Start an echo server to act as the upstream target.
    let echo_addr = spawn_echo_server().await;

    // 2. Build config + shared state, bind the proxy listener, spawn the server.
    let cfg = Arc::new(no_auth_config());
    let listener = TcpListener::bind(&cfg.listen).await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let metrics = Metrics::new();
    let (events, _events_rx) = broadcast::channel(64);
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(server::run(
        listener,
        cfg.clone(),
        metrics.clone(),
        events.clone(),
        shutdown_rx,
    ));

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
