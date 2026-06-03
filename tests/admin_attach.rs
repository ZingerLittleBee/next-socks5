//! Integration test for the admin attach endpoint: bind a socket, connect a
//! raw client, and assert the Hello → replay → Stats sequence.

use std::sync::Arc;
use std::time::Duration;

use next_socks5::admin::{read_frame, serve, EventRing, Frame, PROTO_VERSION};
use next_socks5::metrics::{ConnKind, Event, Metrics};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, watch};

#[tokio::test]
async fn attach_receives_hello_replay_and_stats() {
    let dir = std::env::temp_dir().join(format!("ns5-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("admin.sock");

    let metrics = Metrics::new();
    metrics.register(
        "127.0.0.1:5000".parse().unwrap(),
        "host:80".into(),
        ConnKind::Connect,
    );
    let (events_tx, _evrx) = broadcast::channel::<Event>(64);
    let ring = EventRing::new();
    ring.push(Event::Log("historic line".into()));
    let (sd_tx, sd_rx) = watch::channel(false);

    let source: Arc<dyn next_socks5::metrics::MetricsSource> = metrics.clone();
    let sock2 = sock.clone();
    let server = tokio::spawn(async move {
        serve(&sock2, source, events_tx, ring, sd_rx, Some("127.0.0.1:1080".into()))
            .await
            .unwrap();
    });

    // Give the listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut client = UnixStream::connect(&sock).await.expect("connect");

    // 1) Hello.
    match read_frame(&mut client).await.unwrap() {
        Frame::Hello { proto, listen_addr } => {
            assert_eq!(proto, PROTO_VERSION);
            assert_eq!(listen_addr.as_deref(), Some("127.0.0.1:1080"));
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    // 2) Replayed historic event.
    assert!(matches!(
        read_frame(&mut client).await.unwrap(),
        Frame::Event(Event::Log(_))
    ));
    // 3) A periodic Stats frame within a couple ticks.
    let stats = read_frame(&mut client).await.unwrap();
    match stats {
        Frame::Stats {
            snapshot,
            connections,
        } => {
            assert_eq!(snapshot.total_conns, 1);
            assert_eq!(connections.len(), 1);
        }
        other => panic!("expected Stats, got {other:?}"),
    }

    sd_tx.send(true).unwrap();
    let _ = server.await;
    let _ = std::fs::remove_dir_all(&dir);
}
