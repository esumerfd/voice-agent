//! Reload-aware embedding cache (D-06): keyed by workflow id, synced
//! against the registry's current `(id, intent)` pairs on every
//! `Router::route` call. Recomputed only for workflows whose `intent`
//! string changed (or that are newly added); evicted for ids no longer
//! present; never rebuilt from scratch per request.
//!
//! Owned by the long-lived `Router` (see the Architecture Deviation note in
//! 09-01-PLAN.md), NOT by the per-request `Registry` -- `Registry::load`
//! runs fresh on every single request in this codebase
//! (`InProcessOrchestrator`), so a cache attached to `Registry` would be
//! reconstructed, and every embedding recomputed, on every request --
//! exactly the anti-pattern D-06 forbids.

use std::collections::{HashMap, HashSet};

use crate::error::RouterError;
use crate::router::ollama_client::{embed_with_retry, OllamaApi};

/// One cached embedding plus the exact `intent` string it was computed
/// from, so a later sync can detect a byte-for-byte change. Comparison is
/// deliberately exact-byte, never normalized (see `router::mod`'s doc
/// comment on `route` -- two Unicode-normalization variants of the same
/// visible text are two different strings and re-embed, the safe
/// direction).
struct CacheEntry {
    intent: String,
    embedding: Vec<f32>,
}

/// D-06's cache contract, numerically provable via this struct rather than
/// merely described: `recomputed` counts workflows whose embedding was
/// actually (re)computed THIS sync call, `reused` counts those served from
/// cache unchanged, `evicted` counts ids removed because they're no longer
/// in `wanted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncStats {
    pub recomputed: usize,
    pub reused: usize,
    pub evicted: usize,
}

/// `HashMap<String, CacheEntry>` keyed by workflow id -- same keying
/// convention as `Registry`'s own `definitions` map.
#[derive(Default)]
pub struct EmbeddingCache {
    entries: HashMap<String, CacheEntry>,
}

