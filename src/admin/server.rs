//! Unix-socket admin listener: accept attach clients and stream Hello → replay
//! → periodic Stats + live Events to each.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};

use crate::metrics::{Event, MetricsSource};

use super::ring::EventRing;
use super::{write_frame, Frame, PROTO_VERSION};

/// How often a client is pushed a fresh Stats frame.
const PUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Bind `socket_path` and serve attach clients until `shutdown` flips true.
///
/// Bind safety: if the path already exists, it is only unlinked when it is
/// actually a socket file; otherwise this returns an error rather than
/// clobbering an unrelated file/dir.
pub async fn serve(
    socket_path: &Path,
    source: Arc<dyn MetricsSource>,
    events: broadcast::Sender<Event>,
    ring: EventRing,
    mut shutdown: watch::Receiver<bool>,
    listen_addr: Option<String>,
) -> std::io::Result<()> {
    unlink_if_socket(socket_path)?;
    let listener = UnixListener::bind(socket_path)?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let source = source.clone();
                    let events_rx = events.subscribe();
                    let replay = ring.snapshot();
                    let shutdown = shutdown.clone();
                    let listen_addr = listen_addr.clone();
                    tokio::spawn(async move {
                        // Per-client errors (e.g. client disconnect) are ignored:
                        // they must not affect the proxy or other clients.
                        let _ = handle_client(stream, source, events_rx, replay, shutdown, listen_addr).await;
                    });
                }
            }
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    // Best-effort cleanup of the socket file on shutdown.
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

fn unlink_if_socket(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

async fn handle_client(
    mut stream: UnixStream,
    source: Arc<dyn MetricsSource>,
    mut events: broadcast::Receiver<Event>,
    replay: Vec<Event>,
    mut shutdown: watch::Receiver<bool>,
    listen_addr: Option<String>,
) -> Result<(), super::FrameError> {
    write_frame(
        &mut stream,
        &Frame::Hello {
            proto: PROTO_VERSION,
            listen_addr,
        },
    )
    .await?;
    for ev in replay {
        write_frame(&mut stream, &Frame::Event(ev)).await?;
    }

    let mut ticker = tokio::time::interval(PUSH_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let frame = Frame::Stats {
                    snapshot: source.snapshot(),
                    connections: source.connections(),
                };
                write_frame(&mut stream, &frame).await?;
            }
            ev = events.recv() => {
                match ev {
                    Ok(ev) => write_frame(&mut stream, &Frame::Event(ev)).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() { break; }
            }
        }
    }
    Ok(())
}
