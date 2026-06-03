//! Attach client: decode incoming frames into a `RemoteState` (a MetricsSource)
//! and forward events onto a local broadcast bus for the TUI to consume.

use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, watch};

use crate::metrics::{ConnInfo, Event, MetricsSource, Snapshot};

use super::{read_frame, Frame};

/// Latest decoded stats, updated by the decode task and read by the TUI.
pub struct RemoteState {
    inner: Mutex<(Snapshot, Vec<ConnInfo>)>,
}

impl RemoteState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new((Snapshot::default(), Vec::new())),
        })
    }
    fn set(&self, snapshot: Snapshot, connections: Vec<ConnInfo>) {
        *self.inner.lock().unwrap() = (snapshot, connections);
    }
}

impl Default for RemoteState {
    fn default() -> Self {
        Self {
            inner: Mutex::new((Snapshot::default(), Vec::new())),
        }
    }
}

impl MetricsSource for RemoteState {
    fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().0.clone()
    }
    fn connections(&self) -> Vec<ConnInfo> {
        self.inner.lock().unwrap().1.clone()
    }
}

/// Run the decode loop until EOF/io/protocol error, updating `state` and
/// forwarding events onto `events_tx`. On any terminating condition it flips
/// `shutdown` to true and sets `lost` so the caller can print "connection lost".
pub async fn decode_loop<R: AsyncReadExt + Unpin>(
    mut reader: R,
    state: Arc<RemoteState>,
    events_tx: broadcast::Sender<Event>,
    shutdown_tx: watch::Sender<bool>,
    lost: Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        match read_frame(&mut reader).await {
            Ok(Frame::Stats {
                snapshot,
                connections,
            }) => state.set(snapshot, connections),
            Ok(Frame::Event(ev)) => {
                let _ = events_tx.send(ev);
            }
            Ok(Frame::Hello { .. }) => {
                // Unexpected mid-stream Hello: ignore.
            }
            Err(_) => {
                lost.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = shutdown_tx.send(true);
                break;
            }
        }
    }
}

/// Connect to `socket_path`, validate the handshake, then run the TUI fed by
/// the decoded remote stream. Prints "connection lost" to stderr (after the
/// terminal is restored) if the stream ends unexpectedly.
#[cfg(feature = "tui")]
pub async fn attach(socket_path: &std::path::Path) -> std::io::Result<()> {
    use tokio::net::UnixStream;

    let mut stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "未找到运行中的服务（socket: {}）：{e}\n服务在运行吗？",
                socket_path.display()
            );
            std::process::exit(1);
        }
    };

    // First frame must be Hello; validate protocol version.
    let listen_addr = match read_frame(&mut stream).await {
        Ok(Frame::Hello { proto, listen_addr }) => {
            if proto != super::PROTO_VERSION {
                eprintln!(
                    "协议版本不匹配：服务端 {proto}，本客户端 {}。请使用同版本的 next-socks5。",
                    super::PROTO_VERSION
                );
                std::process::exit(1);
            }
            listen_addr
        }
        Ok(other) => {
            eprintln!("协议错误：期望 Hello，收到 {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("读取握手失败：{e}");
            std::process::exit(1);
        }
    };

    let state = RemoteState::new();
    let (ev_tx, ev_rx) = broadcast::channel::<Event>(1024);
    let (sd_tx, sd_rx) = watch::channel(false);
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Decode task reads the rest of the stream.
    let (reader, _writer) = stream.into_split();
    let decode = tokio::spawn(decode_loop(
        reader,
        state.clone(),
        ev_tx,
        sd_tx.clone(),
        lost.clone(),
    ));

    let source: Arc<dyn MetricsSource> = state;
    let res = crate::tui::run(source, ev_rx, sd_tx, sd_rx, listen_addr).await;

    decode.abort();
    // Terminal is restored by tui::run's guard before we return; print here.
    if lost.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!("connection lost");
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{write_frame, Frame};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn decodes_stats_into_state_and_forwards_events() {
        // Encode a Stats then an Event into a buffer, feed it through decode_loop.
        let mut buf: Vec<u8> = Vec::new();
        write_frame(
            &mut buf,
            &Frame::Stats {
                snapshot: Snapshot {
                    total_conns: 42,
                    ..Default::default()
                },
                connections: vec![],
            },
        )
        .await
        .unwrap();
        write_frame(&mut buf, &Frame::Event(Event::Closed { id: 1 }))
            .await
            .unwrap();

        let state = RemoteState::new();
        let (ev_tx, mut ev_rx) = broadcast::channel(16);
        let (sd_tx, _sd_rx) = watch::channel(false);
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let reader: &[u8] = &buf;
        decode_loop(reader, state.clone(), ev_tx, sd_tx, lost.clone()).await;

        // After EOF the loop should have applied the stats and forwarded the event.
        assert_eq!(state.snapshot().total_conns, 42);
        assert!(matches!(ev_rx.try_recv().unwrap(), Event::Closed { id: 1 }));
        // EOF is a terminating condition: lost flag set.
        assert!(lost.load(Ordering::SeqCst));
    }
}
