//! Save-time semantic-collision classifier (Phase 10 plan 10-02, D-03/D-04).
//! Pure, I/O-free, mirroring `confirm_tier.rs`'s fail-closed classification
//! shape and `similarity.rs`'s "pure function, never panics on
//! network-derived data" discipline. The only place the collision threshold
//! comparison happens -- `Router::check_intent_collision` (in `router/mod.rs`)
//! is the thin async wrapper that gathers candidates and makes the one
//! embedding call, then hands everything to [`find_collision`] here.

use crate::router::similarity::cosine_similarity;
use crate::router::threshold;

/// The winning (highest-scoring, above-threshold) candidate a new intent
/// collides with: its id, its own intent text (so the caller can render it),
/// and the cosine-similarity score against the new intent.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentCollision {
    pub workflow_id: String,
    pub intent: String,
    pub score: f32,
}

/// The authoring-time collision bar is DERIVED from `threshold::MATCH_THRESHOLD`,
/// never a fresh literal: an existing intent that a NEW intent would itself
/// match at routing time is, by construction, one the router can confuse the
/// two on -- so the authoring-time bar and the routing-time bar are the same
/// number by derivation, and 09-05's live calibration (which moved this from
/// a 0.60 placeholder to a measured 0.69) governs both. Introducing an
/// independent literal here would recreate exactly the placeholder-threshold
/// problem STATE.md records from plan 09-01.
pub const COLLISION_THRESHOLD: f32 = threshold::MATCH_THRESHOLD;

/// Scans `candidates` (each a `(workflow_id, intent, embedding)` triple) for
/// the highest-scoring one whose cosine similarity against `new_embedding`
/// clears `threshold` (inclusive). Pure, zero I/O, no async context -- callable
/// from a plain `#[test]` with no tokio runtime.
///
/// A strict greater-than comparison against the running best means an exact
/// score tie goes to the FIRST (lowest-id, assuming `candidates` arrives in
/// id-sorted order) candidate, matching `Router::route`'s documented
/// tie-break exactly. A candidate whose `cosine_similarity` returns `None`
/// (mismatched embedding dimension, zero norm, or a non-finite score) is
/// skipped entirely -- never treated as a zero score. The threshold is
/// applied once, at the end of the scan, never mid-loop.
pub fn find_collision(
    new_embedding: &[f32],
    candidates: &[(String, String, Vec<f32>)],
    threshold: f32,
) -> Option<IntentCollision> {
    let mut best: Option<(String, String, f32)> = None;
    for (id, intent, embedding) in candidates {
        let Some(score) = cosine_similarity(new_embedding, embedding) else {
            continue;
        };
        let is_better = match &best {
            Some((_, _, best_score)) => score > *best_score,
            None => true,
        };
        if is_better {
            best = Some((id.clone(), intent.clone(), score));
        }
    }

    best.and_then(|(workflow_id, intent, score)| {
        if score >= threshold {
            Some(IntentCollision { workflow_id, intent, score })
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, intent: &str, embedding: Vec<f32>) -> (String, String, Vec<f32>) {
        (id.to_string(), intent.to_string(), embedding)
    }

    #[test]
    fn empty_candidate_list_returns_none() {
        assert_eq!(find_collision(&[1.0, 0.0], &[], COLLISION_THRESHOLD), None);
    }

    #[test]
    fn every_candidate_strictly_below_threshold_returns_none() {
        let candidates = vec![
            candidate("aaa_wf", "some other intent", vec![0.0, 1.0, 0.0]),
            candidate("bbb_wf", "yet another intent", vec![0.0, 0.0, 1.0]),
        ];
        // Orthogonal to both candidates -- cosine similarity 0.0, well below
        // the threshold.
        assert_eq!(find_collision(&[1.0, 0.0, 0.0], &candidates, COLLISION_THRESHOLD), None);
    }

    #[test]
    fn returns_the_highest_scoring_candidate_when_two_are_above_threshold() {
        // Candidate "close" scores ~0.995 (nearly identical direction);
        // candidate "closer" is identical (score 1.0) -- both clear the
        // threshold, but "closer" must win.
        let candidates = vec![
            candidate("close_wf", "close intent", vec![1.0, 0.05, 0.0]),
            candidate("closer_wf", "closer intent", vec![1.0, 0.0, 0.0]),
        ];
        let result = find_collision(&[1.0, 0.0, 0.0], &candidates, COLLISION_THRESHOLD)
            .expect("expected a collision above threshold");
        assert_eq!(result.workflow_id, "closer_wf");
        assert_eq!(result.intent, "closer intent");
    }

    #[test]
    fn a_score_exactly_at_the_threshold_is_a_collision_inclusive() {
        let candidates = vec![candidate("wf", "intent", vec![1.0, 0.0])];
        // Identical vectors score exactly 1.0 -- construct a synthetic
        // boundary case by calling with a threshold equal to the observed
        // score (1.0), proving the inclusive `>=` comparison directly.
        let result = find_collision(&[1.0, 0.0], &candidates, 1.0);
        assert_eq!(
            result,
            Some(IntentCollision {
                workflow_id: "wf".to_string(),
                intent: "intent".to_string(),
                score: 1.0
            }),
            "expected a score exactly equal to the threshold to be reported as a collision (inclusive)"
        );
    }

    #[test]
    fn a_score_one_f32_step_below_threshold_is_not_a_collision() {
        let candidates = vec![candidate("wf", "intent", vec![1.0, 0.0])];
        let just_above_the_score = f32::from_bits(1.0_f32.to_bits() + 1);
        let result = find_collision(&[1.0, 0.0], &candidates, just_above_the_score);
        assert_eq!(result, None, "expected a score one f32 step below the threshold to not collide");
    }

    #[test]
    fn a_dimension_mismatched_candidate_is_skipped_and_a_valid_winner_still_returns() {
        let candidates = vec![
            // 3-dimensional -- mismatches the 2-dimensional probe below,
            // cosine_similarity returns None, must be skipped without panic.
            candidate("mismatched_wf", "mismatched intent", vec![1.0, 0.0, 0.0]),
            candidate("valid_wf", "valid intent", vec![1.0, 0.0]),
        ];
        let result = find_collision(&[1.0, 0.0], &candidates, COLLISION_THRESHOLD)
            .expect("expected the dimension-mismatched candidate to be skipped, not panic");
        assert_eq!(result.workflow_id, "valid_wf");
    }

    #[test]
    fn the_winning_candidates_own_intent_text_is_returned() {
        let candidates = vec![candidate("wf_a", "the winning intent text", vec![1.0, 0.0])];
        let result = find_collision(&[1.0, 0.0], &candidates, COLLISION_THRESHOLD)
            .expect("expected a collision");
        assert_eq!(result.intent, "the winning intent text");
    }

    #[test]
    fn an_exact_tie_goes_to_the_first_lowest_id_candidate() {
        let candidates = vec![
            candidate("aaa_wf", "identical intent", vec![1.0, 0.0]),
            candidate("bbb_wf", "identical intent", vec![1.0, 0.0]),
        ];
        let result = find_collision(&[1.0, 0.0], &candidates, COLLISION_THRESHOLD)
            .expect("expected a collision");
        assert_eq!(result.workflow_id, "aaa_wf", "expected the FIRST (lowest-id) candidate to win an exact tie");
    }

    #[test]
    fn collision_threshold_equals_match_threshold() {
        // Drift guard: if 09-05's calibration is ever re-measured, this
        // fails loudly rather than silently diverging (D-03/D-04).
        assert_eq!(COLLISION_THRESHOLD, threshold::MATCH_THRESHOLD);
    }
}
