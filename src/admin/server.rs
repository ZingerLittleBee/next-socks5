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
/// Ownership safety: the path is claimed via [`claim_socket`], which refuses to
/// clobber a socket a live peer is already serving. The returned lock is held
/// for the whole serve lifetime and released on return.
pub async fn serve(
    socket_path: &Path,
    source: Arc<dyn MetricsSource>,
    events: broadcast::Sender<Event>,
    ring: EventRing,
    mut shutdown: watch::Receiver<bool>,
    listen_addr: Option<String>,
) -> std::io::Result<()> {
    // Claim exclusive ownership of the socket path. `_lock` holds the advisory
    // lock for as long as we serve; dropping it on return releases the lock.
    let (listener, _lock) = claim_socket(socket_path).await?;

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
    // Best-effort cleanup of the socket file on shutdown. The sidecar lock file
    // is intentionally left in place so concurrent starts keep observing the
    // same inode; its advisory lock is released when `_lock` is dropped here.
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// Claim exclusive ownership of the admin socket at `socket_path` and bind it.
///
/// Binding a fixed-path Unix socket has a hazard: a second process can unlink a
/// *live* server's socket and rebind the path, silently hijacking it (and then
/// deleting it on its own exit). To prevent that we mirror tmux's race-free
/// bootstrap — connect → flock → re-connect:
///
/// 1. Probe the path with `connect()`; if a live peer answers, refuse.
/// 2. Take a non-blocking exclusive advisory lock on a sidecar `<socket>.lock`
///    file to serialize racing starters. `flock` is released by the kernel on
///    process death, so it can never itself become a stale artifact.
/// 3. Re-probe under the lock — this closes the window where a competitor bound
///    the socket between our first probe and acquiring the lock.
/// 4. Only then unlink any stale socket inode and bind.
///
/// Returns the bound listener and the held lock file. The caller MUST keep the
/// lock alive for as long as the socket is served; dropping it releases the
/// lock. Any "already running" condition is returned as an error, which the
/// caller treats as "disable the admin endpoint" without disturbing the peer.
pub async fn claim_socket(socket_path: &Path) -> std::io::Result<(UnixListener, std::fs::File)> {
    ensure_parent_dir(socket_path)?;

    // 1. Cheap liveness probe: never disturb a socket a live peer is serving.
    if socket_is_live(socket_path).await? {
        return Err(already_running(socket_path));
    }

    // 2. Serialize racing starters with a lifetime advisory lock.
    let lock = acquire_lock(socket_path)?;

    // 3. Authoritative re-probe under the lock (TOCTOU close).
    if socket_is_live(socket_path).await? {
        return Err(already_running(socket_path));
    }

    // 4. The path is ours: clear any stale socket inode and bind.
    unlink_if_socket(socket_path)?;
    let listener = UnixListener::bind(socket_path)?;
    // The admin stream carries sensitive telemetry (client IPs, targets, usage,
    // usernames) and has no application-level auth, so restrict the socket to
    // the owner only rather than relying on the process umask.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((listener, lock))
}

/// Ensure the socket's parent directory exists (the default /run/next-socks5 may
/// not). Only a directory we create ourselves is chmod'd 0700 — never an
/// existing, possibly shared, directory like /run.
fn ensure_parent_dir(socket_path: &Path) -> std::io::Result<()> {
    if let Some(parent) = socket_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    Ok(())
}

/// Probe whether a live server is listening on `socket_path`.
///
/// `Ok(true)` — a peer accepted our connection (live). `Ok(false)` — the path is
/// absent or refuses connections (`ENOENT`/`ECONNREFUSED`), i.e. a stale/missing
/// socket that is safe to bind. Any other error is propagated so we err on the
/// side of NOT clobbering an endpoint we cannot reason about.
async fn socket_is_live(socket_path: &Path) -> std::io::Result<bool> {
    match UnixStream::connect(socket_path).await {
        Ok(_) => Ok(true),
        Err(e)
            if e.kind() == std::io::ErrorKind::ConnectionRefused
                || e.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Path of the sidecar lock file for `socket_path` (`<socket>.lock`).
fn lock_path_for(socket_path: &Path) -> std::path::PathBuf {
    let mut s = socket_path.as_os_str().to_owned();
    s.push(".lock");
    std::path::PathBuf::from(s)
}

/// Take a non-blocking exclusive advisory lock on `<socket>.lock`, returning the
/// open file that holds the lock until dropped. A `WouldBlock` error means
/// another instance already holds it (and is therefore the live/starting owner).
fn acquire_lock(socket_path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let lock_path = lock_path_for(socket_path);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // The file is only a flock anchor; never truncate it, and keep the inode
        // stable so concurrent starters lock the same object.
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;

    // SAFETY: `file.as_raw_fd()` is a valid fd owned for the lifetime of `file`;
    // flock with LOCK_NB never blocks and only touches that fd.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // EWOULDBLOCK/EAGAIN: a live (or concurrently starting) instance holds
        // the lock. Surface it as WouldBlock so the caller can report it.
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!(
                    "another next-socks5 instance already holds {}",
                    lock_path.display()
                ),
            ));
        }
        return Err(err);
    }
    Ok(file)
}

/// Error returned when a live instance already owns the admin socket.
fn already_running(socket_path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!(
            "admin socket {} is already served by a live next-socks5 instance",
            socket_path.display()
        ),
    )
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
