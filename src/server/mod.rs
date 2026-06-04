//! TCP server: accept loop, per-connection state machine, and CONNECT relay.

pub mod admission;
pub mod connect;
pub mod connection;
pub mod udp;

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

use crate::config::Config;
use crate::metrics::{Event, Metrics};

use admission::Admission;

/// Run the accept loop until `shutdown` flips to `true`.
///
/// Each accepted socket is admitted through [`Admission`] (bounding total and
/// per-IP concurrency, half-open connections included) and, if admitted, handed
/// to [`connection::handle`] on its own task. On shutdown the loop stops
/// accepting and drains in-flight tasks before returning; the shutdown signal is
/// forwarded into each connection so in-flight relays wind down too.
pub async fn run(
    listener: TcpListener,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    // JoinSet tracks spawned connection tasks so shutdown can drain them.
    let mut tasks = tokio::task::JoinSet::new();
    let admission = Admission::new(cfg.limits.max_connections, cfg.limits.max_per_ip);

    loop {
        tokio::select! {
            // Reap finished tasks so the JoinSet does not grow unbounded.
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}

            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        // Admit the connection at accept time, counting it (and
                        // any per-IP budget) for its whole lifetime. Over-limit
                        // connections are dropped now rather than after a
                        // handshake, so a half-open flood cannot bypass the cap.
                        let permit = match admission.try_admit(peer.ip()) {
                            Some(p) => p,
                            None => {
                                let _ = events.send(Event::Log(format!(
                                    "connection from {peer} rejected: limit reached"
                                )));
                                continue;
                            }
                        };
                        let cfg = cfg.clone();
                        let metrics = metrics.clone();
                        let events = events.clone();
                        let shutdown = shutdown.clone();
                        tasks.spawn(connection::handle(
                            stream, peer, cfg, metrics, events, shutdown, permit,
                        ));
                    }
                    Err(e) => {
                        // Accept errors are transient (e.g. fd exhaustion); log
                        // and keep serving rather than tearing down the server.
                        // Back off briefly so a persistent error (e.g. EMFILE,
                        // where the listener stays readable) does not spin the
                        // loop at 100% CPU.
                        let _ = events.send(Event::Log(format!("accept error: {e}")));
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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

    // Drain in-flight connections so they can finish cleanly. They observe the
    // shutdown signal and wind down promptly, so this does not block on
    // long-lived relays.
    while tasks.join_next().await.is_some() {}

    Ok(())
}
