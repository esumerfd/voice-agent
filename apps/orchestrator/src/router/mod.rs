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
pub mod collision;
pub mod confirm_tier;
pub mod ollama_client;
pub mod schema;
pub mod similarity;
pub mod threshold;

use std::sync::Arc;

use cache::EmbeddingCache;
use collision::IntentCollision;
use confirm_tier::ConfirmTier;
use ollama_client::{embed_with_retry, OllamaApi};
use similarity::cosine_similarity;

use crate::definition::WorkflowDefinition;
use crate::error::RouterError;
use crate::registry::Registry;

/// Every Ollama request (embed AND generate) carries this `keep_alive`
/// value (plan 09-04 Task 3, COVERAGE.md row 3): Ollama unloads an idle
/// model after roughly five minutes by default, so a startup warm-up alone
/// only removes the cold-load cost for the FIRST request after a daemon
/// start -- the measured multi-second penalty silently returns after every
/// quiet period, and a voice assistant is exactly that bursty (long gaps
/// between utterances, not a steady request stream). `"30m"` sits in the
/// tens-of-minutes range RESEARCH recommends: long enough to survive a
/// normal between-utterances gap without re-paying the cold-load tax, short
/// enough that an abandoned session eventually frees the RAM both
/// `nomic-embed-text` and the extraction model hold resident on this
/// CPU-only, single-user machine -- a real trade-off on a machine with no
/// other workload to reclaim that memory from. Declared here (not in
/// `router::threshold`, which plan 09-05 owns) to preserve the two plans'
/// file disjointness within this wave.
pub const OLLAMA_KEEP_ALIVE: &str = "30m";

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
    /// `Some` on a successful extraction (including the zero-parameter
    /// short-circuit, which reports an empty object) -- `None` when nothing
    /// matched OR when extraction failed for any reason (plan 09-04,
    /// ROUT-03). A failed extraction never invalidates a successful match:
    /// `matched_workflow_id`/`similarity_score`/`confirm_tier` are set
    /// identically whether or not extraction succeeded.
    pub extracted_params: Option<serde_json::Value>,
    /// A human-readable refusal/degradation reason. Never names the
    /// runner-up workflow id on a routing refusal -- a refusal must never
    /// be mistaken for a weak suggestion. May be `Some` alongside a
    /// successful match when extraction itself degraded (plan 09-04) --
    /// that is not a routing refusal, only an extraction one.
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
    /// The Ollama instruct model `extract_params` calls (plan 09-04,
    /// ROUT-03) -- deliberately a SEPARATE model/field from `embed_model`:
    /// embedding and structured generation are different Ollama endpoints
    /// with different model requirements.
    extract_model: String,
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
    pub fn new(
        client: Arc<dyn OllamaApi>,
        embed_model: impl Into<String>,
        extract_model: impl Into<String>,
    ) -> Self {
        Self {
            client,
            cache: tokio::sync::Mutex::new(EmbeddingCache::new()),
            embed_model: embed_model.into(),
            extract_model: extract_model.into(),
            #[cfg(test)]
            last_sync: tokio::sync::Mutex::new(cache::SyncStats::default()),
        }
    }

    #[cfg(test)]
    async fn last_sync_stats(&self) -> cache::SyncStats {
        *self.last_sync.lock().await
    }

    /// Fire-and-forget model warm-up (plan 09-04 Task 3, RESEARCH Pitfall
    /// B): one throwaway `embed` call and one throwaway `generate_json`
    /// call, purely to force Ollama to load both models into memory before
    /// the first REAL utterance has to pay the measured multi-second
    /// cold-load cost. Both results are discarded -- only success/failure
    /// is logged, one line per model. NEVER returns an error and NEVER
    /// panics: the caller (`orchestratord::main`) is responsible for
    /// spawning this as an independent task AFTER the listener binds and
    /// BEFORE `serve` is awaited, so a slow or unreachable Ollama can
    /// neither delay the daemon's readiness nor fail startup.
    pub async fn warm_up(&self) {
        let trivial_input = vec!["warm up".to_string()];
        match self.client.embed(&self.embed_model, &trivial_input).await {
            Ok(_) => eprintln!(
                "orchestratord: router warm-up succeeded for embed model {:?}",
                self.embed_model
            ),
            Err(e) => eprintln!(
                "orchestratord: router warm-up FAILED for embed model {:?}: {e} (the first real \
                 request will pay the cold-load cost instead)",
                self.embed_model
            ),
        }

        let trivial_schema = serde_json::json!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"],
            "additionalProperties": false,
        });
        match self
            .client
            .generate_json(&self.extract_model, "Respond with {\"ok\": true} and nothing else.", "warm up", &trivial_schema)
            .await
        {
            Ok(_) => eprintln!(
                "orchestratord: router warm-up succeeded for extract model {:?}",
                self.extract_model
            ),
            Err(e) => eprintln!(
                "orchestratord: router warm-up FAILED for extract model {:?}: {e} (the first real \
                 request will pay the cold-load cost instead)",
                self.extract_model
            ),
        }
    }

    /// Fills `def`'s declared parameters from `utterance` via the instruct
    /// model (ROUT-03, plan 09-04). The schema is built from `def.parameters`
    /// ALONE -- never the registry, never any other workflow's parameters or
    /// intent (T-09-01/T-09-14): the similarity match has already selected
    /// `def`, independently of this call, so an instruction hidden inside
    /// `utterance` can at worst distort the VALUES extracted for `def`; it
    /// can never retarget which workflow's schema is even built. The fixed
    /// instruction text travels in the `system` field and `utterance` alone
    /// travels in `prompt` (as a JSON-encoded string value) -- structurally
    /// separate request fields, a stronger injection boundary than
    /// delimiting the two inside one concatenated string.
    ///
    /// A workflow with zero declared parameters short-circuits here without
    /// ever touching `self.client` -- two of the four shipped workflows
    /// declare no parameters, so this is the common case, and an
    /// instruct-model call for it would cost roughly a second of latency to
    /// produce a guaranteed empty object.
    ///
    /// NEVER returns an error to the caller: every extraction-specific
    /// failure (an unreachable/erroring Ollama call, a non-object result, or
    /// a `validate_payload` failure against `def.parameters`) degrades to
    /// `(None, Some(detail))` rather than propagating -- a failed extraction
    /// must never invalidate the match `route()` already decided (ROUT-04
    /// boundary). `detail` is always built from `RouterError::
    /// ExtractionRejected`'s `Display` impl, so every failure path names the
    /// offending workflow id in one consistently-worded string.
    ///
    /// `pub` (plan 09-04 Task 3): `tests/router_extraction.rs`'s opt-in
    /// model bake-off calls this directly against each installed candidate
    /// model, isolating the measurement to the ONE thing that varies across
    /// candidates (structured generation) without also depending on
    /// embedding-based similarity matching to succeed first.
    pub async fn extract_params(
        &self,
        def: &WorkflowDefinition,
        utterance: &str,
    ) -> (Option<serde_json::Value>, Option<String>) {
        if def.parameters.is_empty() {
            return (Some(serde_json::Value::Object(serde_json::Map::new())), None);
        }

        let param_schema = schema::build_param_schema(&def.parameters);
        let system = format!(
            "You extract structured parameters for the \"{}\" workflow from a user's \
             utterance. Respond with ONLY a JSON object matching the given schema -- never \
             invent a parameter the schema does not declare, and never respond on behalf of \
             any other workflow.",
            def.id
        );
        // The untrusted utterance travels as a JSON-encoded STRING value in
        // `prompt`, never as free instruction text -- the schema (not the
        // prose) is what actually constrains the model's output.
        let prompt = format!("Utterance: {}", serde_json::Value::String(utterance.to_string()));

        let raw = match self.client.generate_json(&self.extract_model, &system, &prompt, &param_schema).await {
            Ok(value) => value,
            Err(e) => {
                let rejected =
                    RouterError::ExtractionRejected { workflow_id: def.id.clone(), detail: e.to_string() };
                return (None, Some(rejected.to_string()));
            }
        };

        let Some(object) = raw.as_object() else {
            let rejected = RouterError::ExtractionRejected {
                workflow_id: def.id.clone(),
                detail: "the model's response parsed to a non-object JSON value".to_string(),
            };
            return (None, Some(rejected.to_string()));
        };

        // The router extracting a value does not exempt it from the
        // workflow's declared type checks: the SAME unconditional gate
        // `dispatch()` applies on the real invocation path (V5 input
        // validation) -- never reported as if it had passed.
        match crate::dispatch::validate_payload(object, &def.parameters) {
            Ok(()) => (Some(raw), None),
            Err(e) => {
                let rejected = RouterError::ExtractionRejected {
                    workflow_id: def.id.clone(),
                    detail: format!("validation failed: {e}"),
                };
                (None, Some(rejected.to_string()))
            }
        }
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
                    // Extraction runs ONLY after the match and the
                    // confirmation tier are already decided (plan 09-04) --
                    // whatever `extract_params` returns can never change
                    // `matched_workflow_id`/`similarity_score`/`confirm_tier`
                    // below, only `extracted_params`/`detail`.
                    let (extracted_params, extraction_detail) = self.extract_params(def, &utterance).await;
                    RouteOutcome {
                        utterance,
                        matched_workflow_id: Some(id),
                        similarity_score: Some(score),
                        confirm_tier: Some(tier),
                        extracted_params,
                        detail: extraction_detail,
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

    /// Save-time semantic-collision check (Phase 10 plan 10-02, D-03/D-04):
    /// resolves whether `intent` -- a NEW workflow's not-yet-saved intent
    /// phrase -- collides with any EXISTING workflow's intent in `registry`.
    /// Read-only classification, never a routing turn: this never calls
    /// `extract_params`, `confirm_tier`, `dispatch`, or anything that mints a
    /// run id.
    ///
    /// Reuses `route`'s exact bounded prologue: an empty (or whitespace-only)
    /// intent, or one over `threshold::MAX_UTTERANCE_CHARS` Unicode code
    /// points, is rejected as `Ok(None)` before any cache sync or client
    /// call -- the embed-call counter is provably 0 for either case (T-10-07).
    /// Candidates are gathered exactly like `route`'s own candidate
    /// collection: every workflow in `registry.enumerate()` whose trimmed
    /// `intent` is non-empty. The SAME embedding cache `route` uses is
    /// synced first (D-06's reload-aware mechanism, no second cache), then
    /// exactly one `embed_with_retry` call embeds the probe intent. A
    /// `RouterError` from either the cache sync or the probe embed
    /// PROPAGATES as `Err` -- never swallowed into `Ok(None)` -- so the
    /// caller (`InProcessOrchestrator::check_intent_collision`) can
    /// distinguish "no collision" from "could not check" and degrade the
    /// latter into a non-blocking warning, per D-04.
    pub async fn check_intent_collision(
        &self,
        intent: &str,
        registry: &Registry,
    ) -> Result<Option<IntentCollision>, RouterError> {
        let trimmed = intent.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let char_count = trimmed.chars().count();
        if char_count > threshold::MAX_UTTERANCE_CHARS {
            return Ok(None);
        }

        let probe_intent = intent.to_string();

        // Candidates: every workflow whose trimmed `intent` is non-empty, in
        // `Registry::enumerate()`'s existing id-sorted order -- identical
        // collection discipline to `route`'s own candidate gathering.
        let candidates: Vec<(String, String)> = registry
            .enumerate()
            .into_iter()
            .filter_map(|summary| {
                registry.lookup(&summary.id).and_then(|def| {
                    let candidate_intent = def.intent.trim();
                    if candidate_intent.is_empty() {
                        None
                    } else {
                        Some((summary.id, candidate_intent.to_string()))
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

        let probe_embeddings =
            embed_with_retry(self.client.as_ref(), &self.embed_model, std::slice::from_ref(&probe_intent)).await?;
        let probe_embedding = probe_embeddings.into_iter().next().ok_or_else(|| {
            RouterError::MalformedResponse {
                endpoint: format!("{}: /api/embed", self.embed_model),
                detail: "embed call for the probe intent returned zero vectors".to_string(),
            }
        })?;

        let cache = self.cache.lock().await;
        let triples: Vec<(String, String, Vec<f32>)> = candidates
            .into_iter()
            .filter_map(|(id, candidate_intent)| {
                cache.lookup(&id).map(|embedding| (id, candidate_intent, embedding.to_vec()))
            })
            .collect();
        drop(cache);

        Ok(collision::find_collision(&probe_embedding, &triples, collision::COLLISION_THRESHOLD))
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
    ///
    /// Plan 09-04 extends this SAME mock with `generate_json` support
    /// (a canned response string + its own call counter + a recorded-request
    /// log) rather than introducing a second mock type -- one `OllamaApi`
    /// double across the whole module, exactly like the trait it mirrors.
    struct MockOllama {
        vectors: HashMap<String, Vec<f32>>,
        call_count: AtomicUsize,
        behavior: MockBehavior,
        /// The canned JSON-encoded string `generate_json` parses and
        /// returns. Defaults to an empty object so every pre-existing
        /// routing test (which never configures this) exercises the
        /// zero-parameter short-circuit and never actually reaches this
        /// value.
        generate_response: String,
        generate_call_count: AtomicUsize,
        /// Every `generate_json` call's `(model, system, prompt, schema)`
        /// tuple, in call order -- lets a test assert on the EXACT request
        /// shape (e.g. byte-identical repeated calls), not just the return
        /// value.
        generate_requests: std::sync::Mutex<Vec<(String, String, String, serde_json::Value)>>,
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
            Self {
                vectors,
                call_count: AtomicUsize::new(0),
                behavior: MockBehavior::Normal,
                generate_response: "{}".to_string(),
                generate_call_count: AtomicUsize::new(0),
                generate_requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_behavior(vectors: HashMap<String, Vec<f32>>, behavior: MockBehavior) -> Self {
            Self {
                vectors,
                call_count: AtomicUsize::new(0),
                behavior,
                generate_response: "{}".to_string(),
                generate_call_count: AtomicUsize::new(0),
                generate_requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// A mock configured with a canned `generate_json` response string
        /// (plan 09-04, Task 1). `vectors` still drives `embed()` exactly as
        /// every other constructor here.
        fn with_generate_response(vectors: HashMap<String, Vec<f32>>, generate_response: &str) -> Self {
            Self {
                vectors,
                call_count: AtomicUsize::new(0),
                behavior: MockBehavior::Normal,
                generate_response: generate_response.to_string(),
                generate_call_count: AtomicUsize::new(0),
                generate_requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }

        fn generate_call_count(&self) -> usize {
            self.generate_call_count.load(Ordering::SeqCst)
        }

        fn generate_requests(&self) -> Vec<(String, String, String, serde_json::Value)> {
            self.generate_requests.lock().expect("mock mutex poisoned").clone()
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

        async fn generate_json(
            &self,
            model: &str,
            system: &str,
            prompt: &str,
            schema: &serde_json::Value,
        ) -> Result<serde_json::Value, RouterError> {
            self.generate_call_count.fetch_add(1, Ordering::SeqCst);
            self.generate_requests.lock().expect("mock mutex poisoned").push((
                model.to_string(),
                system.to_string(),
                prompt.to_string(),
                schema.clone(),
            ));
            serde_json::from_str(&self.generate_response).map_err(|e| RouterError::MalformedResponse {
                endpoint: "mock://ollama/api/generate".to_string(),
                detail: e.to_string(),
            })
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
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");

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
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");

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
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");

        let registry = registry_from(&[("calendar_today", calendar_intent()), ("empty_intent_wf", "")]);

        let outcome = router.route("something", &registry).await.expect("route should succeed");

        assert_ne!(outcome.matched_workflow_id, Some("empty_intent_wf".to_string()));
        assert_eq!(router.last_sync_stats().await.recomputed, 1, "only the non-empty-intent workflow embeds");
    }

    #[tokio::test]
    async fn a_connection_error_surfaces_as_ollama_unreachable_and_never_panics() {
        let client = Arc::new(MockOllama::with_behavior(HashMap::new(), MockBehavior::Unreachable));
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
        let registry = registry_from(&[]);

        let result = router.route("anything", &registry).await;

        assert!(result.is_ok(), "expected a retry that succeeds on the second attempt to succeed");
        assert_eq!(client.call_count(), 2, "expected exactly one retry (two total calls)");
    }

    #[tokio::test]
    async fn a_timeout_on_both_attempts_fails_after_exactly_two_attempts_never_a_third() {
        let client = Arc::new(MockOllama::with_behavior(HashMap::new(), MockBehavior::AlwaysTimeout));
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
        let registry =
            registry_from(&[("calendar_today", calendar_intent()), ("set_timer", timer_intent())]);

        let outcome = router.route("   ", &registry).await.expect("route should succeed (a refusal)");

        assert_eq!(outcome.matched_workflow_id, None);
        assert_eq!(client.call_count(), 0, "an empty utterance must never trigger an Ollama call");
    }

    #[tokio::test]
    async fn an_utterance_over_the_max_char_limit_refuses_before_any_ollama_call() {
        let client = Arc::new(MockOllama::new(HashMap::new()));
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");
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
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
        let registry = registry_from(&[]);

        router.route(&multi_byte_utterance, &registry).await.expect("route should succeed");

        assert_eq!(
            client.call_count(),
            1,
            "expected a byte-long-but-code-point-short utterance to be accepted, not refused"
        );
    }

    // -----------------------------------------------------------------
    // Plan 09-04, Task 1: structured parameter extraction, happy path
    // -----------------------------------------------------------------

    /// A `WorkflowDefinition` with one required int parameter, constructed
    /// directly via struct literal (every field on `WorkflowDefinition`/
    /// `ServiceSpec` is `pub`, and this test module is inside the SAME
    /// crate) -- unlike `Registry`, `WorkflowDefinition` carries no
    /// plan-level construction restriction.
    fn timer_definition() -> WorkflowDefinition {
        let mut parameters = HashMap::new();
        parameters.insert(
            "duration_minutes".to_string(),
            crate::definition::ParameterSpec {
                type_: crate::definition::ParameterType::Int,
                description: Some("how many minutes".to_string()),
                required: true,
            },
        );
        WorkflowDefinition {
            id: "set_timer".to_string(),
            name: "Set Timer".to_string(),
            description: String::new(),
            parameters,
            service: crate::definition::ServiceSpec {
                type_: Some("action".to_string()),
                handler: "timers.start".to_string(),
                mode: crate::definition::ServiceMode::Sync,
                command: None,
                args: Vec::new(),
                agent: None,
            },
            source_path: std::path::PathBuf::from("stub.md"),
            intent: timer_intent().to_string(),
            triggers: Vec::new(),
        }
    }

    /// Same fixture-writer shape as `registry_from`, but with a real
    /// `parameters:` block -- `registry_from` hardcodes `parameters: {{}}`,
    /// which cannot express `set_timer`'s required int parameter.
    fn registry_with_timer_workflow() -> Registry {
        let dir = tempfile::TempDir::new().expect("failed to create fixture tempdir");
        let contents = format!(
            "---\nid: set_timer\nname: Set Timer\nparameters:\n  duration_minutes:\n    type: int\n    required: true\nservice:\n  type: action\n  handler: timers.start\nintent: {:?}\n---\nStub timer workflow for extraction tests.\n",
            timer_intent()
        );
        std::fs::write(dir.path().join("set_timer.md"), contents).expect("failed to write workflow fixture");
        let (registry, errors) = Registry::load(dir.path());
        assert!(errors.is_empty(), "expected zero load errors from the fixture directory, got: {errors:?}");
        registry
    }

    #[tokio::test]
    async fn a_matched_utterance_returns_typed_extracted_parameters_from_the_double_parsed_response() {
        let mut vectors = HashMap::new();
        vectors.insert(timer_intent().to_string(), vec![0.0, 1.0, 0.0]);
        vectors.insert("set a timer for ten minutes".to_string(), vec![0.0, 1.0, 0.0]);
        let mock =
            Arc::new(MockOllama::with_generate_response(vectors, r#"{"duration_minutes": 10}"#));
        let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
        let registry = registry_with_timer_workflow();

        let outcome = router
            .route("set a timer for ten minutes", &registry)
            .await
            .expect("route should succeed");

        assert_eq!(outcome.matched_workflow_id, Some("set_timer".to_string()));
        assert_eq!(outcome.confirm_tier, Some(ConfirmTier::RouteFreely));
        assert_eq!(
            outcome.extracted_params,
            Some(serde_json::json!({"duration_minutes": 10})),
            "expected the double-parsed response value, typed, on the outcome"
        );
        assert_eq!(mock.generate_call_count(), 1);
    }

    #[tokio::test]
    async fn the_schema_handed_to_the_model_names_exactly_the_matched_workflows_parameters() {
        let mut vectors = HashMap::new();
        vectors.insert(timer_intent().to_string(), vec![0.0, 1.0, 0.0]);
        vectors.insert("set a timer for ten minutes".to_string(), vec![0.0, 1.0, 0.0]);
        let mock =
            Arc::new(MockOllama::with_generate_response(vectors, r#"{"duration_minutes": 10}"#));
        let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
        let registry = registry_with_timer_workflow();

        router.route("set a timer for ten minutes", &registry).await.expect("route should succeed");

        let requests = mock.generate_requests();
        assert_eq!(requests.len(), 1);
        let schema = &requests[0].3;
        let properties: Vec<&str> =
            schema["properties"].as_object().expect("properties must be an object").keys().map(|s| s.as_str()).collect();
        assert_eq!(
            properties,
            vec!["duration_minutes"],
            "expected the schema to name exactly the matched workflow's own parameters, no others"
        );
    }

    #[tokio::test]
    async fn extract_params_produces_byte_identical_requests_across_repeated_calls() {
        let mock =
            Arc::new(MockOllama::with_generate_response(HashMap::new(), r#"{"duration_minutes": 10}"#));
        let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
        let def = timer_definition();

        router.extract_params(&def, "set a timer for ten minutes").await;
        router.extract_params(&def, "set a timer for ten minutes").await;

        let requests = mock.generate_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0], requests[1],
            "expected byte-identical (model, system, prompt, schema) requests across repeated \
             calls with the same input"
        );
    }

    #[tokio::test]
    async fn extract_params_never_reads_the_registry_and_only_ever_sees_the_one_matched_definition() {
        // Threat model gate (T-09-01/T-09-14): `extract_params` takes a
        // single `&WorkflowDefinition`, never a `&Registry` -- there is
        // nothing for it to enumerate even if it tried. This test exercises
        // the call directly against a bare definition with no registry in
        // scope at all, proving the function's own signature is the
        // enforcement mechanism.
        let mock =
            Arc::new(MockOllama::with_generate_response(HashMap::new(), r#"{"duration_minutes": 5}"#));
        let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
        let def = timer_definition();

        let (extracted, detail) = router.extract_params(&def, "five minutes please").await;

        assert_eq!(extracted, Some(serde_json::json!({"duration_minutes": 5})));
        assert_eq!(detail, None);
    }

    // -----------------------------------------------------------------
    // Plan 10-02, Task 1: Router::check_intent_collision
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn a_probe_intent_identical_to_an_existing_intent_names_the_lower_id_candidate_when_it_is_also_higher_scoring() {
        let mut vectors = two_workflow_vectors();
        vectors.insert("a brand new calendar-like intent".to_string(), vec![1.0, 0.0, 0.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");
        let registry = registry_from(&[
            ("calendar_today", calendar_intent()),
            ("set_timer", timer_intent()),
        ]);

        let result = router
            .check_intent_collision("a brand new calendar-like intent", &registry)
            .await
            .expect("check_intent_collision should succeed");

        let collision = result.expect("expected a collision against calendar_today");
        assert_eq!(collision.workflow_id, "calendar_today");
        assert_eq!(collision.intent, calendar_intent());
    }

    #[tokio::test]
    async fn distinguishable_intents_report_no_collision() {
        let mut vectors = two_workflow_vectors();
        vectors.insert("something totally unrelated".to_string(), vec![0.0, 0.0, 1.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client, "nomic-embed-text", "llama3.2:3b");
        let registry = registry_from(&[
            ("calendar_today", calendar_intent()),
            ("set_timer", timer_intent()),
        ]);

        let result = router
            .check_intent_collision("something totally unrelated", &registry)
            .await
            .expect("check_intent_collision should succeed");

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn check_intent_collision_issues_exactly_one_probe_embed_call_beyond_the_cache_sync() {
        let mut vectors = two_workflow_vectors();
        vectors.insert("probe intent text".to_string(), vec![0.0, 0.0, 1.0]);
        let client = Arc::new(MockOllama::new(vectors));
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
        let registry = registry_from(&[
            ("calendar_today", calendar_intent()),
            ("set_timer", timer_intent()),
        ]);

        // First call embeds both candidates plus the probe -- 1 batched
        // call for the corpus sync, 1 for the probe.
        router.check_intent_collision("probe intent text", &registry).await.expect("first check");
        let calls_after_first = client.call_count();

        // Second call against the SAME unchanged registry should only issue
        // ONE additional embed call (the probe) -- the corpus is already
        // cached.
        router.check_intent_collision("probe intent text", &registry).await.expect("second check");
        let calls_after_second = client.call_count();

        assert_eq!(
            calls_after_second - calls_after_first,
            1,
            "expected exactly ONE additional embed call (the probe intent) beyond whatever the cache sync needs"
        );
    }

    #[tokio::test]
    async fn an_empty_or_whitespace_only_intent_returns_ok_none_with_zero_probe_calls() {
        let client = Arc::new(MockOllama::new(two_workflow_vectors()));
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
        let registry =
            registry_from(&[("calendar_today", calendar_intent()), ("set_timer", timer_intent())]);

        let result = router
            .check_intent_collision("   ", &registry)
            .await
            .expect("check_intent_collision should succeed (no collision, nothing to check)");

        assert_eq!(result, None);
        assert_eq!(client.call_count(), 0, "an empty/whitespace-only intent must never trigger an Ollama call");
    }

    #[tokio::test]
    async fn a_router_error_from_the_probe_embed_propagates_as_err_not_ok_none() {
        let client = Arc::new(MockOllama::with_behavior(HashMap::new(), MockBehavior::Unreachable));
        let router = Router::new(client.clone(), "nomic-embed-text", "llama3.2:3b");
        // No candidates -- isolates the assertion to the probe embed call.
        let registry = registry_from(&[]);

        let result = router.check_intent_collision("anything at all", &registry).await;

        assert_eq!(
            result,
            Err(RouterError::OllamaUnreachable {
                base_url: "mock://ollama".to_string(),
                detail: "connection refused".to_string()
            }),
            "expected an Ollama failure to propagate as Err, never swallowed into Ok(None)"
        );
    }
}
