//! The double-gated LIVE calibration harness (Phase 9, ROUT-02/04, plan
//! 09-05). The phase's only end-to-end proof against the real world: real
//! Ollama, the real shipped `workflows/` `intent:` sentences, the real
//! `cosine_similarity` code, and the real `Router::route` pipeline (not a
//! mock, not a synthetic corpus).
//!
//! THIS FILE MUST NEVER BE WIRED INTO AUTOMATED CONTINUOUS INTEGRATION.
//! There is no `.github/workflows` job that runs `--ignored` tests today; if
//! one is ever added, it must never include this file. Every test function
//! below carries the `ignore` attribute, so the default `cargo test` /
//! `make test` command excludes it. Each test ADDITIONALLY reads
//! `WK_ROUTER_CALIBRATION_LIVE` and returns early -- a visible skip,
//! printed, never a failure -- unless that variable holds exactly `"1"`.
//! Both gates are required: the attribute alone would still run under
//! `--include-ignored`; the environment check alone would still run if
//! someone deleted the attribute. Run deliberately with:
//!
//! ```text
//! WK_ROUTER_CALIBRATION_LIVE=1 cargo test --manifest-path apps/orchestrator/Cargo.toml \
//!   --target-dir apps/orchestrator/target --test router_calibration -- --ignored --nocapture
//! ```
//!
//! Includes the shared calibration corpus by `#[path]`, exactly like
//! `router_fixtures.rs` -- one corpus definition, two consumers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use orchestrator::router::confirm_tier::{confirm_tier, ConfirmTier};
use orchestrator::router::ollama_client::{HttpOllamaClient, OllamaApi};
use orchestrator::router::similarity::cosine_similarity;
use orchestrator::router::threshold::{
    CONFIRM_REQUIRED_MATCH_THRESHOLD, MATCH_THRESHOLD, REQUIRED_SEPARATION_MARGIN,
};
use orchestrator::router::Router;
use orchestrator::Registry;

#[path = "router_calibration/fixtures.rs"]
mod fixtures;

use fixtures::{known_negatives, known_positives, NegativeKind};

/// The opt-in environment variable gating this entire file. Its literal
/// spelling lives here once so every test in this file checks the identical
/// name -- mirrors `live_agent_eval/support.rs::LIVE_EVAL_OPT_IN_VAR`.
const CALIBRATION_OPT_IN_VAR: &str = "WK_ROUTER_CALIBRATION_LIVE";

/// `true` only when the opt-in variable holds exactly `"1"`.
fn opted_in() -> bool {
    std::env::var(CALIBRATION_OPT_IN_VAR).map(|v| v == "1").unwrap_or(false)
}

/// Matches `orchestratord`'s own `--ollama-url` default (`ORCHESTRATOR_OLLAMA_URL`,
/// `bin/orchestratord.rs`) -- never a value invented independently here.
const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

/// Matches `orchestratord`'s own `--embed-model` default.
const EMBED_MODEL: &str = "nomic-embed-text";

/// Loads the real, committed `workflows/` directory -- the same resolution
/// `registry_integration.rs`/`confirm_tier.rs`'s real-workflow test use.
fn real_registry() -> Registry {
    let workflows_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows");
    let (registry, errors) = Registry::load(&workflows_dir);
    assert!(
        errors.is_empty(),
        "expected the committed workflows/ dir to load with zero errors: {errors:?}"
    );
    registry
}

/// One embedding call for a batch of inputs, panicking with a message
/// naming the Ollama endpoint/model/error on failure -- never an unwrapped
/// `Result`. `endpoint_desc` names what was being embedded, for a readable
/// failure.
async fn embed_or_fail(
    client: &dyn OllamaApi,
    model: &str,
    inputs: &[String],
    endpoint_desc: &str,
) -> Vec<Vec<f32>> {
    match client.embed(model, inputs).await {
        Ok(vectors) => vectors,
        Err(e) => panic!(
            "embedding call to Ollama at {OLLAMA_BASE_URL} ({endpoint_desc}, model `{model}`) \
             failed: {e}. Is Ollama running with `{model}` pulled? (`curl -sf {OLLAMA_BASE_URL}/api/version`, `ollama list`)"
        ),
    }
}

