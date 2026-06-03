//! TCP server: accept loop, per-connection state machine, and CONNECT relay.

pub mod connect;
pub mod connection;

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

use crate::config::Config;
use crate::metrics::{Event, Metrics};

/// Run the accept loop until `shutdown` flips to `true`.
///
/// Each accepted socket is handed to [`connection::handle`] on its own task.
/// On shutdown the loop stops accepting and drains in-flight tasks before
/// returning.
pub async fn run(
    listener: TcpListener,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    // JoinSet tracks spawned connection tasks so shutdown can drain them.
    let mut tasks = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            // Reap finished tasks so the JoinSet does not grow unbounded.
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}

            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let cfg = cfg.clone();
                        let metrics = metrics.clone();
                        let events = events.clone();
                        let shutdown = shutdown.clone();
                        tasks.spawn(connection::handle(
                            stream, peer, cfg, metrics, events, shutdown,
                        ));
                    }
                    Err(e) => {
                        // Accept errors are transient (e.g. fd exhaustion); log
                        // and keep serving rather than tearing down the server.
                        let _ = events.send(Event::Log(format!("accept error: {e}")));
                    }
                }
            }

            res = shutdown.changed() => {
                // Stop accepting on an explicit shutdown signal or when the
                // sender is dropped (Err).
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    // Drain in-flight connections so they can finish cleanly.
    while tasks.join_next().await.is_some() {}

    Ok(())
}
