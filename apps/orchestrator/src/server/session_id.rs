//! Server-owned session-id minter (D-01) -- mirrors `activity::run_id`'s
//! `AtomicU64`-based server-owned-counter idiom, applied here to
//! per-connection identity instead of run identity.
//!
//! The value minted here IS the connection's identity (D-01/D-02): it is
//! minted exclusively server-side, once per accepted connection,
//! immediately after that connection's Hello read -- never derived from,
//! or accepted from, any client-supplied value (a client-sent `session_id`
//! key on `Hello`, however it might arrive, must never be read into this
//! value). `client_name` remains a plain, self-reported display label; it
//! is never promoted back into a reconnect or ownership key.

use std::sync::atomic::{AtomicU64, Ordering};

/// Mints globally-unique, monotonically-increasing session ids (D-01).
/// Intended to be owned once, server-globally -- never constructed
/// per-connection or per-request. Mirrors `run_id::RunIdMinter`'s shape
/// exactly.
#[derive(Debug, Default)]
pub struct SessionIdMinter {
    next: AtomicU64,
}

impl SessionIdMinter {
    /// Creates a fresh minter starting at 0.
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
        }
    }

    /// Mints a fresh, unique session id (D-01). Safe to call concurrently
    /// from any number of callers (async tasks or threads) sharing this
    /// minter via `Arc` -- every call returns a distinct value.
    pub fn mint(&self) -> String {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        format!("sess-{id}")
    }
}

/// Server-global, monotonically increasing session-id source (D-01) --
/// never derived from any client-supplied value, mirroring
/// `connection_registry::next_conn_id`'s server-owned-counter idiom applied
/// here to session identity instead of connection identity.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

/// Mints a fresh session id for a newly-accepted connection, called
/// immediately after that connection's Hello read (D-01) -- never keyed off
/// any client-supplied value. This is the value `handle_connection` sends
/// back on the connection's `Welcome` frame and stores as
/// `ConnEntry.session_id` -- the connection's real identity from this point
/// forward (D-02), never `client_name`.
pub fn next_session_id() -> String {
    let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    format!("sess-{id}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn sequential_mints_differ() {
        let minter = SessionIdMinter::new();
        let first = minter.mint();
        let second = minter.mint();
        assert_ne!(first, second, "sequential mints must produce distinct session ids");
    }

    #[test]
    fn mints_are_strictly_increasing() {
        let minter = SessionIdMinter::new();
        let first = minter.mint();
        let second = minter.mint();
        let first_n: u64 = first
            .strip_prefix("sess-")
            .expect("mint() should produce a sess- prefixed id")
            .parse()
            .expect("suffix should be numeric");
        let second_n: u64 = second
            .strip_prefix("sess-")
            .expect("mint() should produce a sess- prefixed id")
            .parse()
            .expect("suffix should be numeric");
        assert!(second_n > first_n, "expected mints to strictly increase");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_mints_are_all_unique() {
        let minter = Arc::new(SessionIdMinter::new());
        let mut handles = Vec::new();
        for _ in 0..200 {
            let minter = Arc::clone(&minter);
            handles.push(tokio::spawn(async move { minter.mint() }));
        }

        let mut ids = HashSet::new();
        for handle in handles {
            let id = handle.await.expect("mint task panicked");
            assert!(ids.insert(id), "duplicate session id minted under concurrency");
        }
        assert_eq!(ids.len(), 200, "expected 200 unique session ids");
    }

    #[test]
    fn next_session_id_is_monotonically_increasing() {
        let first = next_session_id();
        let second = next_session_id();
        assert!(second > first, "expected next_session_id to strictly increase");
    }

    #[test]
    fn session_id_is_never_derived_from_an_external_value() {
        // D-01 regression guard: mint()/next_session_id() take no
        // arguments at all, so a session id structurally cannot be keyed
        // off a client-supplied value.
        let id = next_session_id();
        assert!(id.starts_with("sess-"), "session id format should remain stable");
    }
}