/// Embeds every real shipped workflow's real `intent:` sentence, once,
/// keyed by workflow id -- never a fixture copy of the intents. Panics
/// (naming the endpoint/model) on any embedding failure, and asserts the
/// response carries one vector per requested intent so a silent
/// batch-count mismatch cannot masquerade as a valid score matrix.
async fn embed_workflow_intents(
    client: &dyn OllamaApi,
    model: &str,
    registry: &Registry,
) -> HashMap<String, Vec<f32>> {
    let mut ids: Vec<String> = Vec::new();
    let mut intents: Vec<String> = Vec::new();
    for summary in registry.enumerate() {
        if let Some(def) = registry.lookup(&summary.id) {
            let intent = def.intent.trim();
            if !intent.is_empty() {
                ids.push(summary.id.clone());
                intents.push(intent.to_string());
            }
        }
    }
    let vectors = embed_or_fail(client, model, &intents, "the shipped workflows' intent sentences").await;
    assert_eq!(
        vectors.len(),
        ids.len(),
        "expected exactly one embedding vector per workflow intent, got {} vectors for {} intents",
        vectors.len(),
        ids.len()
    );
    ids.into_iter().zip(vectors).collect()
}

/// The maximum cosine similarity `utterance_embedding` achieves against
/// ANY workflow intent in `intent_embeddings`, plus which workflow id
/// produced it -- "the worst case, its best false match" (Task 2's own
/// wording). A non-finite/incomparable pairing (`cosine_similarity`
/// returning `None`, e.g. a dimension mismatch) is skipped from the
/// maximum rather than participating in a comparison; if EVERY pairing is
/// skipped, `None` propagates so the caller can fail loudly by name rather
/// than silently reporting a vacuous `f32::MIN`.
fn max_similarity_and_best_match(
    utterance_embedding: &[f32],
    intent_embeddings: &HashMap<String, Vec<f32>>,
) -> Option<(f32, String)> {
    let mut best: Option<(f32, String)> = None;
    // Iterate in a stable (sorted-by-id) order so an exact tie always
    // resolves to the same reported "best match" across runs.
    let mut ids: Vec<&String> = intent_embeddings.keys().collect();
    ids.sort();
    for id in ids {
        let intent_embedding = &intent_embeddings[id];
        if let Some(score) = cosine_similarity(utterance_embedding, intent_embedding) {
            let is_better = match &best {
                Some((best_score, _)) => score > *best_score,
                None => true,
            };
            if is_better {
                best = Some((score, id.clone()));
            }
        }
    }
    best
}

// =========================================================================
// Task 1: every known positive routes to its expected workflow, end to end
// =========================================================================

