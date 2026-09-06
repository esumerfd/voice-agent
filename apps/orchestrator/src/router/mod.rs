//! Router core (ROUT-01, ROUT-04's classification half): resolves an
//! utterance to the nearest-intent workflow via cosine similarity against a
//! reload-aware embedding cache, then (Task 3) fail-closed
//! confirmation-tier classification. Mirrors `dispatch()`'s own multi-step
//! pipeline shape -- every failure maps into a typed error or a named
//! refusal, never a panic.
//!
//! `Router::route` reads candidates through `Registry::enumerate()` +
//! `Registry::lookup()` only -- it never re-implements the loader and never
//! reaches into `Registry`'s private map.
//!
//! Utterance length (Task 2) is measured in Unicode code points
//! (`chars().count()`), never a raw byte length, and no normalization form
//! is ever applied. Cache invalidation (`EmbeddingCache::sync`) compares
//! `intent` strings by exact bytes -- so two Unicode-normalization variants
//! of the same visible text are two different strings and re-embed, which
//! is the safe direction (never silently treating a changed intent as
//! unchanged).

pub mod cache;
pub mod confirm_tier;
pub mod ollama_client;
pub mod similarity;
pub mod threshold;

use std::sync::Arc;

use cache::EmbeddingCache;
use confirm_tier::ConfirmTier;
use ollama_client::{embed_with_retry, OllamaApi};
use similarity::cosine_similarity;

use crate::error::RouterError;
use crate::registry::Registry;

/// The in-process result of one `route` call. Distinct from the wire
/// `ProtocolFrame::RouteResult` plan 09-03 introduces -- this struct is the
/// internal shape; the wire frame is built from it.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteOutcome {
    pub utterance: String,
    pub matched_workflow_id: Option<String>,
    pub similarity_score: Option<f32>,
    /// Fail-closed classification (Task 3, ROUT-04): `None` only when
    /// `matched_workflow_id` is also `None` (nothing matched, so nothing to
    /// classify). Populated BEFORE the threshold decision, so a
    /// `ConfirmRequired` winner is checked against the stricter
    /// `threshold::CONFIRM_REQUIRED_MATCH_THRESHOLD`, not the base one.
    pub confirm_tier: Option<ConfirmTier>,
    /// Left `None` in Task 1 and Task 2 -- plan 09-04 fills this in.
    pub extracted_params: Option<serde_json::Value>,
    /// A human-readable refusal reason. Never names the runner-up
    /// workflow id -- a refusal must never be mistaken for a weak
    /// suggestion.
    pub detail: Option<String>,
}

/// Owns the Ollama client seam and the reload-aware embedding cache for the
/// daemon's whole lifetime (wired into `InProcessOrchestrator` in plan
/// 09-03). `Router::route` receives a borrowed, freshly-loaded `&Registry`
/// per call and syncs the cache against it -- see the module doc comment
/// and 09-01-PLAN.md's Architecture Deviation note for why the cache lives
/// here and not on `Registry`.
pub struct Router {
    client: Arc<dyn OllamaApi>,
    /// `tokio::sync::Mutex`, not `std::sync::Mutex`: the embed-and-sync
    /// await is held across the guard so two concurrent `route` calls can
    /// never each launch a duplicate corpus rebuild. The rest of this
    /// crate uses `std::sync::Mutex` where no `.await` is ever held across
    /// the lock (e.g. `ActivityRegistry`) -- this is the one exception, and
    /// the reason lives here rather than at each call site.
    cache: tokio::sync::Mutex<EmbeddingCache>,
    embed_model: String,
    /// Test-only observability hook: the `SyncStats` computed by this
    /// router's most recent cache sync, so a test can assert D-06's
    /// contract (recomputed/reused/evicted counts) through a real `route`
    /// call rather than only against `EmbeddingCache::sync` directly.
    #[cfg(test)]
    last_sync: tokio::sync::Mutex<cache::SyncStats>,
}