impl EmbeddingCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mirrors `Registry::lookup`'s shape: `None` when this id has no
    /// cached embedding (never synced, or evicted).
    pub fn lookup(&self, id: &str) -> Option<&[f32]> {
        self.entries.get(id).map(|e| e.embedding.as_slice())
    }

    /// Syncs this cache against the registry's current `(id, intent)`
    /// pairs. `wanted` must never contain a pair whose `intent` is empty
    /// after trimming -- the caller (`Router::route`) filters those out
    /// before calling, so an empty-intent workflow is never embedded and
    /// never cached.
    ///
    /// Evicts ids absent from `wanted`; keeps entries whose stored `intent`
    /// is byte-identical to the incoming one (`reused`); batches the
    /// remainder into ONE `embed_with_retry` call (`recomputed`) -- never
    /// one call per changed workflow.
    pub async fn sync(
        &mut self,
        wanted: &[(String, String)],
        client: &dyn OllamaApi,
        model: &str,
    ) -> Result<SyncStats, RouterError> {
        let wanted_ids: HashSet<&str> = wanted.iter().map(|(id, _)| id.as_str()).collect();

        let stale_ids: Vec<String> = self
            .entries
            .keys()
            .filter(|id| !wanted_ids.contains(id.as_str()))
            .cloned()
            .collect();
        let evicted = stale_ids.len();
        for id in &stale_ids {
            self.entries.remove(id);
        }

        let mut to_embed_ids: Vec<String> = Vec::new();
        let mut to_embed_intents: Vec<String> = Vec::new();
        let mut reused = 0usize;
        for (id, intent) in wanted {
            match self.entries.get(id) {
                Some(entry) if entry.intent == *intent => reused += 1,
                _ => {
                    to_embed_ids.push(id.clone());
                    to_embed_intents.push(intent.clone());
                }
            }
        }

        let recomputed = to_embed_ids.len();
        if recomputed > 0 {
            let embeddings = embed_with_retry(client, model, &to_embed_intents).await?;
            if embeddings.len() != to_embed_ids.len() {
                return Err(RouterError::MalformedResponse {
                    endpoint: format!("{model}: /api/embed"),
                    detail: format!(
                        "expected {} embeddings for the batched corpus sync, got {}",
                        to_embed_ids.len(),
                        embeddings.len()
                    ),
                });
            }
            for ((id, intent), embedding) in
                to_embed_ids.into_iter().zip(to_embed_intents).zip(embeddings)
            {
                self.entries.insert(id, CacheEntry { intent, embedding });
            }
        }

        Ok(SyncStats {
            recomputed,
            reused,
            evicted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deterministic, call-counting `OllamaApi` test double for cache-level
    /// tests -- the exact vector values don't matter here, only that a
    /// batch call embeds every requested string in one round trip.
    struct CountingClient {
        call_count: AtomicUsize,
    }

    impl CountingClient {
        fn new() -> Self {
            Self { call_count: AtomicUsize::new(0) }
        }
        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl OllamaApi for CountingClient {
        async fn embed(&self, _model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(inputs.iter().map(|s| vec![s.len() as f32]).collect())
        }
    }

    fn pair(id: &str, intent: &str) -> (String, String) {
        (id.to_string(), intent.to_string())
    }

    #[tokio::test]
    async fn first_sync_recomputes_every_wanted_entry() {
        let client = CountingClient::new();
        let mut cache = EmbeddingCache::new();

        let wanted = vec![pair("a", "check calendar"), pair("b", "set a timer")];
        let stats = cache.sync(&wanted, &client, "nomic-embed-text").await.expect("sync should succeed");

        assert_eq!(stats, SyncStats { recomputed: 2, reused: 0, evicted: 0 });
        assert_eq!(client.call_count(), 1, "expected the corpus to embed in ONE batched call");
        assert!(cache.lookup("a").is_some());
        assert!(cache.lookup("b").is_some());
    }

    #[tokio::test]
    async fn second_sync_against_the_same_pairs_reuses_everything() {
        let client = CountingClient::new();
        let mut cache = EmbeddingCache::new();
        let wanted = vec![pair("a", "check calendar"), pair("b", "set a timer")];

        cache.sync(&wanted, &client, "nomic-embed-text").await.expect("first sync");
        let stats = cache.sync(&wanted, &client, "nomic-embed-text").await.expect("second sync");

        assert_eq!(stats, SyncStats { recomputed: 0, reused: 2, evicted: 0 });
        assert_eq!(client.call_count(), 1, "expected zero additional embed calls on an unchanged sync");
    }

    #[tokio::test]
    async fn a_changed_intent_re_embeds_only_that_one_workflow() {
        let client = CountingClient::new();
        let mut cache = EmbeddingCache::new();
        cache
            .sync(&[pair("a", "check calendar"), pair("b", "set a timer")], &client, "m")
            .await
            .expect("first sync");

        let stats = cache
            .sync(&[pair("a", "check calendar events today"), pair("b", "set a timer")], &client, "m")
            .await
            .expect("second sync");

        assert_eq!(stats, SyncStats { recomputed: 1, reused: 1, evicted: 0 });
    }

    #[tokio::test]
    async fn a_removed_workflow_id_is_evicted_and_no_longer_looked_up() {
        let client = CountingClient::new();
        let mut cache = EmbeddingCache::new();
        cache
            .sync(&[pair("a", "check calendar"), pair("b", "set a timer")], &client, "m")
            .await
            .expect("first sync");
        assert!(cache.lookup("b").is_some());

        let stats = cache.sync(&[pair("a", "check calendar")], &client, "m").await.expect("second sync");

        assert_eq!(stats, SyncStats { recomputed: 0, reused: 1, evicted: 1 });
        assert!(cache.lookup("b").is_none(), "expected the evicted id to no longer be looked up");
    }
}
