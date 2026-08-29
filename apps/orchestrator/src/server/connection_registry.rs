//! Connection fan-out registry (D-01, RESEARCH Pattern 1): the accept loop
//! registers each connection's outbound `mpsc::UnboundedSender<Envelope>`
//! here (after that connection's Hello handshake); `broadcast` reaches
//! every registered connection without ever blocking on, or panicking
//! because of, one slow/dead one (Pitfall 1). Mirrors `server/mod.rs`'s
//! existing `futures_util::lock::Mutex` choice for its `WsWrite` lock, so
//! this module uses the same `Mutex` type for consistency, not
//! `std::sync::Mutex`/`tokio::sync::Mutex`.
//!
//! Deliberately NOT tokio's broadcast channel (`tokio::sync::` module,
//! `broadcast` submodule -- RESEARCH anti-pattern): that primitive silently
//! drops the oldest queued message once any one receiver falls behind
//! (`RecvError::Lagged`), which is unacceptable when the whole feature is
//! "every client sees every activity, reliably."

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::lock::Mutex;
use tokio::sync::mpsc;

use shared::Envelope;

/// Identifies one accepted WS connection for the lifetime of that
/// connection only — never persisted across a reconnect, never
/// client-supplied.
pub type ConnId = u64;

type EventTx = mpsc::UnboundedSender<Envelope>;

struct ConnEntry {
    client_name: String,
    tx: EventTx,
}

/// Shared, cheaply-`Clone`-able (an `Arc` clone) fan-out registry:
/// `ConnId -> (client_name, sender)`. Every spawned connection task holds
/// its own handle to the same underlying map.
#[derive(Default, Clone)]
pub struct ConnectionRegistry {
    inner: Arc<Mutex<HashMap<ConnId, ConnEntry>>>,
}

/// Server-global, monotonically increasing connection-id source — never
/// derived from any client-supplied value (mirrors `activity::run_id`'s
/// server-owned-counter idiom, applied here to connection identity instead
/// of activity identity).
static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(0);

/// Mints a fresh `ConnId` for a newly-accepted connection.
pub fn next_conn_id() -> ConnId {
    NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed)
}

impl ConnectionRegistry {
    /// Registers `id` with its self-reported `client_name` (from Hello,
    /// D-03/D-04) and the sender half of its per-connection mpsc channel.
    pub async fn register(&self, id: ConnId, client_name: String, tx: EventTx) {
        self.inner.lock().await.insert(id, ConnEntry { client_name, tx });
    }

    /// Removes `id`'s entry. Must run unconditionally on every
    /// connection-exit path (Pitfall 2) — a subsequent `broadcast` will no
    /// longer target this connection.
    pub async fn deregister(&self, id: ConnId) {
        self.inner.lock().await.remove(&id);
    }

    /// Updates the recorded `client_name` for an already-registered
    /// connection, if still present.
    #[allow(dead_code)] // available for a future late-identify path; unused by 05-04 itself
    pub async fn set_client_name(&self, id: ConnId, client_name: String) {
        if let Some(entry) = self.inner.lock().await.get_mut(&id) {
            entry.client_name = client_name;
        }
    }

    /// Looks up the client_name recorded for `id`, if it is still
    /// registered.
    #[allow(dead_code)] // available for a future "which client ran this" lookup; unused by 05-04 itself
    pub async fn client_name(&self, id: ConnId) -> Option<String> {
        self.inner.lock().await.get(&id).map(|entry| entry.client_name.clone())
    }

    /// Fans `env` out to every registered connection. Never blocks on, and
    /// never panics because of, a slow or already-disconnected receiver
    /// (Pitfall 1): `unbounded_send` only errors when that one receiver has
    /// already been dropped, which is a handled no-op — exactly like this
    /// module's existing `send_frame` contract in `server/mod.rs`.
    pub async fn broadcast(&self, env: &Envelope) {
        let guard = self.inner.lock().await;
        for entry in guard.values() {
            let _ = entry.tx.send(env.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(name: &str) -> Envelope {
        Envelope::Hello {
            client_name: name.to_string(),
        }
    }

    #[tokio::test]
    async fn register_then_broadcast_delivers_to_that_connection() {
        let registry = ConnectionRegistry::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry.register(1, "test-client".to_string(), tx).await;

        registry.broadcast(&hello("orchestrator-cli")).await;

        match rx.recv().await {
            Some(Envelope::Hello { client_name }) => {
                assert_eq!(client_name, "orchestrator-cli");
            }
            other => panic!("expected a delivered Hello envelope, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn broadcast_reaches_two_registered_connections() {
        let registry = ConnectionRegistry::default();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        registry.register(1, "a".to_string(), tx1).await;
        registry.register(2, "b".to_string(), tx2).await;

        registry.broadcast(&hello("orchestrator-tui")).await;

        assert!(rx1.recv().await.is_some(), "connection 1 must receive the broadcast");
        assert!(rx2.recv().await.is_some(), "connection 2 must receive the broadcast");
    }

    #[tokio::test]
    async fn broadcast_to_a_dropped_receiver_is_a_handled_no_op_and_still_reaches_the_survivor() {
        let registry = ConnectionRegistry::default();
        let (tx1, rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        registry.register(1, "dead".to_string(), tx1).await;
        registry.register(2, "alive".to_string(), tx2).await;
        drop(rx1); // simulate a disconnected client whose receiver is gone

        // Must not panic, and must still reach the surviving connection.
        registry.broadcast(&hello("orchestrator-cli")).await;

        assert!(
            rx2.recv().await.is_some(),
            "the surviving connection must still receive the broadcast"
        );
    }

    #[tokio::test]
    async fn deregister_removes_the_entry_so_broadcast_no_longer_targets_it() {
        let registry = ConnectionRegistry::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        registry.register(1, "test-client".to_string(), tx).await;
        registry.deregister(1).await;

        registry.broadcast(&hello("orchestrator-cli")).await;

        assert!(
            rx.try_recv().is_err(),
            "a deregistered connection must not receive further broadcasts"
        );
    }

    #[test]
    fn next_conn_id_is_monotonically_increasing() {
        let first = next_conn_id();
        let second = next_conn_id();
        assert!(second > first, "expected next_conn_id to strictly increase");
    }
}