#[ignore]
#[tokio::test]
async fn every_known_positive_utterance_routes_to_its_expected_workflow_against_a_live_ollama() {
    if !opted_in() {
        println!(
            "SKIP: set {CALIBRATION_OPT_IN_VAR}=1 to run against a live local Ollama server. \
             See 09-05-PLAN.md."
        );
        return;
    }

    let positives = known_positives();
    let negatives = known_negatives();
    assert!(
        !positives.is_empty(),
        "the known-positive corpus must be non-empty before measuring anything -- a calibration \
         that silently passes over an empty corpus is worse than no calibration"
    );
    assert!(
        !negatives.is_empty(),
        "the known-negative corpus must be non-empty before measuring anything -- a calibration \
         that silently passes over an empty corpus is worse than no calibration"
    );

    let registry = real_registry();
    let client: Arc<dyn OllamaApi> = Arc::new(HttpOllamaClient::new(OLLAMA_BASE_URL));
    let router = Router::new(client, EMBED_MODEL);

    // Route through the FULL pipeline (thresholds, tie-break, cache sync,
    // refusal logic), never `cosine_similarity` directly -- the same run
    // that produces the numbers also proves the pipeline that will use them.
    let mut failures: Vec<String> = Vec::new();
    for case in positives {
        let outcome = router.route(case.utterance, &registry).await.unwrap_or_else(|e| {
            panic!(
                "route() for positive utterance {:?} failed talking to Ollama at {OLLAMA_BASE_URL}: {e}",
                case.utterance
            )
        });
        match &outcome.matched_workflow_id {
            Some(id) if id == case.expected_workflow_id => {
                println!(
                    "POSITIVE OK: {:?} -> `{}` (score {:.6})",
                    case.utterance,
                    id,
                    outcome.similarity_score.unwrap_or(f32::NAN)
                );
            }
            other => {
                failures.push(format!(
                    "utterance {:?}: expected `{}`, got `{:?}` (score {:?}, detail {:?})",
                    case.utterance, case.expected_workflow_id, other, outcome.similarity_score, outcome.detail
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "the following known-positive utterances did not route to their expected workflow:\n{}",
        failures.join("\n")
    );
}

// =========================================================================
// Task 2: measured separation, derived threshold positioning, and the
// independent confirmation-tier separation Pitfall C requires
// =========================================================================

/// One row of the printed per-case score table.
struct ScoreRow {
    workflow_id: String,
    utterance: String,
    score: f32,
    kind: Option<NegativeKind>,
    best_match_id: String,
}

/// Prints the per-case score table sorted by workflow id then utterance --
/// deterministic across runs so two runs diff cleanly.
fn print_score_table(rows: &[ScoreRow]) {
    let mut sorted: Vec<&ScoreRow> = rows.iter().collect();
    sorted.sort_by(|a, b| a.workflow_id.cmp(&b.workflow_id).then_with(|| a.utterance.cmp(&b.utterance)));
    println!("--- calibration score table (sorted by workflow id, then utterance) ---");
    for row in sorted {
        match row.kind {
            Some(kind) => println!(
                "  [{}] {:?} kind={kind:?} score={:.6} best_match=`{}`",
                row.workflow_id, row.utterance, row.score, row.best_match_id
            ),
            None => println!(
                "  [{}] {:?} score={:.6} best_match=`{}`",
                row.workflow_id, row.utterance, row.score, row.best_match_id
            ),
        }
    }
}

#[ignore]
#[tokio::test]
async fn the_measured_separation_between_positives_and_negatives_meets_the_required_margin() {
    if !opted_in() {
        println!(
            "SKIP: set {CALIBRATION_OPT_IN_VAR}=1 to run against a live local Ollama server. \
             See 09-05-PLAN.md."
        );
        return;
    }

    let positives = known_positives();
    let negatives = known_negatives();
    assert!(!positives.is_empty(), "the known-positive corpus must be non-empty before measuring anything");
    assert!(!negatives.is_empty(), "the known-negative corpus must be non-empty before measuring anything");

    let registry = real_registry();
    let client: Arc<dyn OllamaApi> = Arc::new(HttpOllamaClient::new(OLLAMA_BASE_URL));
    let intent_embeddings = embed_workflow_intents(client.as_ref(), EMBED_MODEL, &registry).await;

    let positive_utterances: Vec<String> = positives.iter().map(|c| c.utterance.to_string()).collect();
    let positive_vectors =
        embed_or_fail(client.as_ref(), EMBED_MODEL, &positive_utterances, "known-positive utterances").await;
    assert_eq!(positive_vectors.len(), positives.len(), "expected one embedding vector per positive utterance");

    let negative_utterances: Vec<String> = negatives.iter().map(|c| c.utterance.to_string()).collect();
    let negative_vectors =
        embed_or_fail(client.as_ref(), EMBED_MODEL, &negative_utterances, "known-negative utterances").await;
    assert_eq!(negative_vectors.len(), negatives.len(), "expected one embedding vector per negative utterance");

    let mut rows: Vec<ScoreRow> = Vec::new();

    // Positives: score = cosine_similarity(utterance, its OWN expected
    // workflow's intent) -- never the max across all workflows, since the
    // interesting positive quantity is "how well does this utterance match
    // the workflow it's supposed to match", not its best false match.
    let mut positive_scores: Vec<f32> = Vec::new();
    for (case, vector) in positives.iter().zip(positive_vectors.iter()) {
        let intent_embedding = intent_embeddings.get(case.expected_workflow_id).unwrap_or_else(|| {
            panic!(
                "positive case names workflow id `{}`, which has no embedded intent -- corpus/registry drift",
                case.expected_workflow_id
            )
        });
        let score = cosine_similarity(vector, intent_embedding).unwrap_or_else(|| {
            panic!(
                "non-finite or incomparable score for positive utterance {:?} against `{}` -- \
                 a dimension mismatch or non-finite embedding must fail the run, not participate \
                 in a comparison",
                case.utterance, case.expected_workflow_id
            )
        });
        positive_scores.push(score);
        let (_, best_match_id) = max_similarity_and_best_match(vector, &intent_embeddings).unwrap_or_else(|| {
            panic!("no comparable intent embedding found for positive utterance {:?}", case.utterance)
        });
        rows.push(ScoreRow {
            workflow_id: case.expected_workflow_id.to_string(),
            utterance: case.utterance.to_string(),
            score,
            kind: None,
            best_match_id,
        });
    }

    // Negatives: two distinct scoring rules, chosen by kind --
    //
    // - `Unrelated`: score = MAXIMUM cosine similarity across ALL workflow
    //   intents -- the worst case, the utterance's best false match --
    //   because an unrelated utterance has no legitimate true domain
    //   anywhere, so any high score anywhere is a genuine false-match risk
    //   (this is exactly RESEARCH's Code Examples rationale, and the plan's
    //   own action text).
    //
    // - `CloseButWrong`: score = cosine similarity against SPECIFICALLY the
    //   workflow it names (`must_not_match`), not the corpus-wide max.
    //   DEVIATION from the plan's literal action text (Rule 1 -- bug,
    //   discovered live against the real corpus): the corpus's own
    //   set_timer/countdown "mirror pair" (09-02-SUMMARY.md) is built from
    //   utterances that are genuinely, correctly a positive match for their
    //   OTHER real domain (e.g. "set a timer for ten minutes and notify me
    //   when it's done" is tagged `must_not_match: countdown` specifically
    //   BECAUSE it is really a valid set_timer request). Taking the
    //   corpus-wide max for such a case captures its legitimate, CORRECT
    //   match against the other real workflow -- not a false match at all
    //   -- and comparing that score against `min_positive` in the general
    //   separation assertion below would make separation mathematically
    //   impossible for any intent wording, since the "negative" would
    //   always score approximately as high as a genuine positive. Scoring
    //   specifically against the named `must_not_match` id instead tests
    //   exactly what the field means ("this utterance must not read as a
    //   match for THIS workflow") without corrupting the corpus-wide
    //   min/max math with an unrelated, legitimate match. For the
    //   single-domain-adjacent CloseButWrong negatives (calendar_today,
    //   ai_summarize) this produces an IDENTICAL result to the corpus-wide
    //   max, since those utterances have no other legitimate domain to
    //   maximize against -- this only changes behavior for the mirror pair.
    let mut negative_scores: Vec<f32> = Vec::new();
    for (case, vector) in negatives.iter().zip(negative_vectors.iter()) {
        let (score, best_match_id) = match case.kind {
            NegativeKind::Unrelated => max_similarity_and_best_match(vector, &intent_embeddings).unwrap_or_else(|| {
                panic!(
                    "non-finite or incomparable score for negative utterance {:?} -- every \
                     candidate pairing was unusable; this must fail the run, not silently sort \
                     as low",
                    case.utterance
                )
            }),
            NegativeKind::CloseButWrong => {
                let named_intent = intent_embeddings.get(case.must_not_match).unwrap_or_else(|| {
                    panic!(
                        "negative case names workflow id `{}`, which has no embedded intent -- \
                         corpus/registry drift",
                        case.must_not_match
                    )
                });
                let score = cosine_similarity(vector, named_intent).unwrap_or_else(|| {
                    panic!(
                        "non-finite or incomparable score for negative utterance {:?} against \
                         `{}` -- a dimension mismatch or non-finite embedding must fail the run, \
                         not participate in a comparison",
                        case.utterance, case.must_not_match
                    )
                });
                (score, case.must_not_match.to_string())
            }
        };
        assert!(
            score.is_finite(),
            "non-finite score for negative utterance {:?}: {score}",
            case.utterance
        );
        negative_scores.push(score);
        rows.push(ScoreRow {
            workflow_id: case.must_not_match.to_string(),
            utterance: case.utterance.to_string(),
            score,
            kind: Some(case.kind),
            best_match_id,
        });
    }

    print_score_table(&rows);

    let min_positive = positive_scores.iter().cloned().fold(f32::INFINITY, f32::min);
    let max_negative = negative_scores.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let observed_margin = min_positive - max_negative;

    assert!(
        observed_margin >= REQUIRED_SEPARATION_MARGIN,
        "no safe threshold exists: lowest observed positive={min_positive:.6}, highest observed \
         negative={max_negative:.6} (observed margin {observed_margin:.6} < required \
         {REQUIRED_SEPARATION_MARGIN}). This is a signal about the intent sentences or the \
         fixtures -- revise them and re-run. Never resolve this by relaxing the threshold or the margin."
    );

    // The assertion is POSITIONAL, not a hard-coded number: the shipped
    // MATCH_THRESHOLD must sit strictly above the highest observed negative
    // and at or below the lowest observed positive -- this keeps meaning
    // after retuning and fails whenever an intent edit erodes the
    // separation the shipped value depends on.
    assert!(
        MATCH_THRESHOLD > max_negative,
        "shipped MATCH_THRESHOLD ({MATCH_THRESHOLD}) must sit strictly above the highest observed \
         negative ({max_negative:.6})"
    );
    assert!(
        MATCH_THRESHOLD <= min_positive,
        "shipped MATCH_THRESHOLD ({MATCH_THRESHOLD}) must sit at or below the lowest observed \
         positive ({min_positive:.6})"
    );

    println!(
        "Observed: lowest positive={min_positive:.6}, highest negative={max_negative:.6}, \
         margin={observed_margin:.6}. Shipped MATCH_THRESHOLD={MATCH_THRESHOLD}."
    );
}

#[ignore]
#[tokio::test]
async fn confirm_required_workflows_separate_independently_from_close_but_wrong_negatives() {
    if !opted_in() {
        println!(
            "SKIP: set {CALIBRATION_OPT_IN_VAR}=1 to run against a live local Ollama server. \
             See 09-05-PLAN.md."
        );
        return;
    }

    let negatives = known_negatives();
    assert!(!negatives.is_empty(), "the known-negative corpus must be non-empty before measuring anything");

    let registry = real_registry();
    let client: Arc<dyn OllamaApi> = Arc::new(HttpOllamaClient::new(OLLAMA_BASE_URL));
    let intent_embeddings = embed_workflow_intents(client.as_ref(), EMBED_MODEL, &registry).await;

    // The confirm-required workflow ids in the real shipped registry --
    // computed from `confirm_tier`, never hard-coded to `ai_summarize` by
    // name, so this test stays correct if the shipped set ever changes.
    let confirm_required_ids: Vec<String> = registry
        .enumerate()
        .into_iter()
        .filter_map(|summary| {
            registry.lookup(&summary.id).and_then(|def| {
                if confirm_tier(def) == ConfirmTier::ConfirmRequired {
                    Some(summary.id)
                } else {
                    None
                }
            })
        })
        .collect();
    assert!(
        !confirm_required_ids.is_empty(),
        "expected at least one confirm-required workflow in the real shipped registry \
         (ai_summarize, per D-01) -- Pitfall C's separation has nothing to test against otherwise"
    );

    // Close-but-wrong negatives adjacent to a confirm-required workflow's
    // domain -- the cases Pitfall C is actually about.
    let adjacent_negatives: Vec<&fixtures::NegativeCase> = negatives
        .iter()
        .filter(|c| c.kind == NegativeKind::CloseButWrong && confirm_required_ids.iter().any(|id| id == c.must_not_match))
        .collect();
    assert!(
        !adjacent_negatives.is_empty(),
        "expected at least one close-but-wrong negative adjacent to a confirm-required workflow's \
         domain -- found none among: {:?}",
        negatives.iter().map(|c| (c.utterance, c.must_not_match, c.kind)).collect::<Vec<_>>()
    );

    let utterances: Vec<String> = adjacent_negatives.iter().map(|c| c.utterance.to_string()).collect();
    let vectors = embed_or_fail(client.as_ref(), EMBED_MODEL, &utterances, "confirm-tier-adjacent negatives").await;
    assert_eq!(vectors.len(), adjacent_negatives.len());

    let mut rows: Vec<ScoreRow> = Vec::new();
    let mut max_score: f32 = f32::NEG_INFINITY;
    for (case, vector) in adjacent_negatives.iter().zip(vectors.iter()) {
        let (score, best_match_id) = max_similarity_and_best_match(vector, &intent_embeddings).unwrap_or_else(|| {
            panic!(
                "non-finite or incomparable score for confirm-tier-adjacent negative {:?} -- must \
                 fail the run, never sort silently",
                case.utterance
            )
        });
        assert!(score.is_finite(), "non-finite score for negative utterance {:?}: {score}", case.utterance);
        max_score = max_score.max(score);
        rows.push(ScoreRow {
            workflow_id: case.must_not_match.to_string(),
            utterance: case.utterance.to_string(),
            score,
            kind: Some(case.kind),
            best_match_id,
        });
    }

    print_score_table(&rows);

    // Independent of the general MATCH_THRESHOLD assertion: the highest
    // score any close-but-wrong negative adjacent to a confirm-required
    // workflow achieves must fall below CONFIRM_REQUIRED_MATCH_THRESHOLD --
    // a single global threshold is exactly what Pitfall C warns against.
    assert!(
        max_score < CONFIRM_REQUIRED_MATCH_THRESHOLD,
        "the highest score among close-but-wrong negatives adjacent to a confirm-required workflow \
         ({max_score:.6}) must fall below CONFIRM_REQUIRED_MATCH_THRESHOLD \
         ({CONFIRM_REQUIRED_MATCH_THRESHOLD}) -- a benign workflow's threshold must never double as \
         the bar a destructive/agent-type workflow clears"
    );

    // The ordering invariant, asserted here too so retuning cannot invert
    // the two constants without this test noticing.
    assert!(
        CONFIRM_REQUIRED_MATCH_THRESHOLD >= MATCH_THRESHOLD,
        "CONFIRM_REQUIRED_MATCH_THRESHOLD ({CONFIRM_REQUIRED_MATCH_THRESHOLD}) must never be below \
         MATCH_THRESHOLD ({MATCH_THRESHOLD})"
    );

    println!(
        "Observed: highest confirm-tier-adjacent negative score={max_score:.6}. Shipped \
         CONFIRM_REQUIRED_MATCH_THRESHOLD={CONFIRM_REQUIRED_MATCH_THRESHOLD}."
    );
}
