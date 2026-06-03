//! Shared, lockless-where-possible metrics plus a connection registry and the
//! [`Event`] type used by the broadcast event bus.
//!
//! [`Metrics`] is wrapped in an [`Arc`] and shared between the server tasks (which
//! mutate counters) and the TUI (which samples [`Snapshot`]s and the active
//! connection list). Global counters use atomics with [`Ordering::Relaxed`]
//! because they are independent statistics that need no cross-counter ordering;
//! the per-connection registry is guarded by a [`Mutex`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Kind of proxied connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnKind {
    Connect,
    Udp,
}

/// Per-connection bookkeeping stored in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnInfo {
    pub id: u64,
    pub src: SocketAddr,
    pub target: String,
    pub kind: ConnKind,
    pub up: u64,
    pub down: u64,
}

/// A point-in-time copy of the global counters for the TUI to sample.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub total_conns: u64,
    pub active_conns: u64,
    pub successes: u64,
    pub failures: u64,
    pub error_codes: [u64; 9], // indexed by RFC reply code 0..=8
}

/// Log/lifecycle events delivered to the TUI log panel or stdout (headless).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Connect {
        id: u64,
        src: SocketAddr,
        target: String,
        kind: ConnKind,
    },
    Closed {
        id: u64,
    },
    Auth {
        ok: bool,
        user: String,
    },
    Error {
        code: u8,
        msg: String,
    },
    Log(String),
}

/// Render an [`Event`] into a one-line, human-readable log string.
///
/// Used by both the TUI log panel and the headless (stdout) logger so the two
/// stay consistent. Lives here (rather than in the optional `tui` module) so it
/// is available to headless builds compiled without the `tui` feature.
pub fn format_event(ev: &Event) -> String {
    match ev {
        Event::Connect {
            id,
            src,
            target,
            kind,
        } => {
            let kind = match kind {
                ConnKind::Connect => "CONNECT",
                ConnKind::Udp => "UDP",
            };
            format!("[#{id}] {kind} open {src} -> {target}")
        }
        Event::Closed { id } => format!("[#{id}] closed"),
        Event::Auth { ok, user } => {
            if *ok {
                format!("auth ok for '{user}'")
            } else {
                format!("auth failed (denied) for '{user}'")
            }
        }
        Event::Error { code, msg } => format!("error 0x{code:02x}: {msg}"),
        Event::Log(s) => s.clone(),
    }
}

/// Shared metrics: global atomic counters plus a mutex-guarded connection
/// registry.
pub struct Metrics {
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
    total_conns: AtomicU64,
    active_conns: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    error_codes: [AtomicU64; 9],
    registry: Mutex<HashMap<u64, ConnInfo>>,
    next_id: AtomicU64,
}