/// Whether a candidate's similarity score clears the threshold it must
/// meet -- inclusive: a score exactly equal to `required` is a match, one
/// representable `f32` step below is not (ROUT-02/ROUT-04 boundary probe).
/// Extracted as its own pure function so the inclusive-boundary contract is
/// directly, bit-exactly testable without depending on `cosine_similarity`'s
/// floating-point rounding to land on an exact score by construction.
fn clears_threshold(score: f32, required: f32) -> bool {
    score >= required
}

impl Router {
    pub fn new(client: Arc<dyn OllamaApi>, embed_model: impl Into<String>) -> Self {
        Self {
            client,
            cache: tokio::sync::Mutex::new(EmbeddingCache::new()),
            embed_model: embed_model.into(),
            #[cfg(test)]
            last_sync: tokio::sync::Mutex::new(cache::SyncStats::default()),
        }
    }

    #[cfg(test)]
    async fn last_sync_stats(&self) -> cache::SyncStats {
        *self.last_sync.lock().await
    }

    /// Resolves `utterance` to the nearest-intent workflow in `registry`
    /// via cosine similarity, syncing the embedding cache against the
    /// registry's current `(id, intent)` pairs first (D-06).
    ///
    /// A bounded, allocation-free guard runs BEFORE the cache sync and
    /// before any client call: an empty (or whitespace-only, after
    /// trimming) utterance, or one over `threshold::MAX_UTTERANCE_CHARS`
    /// Unicode code points, is REJECTED rather than truncated (mirrors
    /// `server/mod.rs`'s `MAX_CLIENT_NAME_LEN` discipline) -- the mock's
    /// embed-call counter is provably 0 for either case. Length is always
    /// measured in code points (`chars().count()`), never a raw byte
    /// length, and no Unicode normalization form is ever applied anywhere
    /// in this pipeline -- `EmbeddingCache::sync` compares `intent` strings
    /// byte-for-byte, so two normalization variants of the same visible
    /// text are two different strings and re-embed, which is the safe
    /// direction.
    pub async fn route(&self, utterance: &str, registry: &Registry) -> Result<RouteOutcome, RouterError> {
        let trimmed = utterance.trim();
        if trimmed.is_empty() {
            return Ok(RouteOutcome {
                utterance: utterance.to_string(),
                matched_workflow_id: None,
                similarity_score: None,
                confirm_tier: None,
                extracted_params: None,
                detail: Some("utterance is empty (or whitespace-only) after trimming".to_string()),
            });
        }
        let char_count = trimmed.chars().count();
        if char_count > threshold::MAX_UTTERANCE_CHARS {
            return Ok(RouteOutcome {
                utterance: utterance.to_string(),
                matched_workflow_id: None,
                similarity_score: None,
                confirm_tier: None,
                extracted_params: None,
                detail: Some(format!(
                    "utterance is {char_count} Unicode code points, exceeding the \
                     {}-code-point limit",
                    threshold::MAX_UTTERANCE_CHARS
                )),
            });
        }

        let utterance = utterance.to_string();

        // Candidates: every workflow whose trimmed `intent` is non-empty,
        // in `Registry::enumerate()`'s existing id-sorted order. An
        // empty-intent workflow is never a routing candidate and is never
        // embedded (ROUT-01 empty probe).
        let candidates: Vec<(String, String)> = registry
            .enumerate()
            .into_iter()
            .filter_map(|summary| {
                registry.lookup(&summary.id).and_then(|def| {
                    let intent = def.intent.trim();
                    if intent.is_empty() {
                        None
                    } else {
                        Some((summary.id, intent.to_string()))
                    }
                })
            })
            .collect();

        {
            let mut cache = self.cache.lock().await;
            #[cfg(test)]
            {
                let stats = cache.sync(&candidates, self.client.as_ref(), &self.embed_model).await?;
                *self.last_sync.lock().await = stats;
            }
            #[cfg(not(test))]
            {
                cache.sync(&candidates, self.client.as_ref(), &self.embed_model).await?;
            }
        }

        let utterance_embeddings =
            embed_with_retry(self.client.as_ref(), &self.embed_model, std::slice::from_ref(&utterance)).await?;
        let utterance_embedding = utterance_embeddings.into_iter().next().ok_or_else(|| {
            RouterError::MalformedResponse {
                endpoint: format!("{}: /api/embed", self.embed_model),
                detail: "embed call for the utterance returned zero vectors".to_string(),
            }
        })?;

        // Scan in `candidates`' existing id-sorted order; a strict
        // greater-than comparison means the FIRST (lowest-id) candidate
        // wins an exact tie, since a later equal score never beats it.
        let cache = self.cache.lock().await;
        let mut best: Option<(String, f32)> = None;
        for (id, _intent) in &candidates {
            let Some(candidate_embedding) = cache.lookup(id) else {
                continue;
            };
            let Some(score) = cosine_similarity(&utterance_embedding, candidate_embedding) else {
                continue;
            };
            let is_better = match &best {
                Some((_, best_score)) => score > *best_score,
                None => true,
            };
            if is_better {
                best = Some((id.clone(), score));
            }
        }
        drop(cache);

        // Threshold applied at the end of the scan, never mid-loop, and
        // classification happens BEFORE the threshold decision so the
        // correct bar is applied: a `ConfirmRequired` winner must clear the
        // stricter `CONFIRM_REQUIRED_MATCH_THRESHOLD`; every other winner
        // clears the base `MATCH_THRESHOLD`. Both bars are inclusive at the
        // boundary (an exact match matches, one representable f32 step
        // below refuses). Below the bar, the refusal names the best
        // observed score and the threshold it failed -- never the
        // runner-up id, so no caller can mistake a refusal for a weak
        // suggestion.
        Ok(match best {
            Some((id, score)) => {
                // `id` was produced by `registry.lookup` earlier in THIS
                // same call, against this SAME immutable `&Registry` --
                // guaranteed present, never a network- or caller-derived
                // value, so `.expect()` here documents an internal
                // invariant rather than tolerating an external failure.
                let def = registry
                    .lookup(&id)
                    .expect("a candidate id produced by this same route() call must still resolve");
                let tier = confirm_tier::confirm_tier(def);
                let required_threshold = match tier {
                    ConfirmTier::ConfirmRequired => threshold::CONFIRM_REQUIRED_MATCH_THRESHOLD,
                    ConfirmTier::RouteFreely => threshold::MATCH_THRESHOLD,
                };

                if clears_threshold(score, required_threshold) {
                    RouteOutcome {
                        utterance,
                        matched_workflow_id: Some(id),
                        similarity_score: Some(score),
                        confirm_tier: Some(tier),
                        extracted_params: None,
                        detail: None,
                    }
                } else {
                    RouteOutcome {
                        utterance,
                        matched_workflow_id: None,
                        similarity_score: Some(score),
                        confirm_tier: None,
                        extracted_params: None,
                        detail: Some(format!(
                            "best observed similarity {score} did not clear the {required_threshold} match threshold"
                        )),
                    }
                }
            }
            None => RouteOutcome {
                utterance,
                matched_workflow_id: None,
                similarity_score: None,
                confirm_tier: None,
                extracted_params: None,
                detail: Some("no candidate workflow matched".to_string()),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Canned-vector `OllamaApi` test double -- the ONLY concrete
    /// `OllamaApi` this module's tests ever construct, exactly mirroring
    /// `MockAgentRuntime`'s shape (canned outcome + call counter + optional
    /// forced error/behavior). Every `embed()` call is a SINGLE attempt --
    /// `embed_with_retry` (the production call site) supplies the retry.
    struct MockOllama {
        vectors: HashMap<String, Vec<f32>>,
        call_count: AtomicUsize,
        behavior: MockBehavior,
    }

    enum MockBehavior {
        Normal,
        Unreachable,
        /// The FIRST call times out; every call after that succeeds from
        /// `vectors` -- proves the one-retry path succeeds when the retry
        /// itself is clean.
        TimeoutThenSucceed,
        /// Every call times out -- proves the one-retry CAP: exactly two
        /// attempts, then `Timeout`, never a third.
        AlwaysTimeout,
    }

    impl MockOllama {
        fn new(vectors: HashMap<String, Vec<f32>>) -> Self {
            Self { vectors, call_count: AtomicUsize::new(0), behavior: MockBehavior::Normal }
        }

        fn with_behavior(vectors: HashMap<String, Vec<f32>>, behavior: MockBehavior) -> Self {
            Self { vectors, call_count: AtomicUsize::new(0), behavior }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl OllamaApi for MockOllama {
        async fn embed(&self, _model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError> {
            let attempt = self.call_count.fetch_add(1, Ordering::SeqCst) + 1;
            match &self.behavior {
                MockBehavior::Unreachable => Err(RouterError::OllamaUnreachable {
                    base_url: "mock://ollama".to_string(),
                    detail: "connection refused".to_string(),
                }),
                MockBehavior::AlwaysTimeout => Err(RouterError::Timeout {
                    endpoint: "mock://ollama/api/embed".to_string(),
                    seconds: 10,
                }),
                MockBehavior::TimeoutThenSucceed if attempt == 1 => Err(RouterError::Timeout {
                    endpoint: "mock://ollama/api/embed".to_string(),
                    seconds: 10,
                }),
                _ => Ok(inputs.iter().map(|i| self.vectors.get(i).cloned().unwrap_or_default()).collect()),
            }
        }
    }

    /// Builds a real `Registry` via `Registry::load` against a fixture
    /// directory of minimal `.md` workflow files -- the plan's Architecture
    /// Deviation note forbids any change to `registry/mod.rs`/`loader.rs`,
    /// so this module cannot construct a `Registry` via a private-field
    /// literal (that convenience is scoped to `registry/mod.rs`'s own
    /// `#[cfg(test)]` block, e.g. `stub_definition`) -- going through the
    /// real, already-public `Registry::load` keeps this test file's diff
    /// entirely outside `registry/`.
    fn registry_from(pairs: &[(&str, &str)]) -> Registry {
        let dir = tempfile::TempDir::new().expect("failed to create fixture tempdir");
        for (id, intent) in pairs {
            let path = dir.path().join(format!("{id}.md"));
            let contents = format!(
                "---\nid: {id}\nname: {id}\nparameters: {{}}\nservice:\n  type: action\n  handler: demo.handler\nintent: {intent:?}\n---\nStub workflow for router tests.\n"
            );
            std::fs::write(&path, contents).expect("failed to write workflow fixture");
        }
        let (registry, errors) = Registry::load(dir.path());
        assert!(errors.is_empty(), "expected zero load errors from the fixture directory, got: {errors:?}");
        registry
    }

    fn calendar_intent() -> &'static str {
        "check today's calendar events"
    }
    fn timer_intent() -> &'static str {
        "set a countdown timer"
    }

    fn two_workflow_vectors() -> HashMap<String, Vec<f32>> {
        let mut v = HashMap::new();
        v.insert(calendar_intent().to_string(), vec![1.0, 0.0, 0.0]);
        v.insert(timer_intent().to_string(), vec![0.0, 1.0, 0.0]);
        v
    }

    #[tokio::test]
    async fn routes_to_the_nearest_intent_workflow() {
        let mut vectors = two_workflow_vectors();
        vectors.insert("check my calendar".to_string(), vec![0.9, 0.1, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text");
        let registry = registry_from(&[
            ("calendar_today", calendar_intent()),
            ("set_timer", timer_intent()),
        ]);

        let outcome =
            router.route("check my calendar", &registry).await.expect("route should succeed");

        assert_eq!(outcome.matched_workflow_id, Some("calendar_today".to_string()));
        assert!(outcome.similarity_score.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn second_route_call_issues_exactly_one_additional_embed_call() {
        let mut vectors = two_workflow_vectors();
        vectors.insert("check my calendar".to_string(), vec![0.9, 0.1, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client.clone(), "nomic-embed-text");
        let registry = registry_from(&[
            ("calendar_today", calendar_intent()),
            ("set_timer", timer_intent()),
        ]);

        router.route("check my calendar", &registry).await.expect("first route");
        let calls_after_first = client.call_count();

        router.route("check my calendar", &registry).await.expect("second route");
        let calls_after_second = client.call_count();

        assert_eq!(
            calls_after_second - calls_after_first,
            1,
            "expected exactly ONE additional embed call (the utterance) on the second route \
             against an unchanged registry, not a re-embedded corpus"
        );
        assert_eq!(router.last_sync_stats().await.recomputed, 0);
    }

    #[tokio::test]
    async fn a_changed_intent_re_embeds_exactly_that_one_workflow_on_the_next_route() {
        let mut vectors = two_workflow_vectors();
        vectors.insert("changed intent text".to_string(), vec![0.0, 0.0, 1.0]);
        vectors.insert("check my calendar".to_string(), vec![0.9, 0.1, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text");

        let registry = registry_from(&[
            ("calendar_today", calendar_intent()),
            ("set_timer", timer_intent()),
        ]);
        router.route("check my calendar", &registry).await.expect("first route");

        let registry_changed = registry_from(&[
            ("calendar_today", "changed intent text"),
            ("set_timer", timer_intent()),
        ]);
        router.route("check my calendar", &registry_changed).await.expect("second route");

        assert_eq!(router.last_sync_stats().await.recomputed, 1);
    }

    #[tokio::test]
    async fn a_removed_workflow_id_is_evicted_and_can_no_longer_be_matched() {
        let mut vectors = two_workflow_vectors();
        vectors.insert("set a timer please".to_string(), vec![0.1, 0.9, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text");

        let registry = registry_from(&[
            ("calendar_today", calendar_intent()),
            ("set_timer", timer_intent()),
        ]);
        router.route("set a timer please", &registry).await.expect("first route");

        let registry_without_timer = registry_from(&[("calendar_today", calendar_intent())]);
        let outcome = router
            .route("set a timer please", &registry_without_timer)
            .await
            .expect("second route");

        assert_eq!(router.last_sync_stats().await.evicted, 1);
        assert_ne!(
            outcome.matched_workflow_id,
            Some("set_timer".to_string()),
            "the evicted workflow must never be matched again"
        );
    }

    #[tokio::test]
    async fn an_empty_intent_workflow_is_never_embedded_and_never_matched() {
        let mut vectors = HashMap::new();
        vectors.insert(calendar_intent().to_string(), vec![1.0, 0.0]);
        vectors.insert("something".to_string(), vec![0.9, 0.1]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text");

        let registry = registry_from(&[("calendar_today", calendar_intent()), ("empty_intent_wf", "")]);

        let outcome = router.route("something", &registry).await.expect("route should succeed");

        assert_ne!(outcome.matched_workflow_id, Some("empty_intent_wf".to_string()));
        assert_eq!(router.last_sync_stats().await.recomputed, 1, "only the non-empty-intent workflow embeds");
    }

    #[tokio::test]
    async fn a_connection_error_surfaces_as_ollama_unreachable_and_never_panics() {
        let client = Arc::new(MockOllama::with_behavior(HashMap::new(), MockBehavior::Unreachable));
        let router = Router::new(client.clone(), "nomic-embed-text");
        // No candidates -- isolates the assertion to the utterance embed call.
        let registry = registry_from(&[]);

        let result = router.route("anything", &registry).await;

        assert_eq!(
            result,
            Err(RouterError::OllamaUnreachable {
                base_url: "mock://ollama".to_string(),
                detail: "connection refused".to_string()
            })
        );
        assert_eq!(client.call_count(), 1, "a connection refusal must never be retried");
    }

    #[tokio::test]
    async fn a_timeout_once_then_success_retries_exactly_once_and_the_route_succeeds() {
        let mut vectors = HashMap::new();
        vectors.insert("anything".to_string(), vec![1.0, 0.0]);
        let client = Arc::new(MockOllama::with_behavior(vectors, MockBehavior::TimeoutThenSucceed));
        let router = Router::new(client.clone(), "nomic-embed-text");
        let registry = registry_from(&[]);

        let result = router.route("anything", &registry).await;

        assert!(result.is_ok(), "expected a retry that succeeds on the second attempt to succeed");
        assert_eq!(client.call_count(), 2, "expected exactly one retry (two total calls)");
    }

    #[tokio::test]
    async fn a_timeout_on_both_attempts_fails_after_exactly_two_attempts_never_a_third() {
        let client = Arc::new(MockOllama::with_behavior(HashMap::new(), MockBehavior::AlwaysTimeout));
        let router = Router::new(client.clone(), "nomic-embed-text");
        let registry = registry_from(&[]);

        let result = router.route("anything", &registry).await;

        assert_eq!(
            result,
            Err(RouterError::Timeout { endpoint: "mock://ollama/api/embed".to_string(), seconds: 10 })
        );
        assert_eq!(client.call_count(), 2, "expected exactly two attempts, never a third");
    }

    // -----------------------------------------------------------------
    // Task 2: threshold, refusal, and the edge contract
    // -----------------------------------------------------------------

    #[test]
    fn a_score_exactly_equal_to_the_match_threshold_clears_it() {
        assert!(clears_threshold(threshold::MATCH_THRESHOLD, threshold::MATCH_THRESHOLD));
    }

    #[test]
    fn a_score_one_f32_step_below_the_match_threshold_does_not_clear_it() {
        let just_below = f32::from_bits(threshold::MATCH_THRESHOLD.to_bits() - 1);
        assert!(!clears_threshold(just_below, threshold::MATCH_THRESHOLD));
    }

    #[test]
    fn a_score_exactly_equal_to_the_confirm_required_threshold_clears_it() {
        assert!(clears_threshold(
            threshold::CONFIRM_REQUIRED_MATCH_THRESHOLD,
            threshold::CONFIRM_REQUIRED_MATCH_THRESHOLD
        ));
    }

    #[test]
    fn a_score_one_f32_step_below_the_confirm_required_threshold_does_not_clear_it() {
        let just_below = f32::from_bits(threshold::CONFIRM_REQUIRED_MATCH_THRESHOLD.to_bits() - 1);
        assert!(!clears_threshold(just_below, threshold::CONFIRM_REQUIRED_MATCH_THRESHOLD));
    }

    #[tokio::test]
    async fn no_candidate_clearing_the_threshold_refuses_and_never_names_the_runner_up() {
        // A 3D utterance vector orthogonal to BOTH candidate vectors --
        // cosine similarity is exactly 0.0 against each, well below
        // MATCH_THRESHOLD, and still a finite/comparable score (never
        // `None`), so this exercises the threshold-refusal branch, not the
        // dimension-mismatch branch.
        let mut vectors = HashMap::new();
        vectors.insert("calendar intent".to_string(), vec![1.0, 0.0, 0.0]);
        vectors.insert("timer intent".to_string(), vec![0.0, 1.0, 0.0]);
        vectors.insert("orthogonal utterance".to_string(), vec![0.0, 0.0, 1.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text");
        let registry = registry_from(&[("aaa_wf", "calendar intent"), ("bbb_wf", "timer intent")]);

        let outcome = router
            .route("orthogonal utterance", &registry)
            .await
            .expect("route should succeed even when nothing clears the threshold");

        assert_eq!(outcome.matched_workflow_id, None, "expected a refusal, never the runner-up id");
        assert!(
            outcome.detail.as_deref().is_some_and(|d| !d.contains("aaa_wf") && !d.contains("bbb_wf")),
            "expected the refusal detail to name a score/threshold, never a candidate id, got: {:?}",
            outcome.detail
        );
    }

    #[tokio::test]
    async fn an_empty_or_whitespace_only_utterance_refuses_before_any_ollama_call() {
        let client = Arc::new(MockOllama::new(two_workflow_vectors()));
        let router = Router::new(client.clone(), "nomic-embed-text");
        let registry =
            registry_from(&[("calendar_today", calendar_intent()), ("set_timer", timer_intent())]);

        let outcome = router.route("   ", &registry).await.expect("route should succeed (a refusal)");

        assert_eq!(outcome.matched_workflow_id, None);
        assert_eq!(client.call_count(), 0, "an empty utterance must never trigger an Ollama call");
    }

    #[tokio::test]
    async fn an_utterance_over_the_max_char_limit_refuses_before_any_ollama_call() {
        let client = Arc::new(MockOllama::new(HashMap::new()));
        let router = Router::new(client.clone(), "nomic-embed-text");
        let registry = registry_from(&[]);
        let too_long: String = "a".repeat(threshold::MAX_UTTERANCE_CHARS + 1);

        let outcome = router.route(&too_long, &registry).await.expect("route should succeed (a refusal)");

        assert_eq!(outcome.matched_workflow_id, None);
        assert_eq!(client.call_count(), 0, "an over-limit utterance must never trigger an Ollama call");
    }

    #[tokio::test]
    async fn an_utterance_of_exactly_the_max_char_limit_is_accepted() {
        let exactly_at_limit = "a".repeat(threshold::MAX_UTTERANCE_CHARS);
        let mut vectors = HashMap::new();
        vectors.insert(exactly_at_limit.clone(), vec![1.0, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client.clone(), "nomic-embed-text");
        let registry = registry_from(&[]);

        router.route(&exactly_at_limit, &registry).await.expect("route should succeed");

        assert_eq!(
            client.call_count(),
            1,
            "an utterance of exactly MAX_UTTERANCE_CHARS must be accepted, not refused"
        );
    }

    #[tokio::test]
    async fn a_dimension_mismatched_candidate_is_skipped_and_the_sole_candidate_case_refuses_not_errors() {
        // The candidate's cached embedding is 3-dimensional; the utterance
        // embedding the mock returns is 2-dimensional -- cosine_similarity
        // must return None for this pair, never a panic or a comparison.
        let mut vectors = HashMap::new();
        vectors.insert("three dim intent".to_string(), vec![1.0, 0.0, 0.0]);
        vectors.insert("two dim utterance".to_string(), vec![1.0, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text");
        let registry = registry_from(&[("only_wf", "three dim intent")]);

        let outcome = router
            .route("two dim utterance", &registry)
            .await
            .expect("a dimension mismatch must refuse, never error");

        assert_eq!(outcome.matched_workflow_id, None);
    }

    #[tokio::test]
    async fn byte_identical_intents_score_identically_and_the_lexicographically_smaller_id_wins() {
        let mut vectors = HashMap::new();
        vectors.insert("identical intent text".to_string(), vec![1.0, 0.0]);
        vectors.insert("identical intent text utterance".to_string(), vec![1.0, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text");
        // "aaa_wf" sorts before "bbb_wf" in Registry::enumerate()'s id order.
        let registry = registry_from(&[
            ("bbb_wf", "identical intent text"),
            ("aaa_wf", "identical intent text"),
        ]);

        let outcome = router
            .route("identical intent text utterance", &registry)
            .await
            .expect("route should succeed");

        assert_eq!(
            outcome.matched_workflow_id,
            Some("aaa_wf".to_string()),
            "expected the lexicographically smaller id to win an exact score tie"
        );
    }

    #[tokio::test]
    async fn utterance_length_is_bounded_by_code_points_not_bytes() {
        // Each "🎉" is 4 UTF-8 bytes but ONE Unicode scalar value -- 300 of
        // them is 1200 bytes (over MAX_UTTERANCE_CHARS if measured in
        // bytes) but only 300 code points (well under the limit).
        let multi_byte_utterance: String = "🎉".repeat(300);
        assert!(multi_byte_utterance.len() > threshold::MAX_UTTERANCE_CHARS);
        assert!(multi_byte_utterance.chars().count() < threshold::MAX_UTTERANCE_CHARS);

        let mut vectors = HashMap::new();
        vectors.insert(multi_byte_utterance.clone(), vec![1.0, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client.clone(), "nomic-embed-text");
        let registry = registry_from(&[]);

        router.route(&multi_byte_utterance, &registry).await.expect("route should succeed");

        assert_eq!(
            client.call_count(),
            1,
            "expected a byte-long-but-code-point-short utterance to be accepted, not refused"
        );
    }
}
