//! Synthetic metrics generator for demos and screenshots.
//!
//! When enabled (`--mock`), this drives the dashboard with fake connections,
//! wave-shaped throughput and lifecycle/error events by calling the same
//! [`Metrics`] and event-bus APIs the real proxy uses — **no real network
//! traffic is involved**. It runs until the shutdown channel flips, then
//! unregisters its synthetic connections so the dashboard returns to idle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, watch};

use crate::metrics::{ConnKind, Event, Metrics};

/// How often the generator advances (drives throughput + lifecycle).
const STEP: Duration = Duration::from_millis(200);
/// Upper bound on simultaneously "active" synthetic connections.
const MAX_ACTIVE: usize = 12;

/// Tiny dependency-free xorshift PRNG — enough variety for demo data.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `0..n` (returns 0 when `n == 0`).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// Mutable generator state, separated from the timer loop so a single tick
/// ([`step`]) is deterministic and unit-testable without real time.
struct Generator {
    rng: Rng,
    /// Currently "open" synthetic connections (id + kind).
    active: Vec<(u64, ConnKind)>,
    /// Phase accumulator driving the throughput wave.
    phase: f64,
}

const TARGETS: [&str; 6] = [
    "example.com:443",
    "github.com:443",
    "api.ipify.org:443",
    "cdn.example.net:80",
    "db.internal:5432",
    "speed.example.com:443",
];
const USERS: [&str; 3] = ["alice", "bob", "carol"];

impl Generator {
    fn new() -> Self {
        Self {
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            active: Vec::new(),
            phase: 0.0,
        }
    }

    /// Advance one tick: maybe open/close connections, push bytes on the active
    /// ones (wave-shaped), and occasionally emit auth/error events.
    fn step(&mut self, metrics: &Metrics, events: &broadcast::Sender<Event>) {
        self.phase += 0.15;
        let wave = self.phase.sin() * 0.5 + 0.5; // 0.0 ..= 1.0

        // Maybe open a new connection.
        if self.active.len() < MAX_ACTIVE && self.rng.below(100) < 40 {
            let kind = if self.rng.below(100) < 75 {
                ConnKind::Connect
            } else {
                ConnKind::Udp
            };
            let addr: SocketAddr = format!(
                "10.0.{}.{}:{}",
                self.rng.below(256),
                self.rng.below(256),
                1024 + self.rng.below(60000)
            )
            .parse()
            .expect("synthetic addr is valid");
            let target = TARGETS[self.rng.below(TARGETS.len() as u64) as usize].to_string();
            let id = metrics.register(addr, target.clone(), kind);
            let _ = events.send(Event::Connect {
                id,
                src: addr,
                target,
                kind,
            });
            self.active.push((id, kind));

            // Occasionally an auth event accompanies a new connection.
            if self.rng.below(100) < 30 {
                let ok = self.rng.below(100) < 85;
                let user = USERS[self.rng.below(USERS.len() as u64) as usize].to_string();
                let _ = events.send(Event::Auth { ok, user });
            }
        }

        // Push wave-shaped bytes onto every active connection.
        let budget = (wave * 100_000.0) as u64; // peak ~500 KB/s per conn at 200ms
        for (id, _) in &self.active {
            let up = self.rng.below(budget / 8 + 1) + budget / 16;
            let down = self.rng.below(budget + 1) + budget / 4;
            metrics.add_up(*id, up);
            metrics.add_down(*id, down);
        }

        // Maybe close a connection (counts as a success).
        if !self.active.is_empty() && self.rng.below(100) < 30 {
            let i = self.rng.below(self.active.len() as u64) as usize;
            let (id, _) = self.active.remove(i);
            metrics.unregister(id);
            metrics.record_success();
            let _ = events.send(Event::Closed { id });
        }

        // Occasionally a request fails (drives the error histogram).
        if self.rng.below(100) < 12 {
            let code = 1 + self.rng.below(8) as u8; // RFC reply codes 0x01..=0x08
            metrics.record_error(code);
            let _ = events.send(Event::Error {
                code,
                msg: format!("synthetic error 0x{code:02x}"),
            });
        }
    }

    /// Unregister all remaining synthetic connections.
    fn drain(&mut self, metrics: &Metrics) {
        for (id, _) in self.active.drain(..) {
            metrics.unregister(id);
        }
    }
}

/// Run the synthetic generator until `shutdown` flips true, then clean up.
pub async fn run(
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut gen = Generator::new();
    let mut ticker = tokio::time::interval(STEP);
    let _ = events.send(Event::Log("mock data generator running".to_string()));
    loop {
        tokio::select! {
            _ = ticker.tick() => gen.step(&metrics, &events),
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    gen.drain(&metrics);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_populates_metrics_then_drain_clears_active() {
        let metrics = Metrics::new();
        let (tx, _rx) = broadcast::channel::<Event>(1024);
        let mut gen = Generator::new();

        // Many ticks should open connections and move bytes.
        for _ in 0..200 {
            gen.step(&metrics, &tx);
        }
        let snap = metrics.snapshot();
        assert!(snap.total_conns > 0, "should have opened connections");
        assert!(
            snap.bytes_up > 0 && snap.bytes_down > 0,
            "should have moved bytes"
        );
        assert!(!gen.active.is_empty(), "some connections still active");

        // Draining unregisters the remaining synthetic connections.
        gen.drain(&metrics);
        assert_eq!(metrics.snapshot().active_conns, 0);
    }
}