impl Metrics {
    /// Create a new, zeroed metrics store wrapped in an [`Arc`] for sharing.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            bytes_up: AtomicU64::new(0),
            bytes_down: AtomicU64::new(0),
            total_conns: AtomicU64::new(0),
            active_conns: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            error_codes: Default::default(),
            registry: Mutex::new(HashMap::new()),
            // Ids start at 1 so that 0 can be used as a sentinel by callers.
            next_id: AtomicU64::new(1),
        })
    }

    /// Allocate a unique id, increment total/active connection counts, and
    /// insert the [`ConnInfo`] into the registry. Returns the new id.
    pub fn register(&self, src: SocketAddr, target: String, kind: ConnKind) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.total_conns.fetch_add(1, Ordering::Relaxed);
        self.active_conns.fetch_add(1, Ordering::Relaxed);
        let info = ConnInfo {
            id,
            src,
            target,
            kind,
            up: 0,
            down: 0,
        };
        self.registry.lock().unwrap().insert(id, info);
        id
    }

    /// Decrement the active connection count and remove the entry from the
    /// registry. No-op for unknown ids beyond the active-count decrement.
    pub fn unregister(&self, id: u64) {
        self.active_conns.fetch_sub(1, Ordering::Relaxed);
        self.registry.lock().unwrap().remove(&id);
    }

    /// Add `n` to the global upload counter and to the per-connection upload
    /// counter (if the connection is still in the registry).
    pub fn add_up(&self, id: u64, n: u64) {
        self.bytes_up.fetch_add(n, Ordering::Relaxed);
        if let Some(info) = self.registry.lock().unwrap().get_mut(&id) {
            info.up += n;
        }
    }

    /// Add `n` to the global download counter and to the per-connection
    /// download counter (if the connection is still in the registry).
    pub fn add_down(&self, id: u64, n: u64) {
        self.bytes_down.fetch_add(n, Ordering::Relaxed);
        if let Some(info) = self.registry.lock().unwrap().get_mut(&id) {
            info.down += n;
        }
    }

    /// Record a failure: increment the failure counter and the per-code
    /// histogram bucket.
    ///
    /// Chosen behavior for out-of-range codes: `failures` is always
    /// incremented, but the histogram bucket is only written when
    /// `code <= 8`. This avoids any out-of-bounds access while still counting
    /// the failure.
    pub fn record_error(&self, code: u8) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        if code <= 8 {
            self.error_codes[code as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a successful request.
    pub fn record_success(&self) {
        self.successes.fetch_add(1, Ordering::Relaxed);
    }

    /// Copy the current global counters into a [`Snapshot`].
    pub fn snapshot(&self) -> Snapshot {
        let mut error_codes = [0u64; 9];
        for (i, slot) in self.error_codes.iter().enumerate() {
            error_codes[i] = slot.load(Ordering::Relaxed);
        }
        Snapshot {
            bytes_up: self.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.bytes_down.load(Ordering::Relaxed),
            total_conns: self.total_conns.load(Ordering::Relaxed),
            active_conns: self.active_conns.load(Ordering::Relaxed),
            successes: self.successes.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            error_codes,
        }
    }

    /// Clone the current list of active connections (for the TUI table).
    pub fn connections(&self) -> Vec<ConnInfo> {
        self.registry.lock().unwrap().values().cloned().collect()
    }

    /// Current number of active connections.
    pub fn active(&self) -> u64 {
        self.active_conns.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1080)
    }

    #[test]
    fn register_assigns_distinct_ids_and_counts() {
        let m = Metrics::new();
        let id1 = m.register(addr(), "a:80".into(), ConnKind::Connect);
        let id2 = m.register(addr(), "b:80".into(), ConnKind::Udp);
        assert_ne!(id1, id2);
        let snap = m.snapshot();
        assert_eq!(snap.total_conns, 2);
        assert_eq!(snap.active_conns, 2);
    }

    #[test]
    fn unregister_decrements_active_only() {
        let m = Metrics::new();
        let id1 = m.register(addr(), "a:80".into(), ConnKind::Connect);
        let _id2 = m.register(addr(), "b:80".into(), ConnKind::Connect);
        m.unregister(id1);
        let snap = m.snapshot();
        assert_eq!(snap.active_conns, 1);
        assert_eq!(snap.total_conns, 2);
    }

    #[test]
    fn add_up_accumulates_global_and_per_conn() {
        let m = Metrics::new();
        let id = m.register(addr(), "a:80".into(), ConnKind::Connect);
        m.add_up(id, 100);
        m.add_up(id, 50);
        assert_eq!(m.snapshot().bytes_up, 150);
        let conn = m.connections().into_iter().find(|c| c.id == id).unwrap();
        assert_eq!(conn.up, 150);
    }

    #[test]
    fn add_down_accumulates_global_and_per_conn() {
        let m = Metrics::new();
        let id = m.register(addr(), "a:80".into(), ConnKind::Connect);
        m.add_down(id, 100);
        m.add_down(id, 50);
        assert_eq!(m.snapshot().bytes_down, 150);
        let conn = m.connections().into_iter().find(|c| c.id == id).unwrap();
        assert_eq!(conn.down, 150);
    }

    #[test]
    fn record_error_counts_failures_and_buckets() {
        let m = Metrics::new();
        m.record_error(5);
        m.record_error(5);
        let snap = m.snapshot();
        assert_eq!(snap.failures, 2);
        assert_eq!(snap.error_codes[5], 2);
    }

    #[test]
    fn record_error_out_of_range_is_safe() {
        let m = Metrics::new();
        m.record_error(200);
        let snap = m.snapshot();
        // Chosen behavior: failures still increments, no histogram bucket
        // written, no panic / out-of-bounds access.
        assert_eq!(snap.failures, 1);
        assert_eq!(snap.error_codes, [0u64; 9]);
    }

    #[test]
    fn record_success_increments() {
        let m = Metrics::new();
        m.record_success();
        m.record_success();
        assert_eq!(m.snapshot().successes, 2);
    }

    #[test]
    fn format_event_auth_failure_mentions_user_and_failure() {
        let s = format_event(&Event::Auth {
            ok: false,
            user: "alice".into(),
        });
        assert!(s.contains("alice"));
        assert!(s.contains("fail") || s.contains("denied"));
    }

    #[test]
    fn event_postcard_round_trip() {
        let ev = Event::Connect {
            id: 7,
            src: addr(),
            target: "example.com:443".into(),
            kind: ConnKind::Udp,
        };
        let bytes = postcard::to_allocvec(&ev).expect("encode");
        let back: Event = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(format_event(&ev), format_event(&back));
    }

    #[test]
    fn snapshot_postcard_round_trip() {
        let snap = Snapshot {
            bytes_up: 1,
            bytes_down: 2,
            total_conns: 3,
            active_conns: 4,
            successes: 5,
            failures: 6,
            error_codes: [1, 0, 0, 0, 0, 0, 0, 0, 2],
        };
        let bytes = postcard::to_allocvec(&snap).expect("encode");
        let back: Snapshot = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(snap.bytes_up, back.bytes_up);
        assert_eq!(snap.error_codes, back.error_codes);
    }
}
