//! Server-owned `run_id` minter (D-02/D-05) -- fixes RESEARCH Pitfall 5.
//!
//! The prior scheme (`format!("run-{id}")` in `server/mod.rs`) derived
//! `run_id` from the per-connection WS envelope `id`, which every client
//! restarts at 0 -- two clients connected at once produce colliding
//! `run-0`/`run-1`/... values. `RunIdMinter` instead wraps a single
//! `AtomicU64` owned server-globally (by the `ActivityRegistry`, see
//! `activity/mod.rs`), never keyed off any client-supplied or
//! per-connection value.

use std::sync::atomic::{AtomicU64, Ordering};

/// Mints globally-unique, monotonically-increasing `run_id`s. Intended to be
/// owned once, server-globally, by the `ActivityRegistry` -- never
/// constructed per-connection or per-request.
#[derive(Debug, Default)]
pub struct RunIdMinter {
    next: AtomicU64,
}

impl RunIdMinter {
    /// Creates a fresh minter starting at 0. Only one of these should exist
    /// per running daemon (owned by the server-global `ActivityRegistry`).
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
        }
    }

    /// Creates a minter whose next mint begins at `start` -- used when
    /// rebuilding a registry from disk (`ActivityRegistry::from_store`) so
    /// a restarted daemon never re-mints a `run_id` already present in its
    /// persisted history.
    pub fn with_start(start: u64) -> Self {
        Self {
            next: AtomicU64::new(start),
        }
    }

    /// Mints a fresh, unique `run_id`. Safe to call concurrently from any
    /// number of callers (async tasks or threads) sharing this minter via
    /// `Arc` -- every call returns a distinct value.
    pub fn mint(&self) -> String {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        format!("run-{id}")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::*;

    #[test]
    fn sequential_mints_differ() {
        let minter = RunIdMinter::new();
        let first = minter.mint();
        let second = minter.mint();
        assert_ne!(first, second, "sequential mints must produce distinct run_ids");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_mints_are_all_unique() {
        let minter = Arc::new(RunIdMinter::new());
        let mut handles = Vec::new();
        for _ in 0..200 {
            let minter = Arc::clone(&minter);
            handles.push(tokio::spawn(async move { minter.mint() }));
        }

        let mut ids = HashSet::new();
        for handle in handles {
            let id = handle.await.expect("mint task panicked");
            assert!(ids.insert(id), "duplicate run_id minted under concurrency");
        }
        assert_eq!(ids.len(), 200, "expected 200 unique run_ids");
    }

    #[test]
    fn run_id_never_derived_from_an_external_value() {
        // Pitfall 5 regression guard: mint() takes no arguments at all, so
        // it structurally cannot be keyed off a connection/envelope id.
        let minter = RunIdMinter::new();
        let id = minter.mint();
        assert!(id.starts_with("run-"), "run_id format should remain stable");
    }
}
