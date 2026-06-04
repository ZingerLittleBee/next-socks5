//! Accept-time admission control.
//!
//! Bounds the number of concurrent connections — counting EVERY accepted socket
//! for its whole lifetime, including those still in the handshake — and,
//! optionally, the number of concurrent connections per source IP. This closes
//! the pre-auth/half-open resource-exhaustion gap that a post-request,
//! registered-only limit could not (a stalled handshake never reaches the
//! registry, so it would otherwise be uncounted and unbounded).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// Shared connection admission controller.
pub struct Admission {
    max_total: Option<usize>,
    max_per_ip: Option<usize>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    total: usize,
    per_ip: HashMap<IpAddr, usize>,
}

/// An admitted slot. The connection's resources are counted until this is
/// dropped (i.e. for the whole task lifetime), at which point the global and
/// per-IP counts are released.
pub struct Permit {
    admission: Arc<Admission>,
    ip: IpAddr,
}

impl Admission {
    /// Create a controller. `None` for either bound means "unlimited".
    pub fn new(max_total: Option<usize>, max_per_ip: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            max_total,
            max_per_ip,
            state: Mutex::new(State::default()),
        })
    }

    /// Try to admit a connection from `ip`. Returns a [`Permit`] when under both
    /// the global and the per-IP caps, or `None` when either would be exceeded.
    pub fn try_admit(self: &Arc<Self>, ip: IpAddr) -> Option<Permit> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(max) = self.max_total {
            if st.total >= max {
                return None;
            }
        }
        if let Some(max) = self.max_per_ip {
            if st.per_ip.get(&ip).copied().unwrap_or(0) >= max {
                return None;
            }
        }
        st.total += 1;
        *st.per_ip.entry(ip).or_insert(0) += 1;
        Some(Permit {
            admission: self.clone(),
            ip,
        })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut st = self
            .admission
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        st.total = st.total.saturating_sub(1);
        if let Some(c) = st.per_ip.get_mut(&self.ip) {
            *c -= 1;
            if *c == 0 {
                st.per_ip.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, n))
    }

    #[test]
    fn unlimited_admits_everything() {
        let a = Admission::new(None, None);
        let permits: Vec<_> = (0..100).filter_map(|_| a.try_admit(ip(1))).collect();
        assert_eq!(permits.len(), 100);
    }

    #[test]
    fn global_cap_enforced_and_released() {
        let a = Admission::new(Some(2), None);
        let p1 = a.try_admit(ip(1)).unwrap();
        let _p2 = a.try_admit(ip(2)).unwrap();
        assert!(a.try_admit(ip(3)).is_none(), "third over the global cap");
        drop(p1);
        assert!(a.try_admit(ip(3)).is_some(), "slot freed after drop");
    }

    #[test]
    fn per_ip_cap_enforced_independently() {
        let a = Admission::new(None, Some(1));
        let _p1 = a.try_admit(ip(1)).unwrap();
        assert!(a.try_admit(ip(1)).is_none(), "second from same IP blocked");
        // A different IP is unaffected.
        assert!(a.try_admit(ip(2)).is_some());
    }

    #[test]
    fn per_ip_count_removed_when_zero() {
        let a = Admission::new(None, Some(1));
        let p = a.try_admit(ip(1)).unwrap();
        drop(p);
        // The entry is gone, so a fresh connection from that IP is admitted.
        assert!(a.try_admit(ip(1)).is_some());
    }
}
