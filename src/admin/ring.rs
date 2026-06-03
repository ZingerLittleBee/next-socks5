//! A bounded ring of recent events, used to replay history to a newly attached
//! client. Independent of `tui::LOG_CAPACITY` (separate responsibility).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::metrics::Event;

/// How many recent events to retain for replay.
pub const ADMIN_EVENT_RING_CAPACITY: usize = 500;

/// Shared ring of the most recent events.
#[derive(Clone)]
pub struct EventRing {
    inner: Arc<Mutex<VecDeque<Event>>>,
}

impl EventRing {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(ADMIN_EVENT_RING_CAPACITY))),
        }
    }

    /// Push an event, evicting the oldest when at capacity.
    ///
    /// Recovers from a poisoned lock rather than panicking: this runs in the
    /// service process, and the admin endpoint must never take the proxy down.
    /// The critical sections here are panic-free, so poisoning is not expected.
    pub fn push(&self, ev: Event) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() == ADMIN_EVENT_RING_CAPACITY {
            q.pop_front();
        }
        q.push_back(ev);
    }

    /// Copy the current contents (oldest first) for replay.
    pub fn snapshot(&self) -> Vec<Event> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Spawn a task that fills this ring from the event bus until the sender
    /// drops. Returns the spawned task handle.
    pub fn spawn_filler(
        &self,
        mut events: broadcast::Receiver<Event>,
    ) -> tokio::task::JoinHandle<()> {
        let ring = self.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => ring.push(ev),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_at_capacity() {
        let ring = EventRing::new();
        for i in 0..(ADMIN_EVENT_RING_CAPACITY as u64 + 5) {
            ring.push(Event::Closed { id: i });
        }
        let snap = ring.snapshot();
        assert_eq!(snap.len(), ADMIN_EVENT_RING_CAPACITY);
        // Oldest 5 evicted: first retained id is 5.
        assert!(matches!(snap.first(), Some(Event::Closed { id: 5 })));
    }

    #[tokio::test]
    async fn filler_collects_from_bus() {
        let (tx, rx) = broadcast::channel(16);
        let ring = EventRing::new();
        let handle = ring.spawn_filler(rx);
        tx.send(Event::Closed { id: 1 }).unwrap();
        tx.send(Event::Closed { id: 2 }).unwrap();
        drop(tx); // closes the channel so the filler task ends
        handle.await.unwrap();
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 2);
    }
}
