//! Non-live corpus integrity gate for the router calibration fixtures
//! (Phase 9, ROUT-02, D-04/D-05). Runs in the default suite on every
//! commit -- needs no live Ollama and carries no `#[ignore]`. Its sibling,
//! `router_calibration.rs` (plan 09-05), is the live-gated test that
//! actually embeds these utterances and calibrates a threshold; this file
//! only proves the corpus itself is well-formed, so drift between
//! `workflows/` and the fixture data breaks the build immediately rather
//! than surfacing as a confusing live-test failure later.
//!
//! Includes the shared fixture module by `#[path]`, exactly like
//! `live_agent_eval.rs` includes `live_agent_eval/support.rs` -- a nested
//! file under `tests/router_calibration/`, never a bare `tests/*.rs` file,
//! so Cargo's autodiscovery never mistakes it for its own test binary.

use std::collections::HashSet;
use std::path::Path;

use orchestrator::Registry;

#[path = "router_calibration/fixtures.rs"]
mod fixtures;

use fixtures::{known_negatives, known_positives, NegativeKind, REAL_WORKFLOW_IDS};

fn real_registry() -> Registry {
    let workflows_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows");
    let (registry, errors) = Registry::load(&workflows_dir);
    assert!(
        errors.is_empty(),
        "expected the committed workflows/ dir to load with zero errors: {errors:?}"
    );
    registry
}

#[test]
fn known_positives_covers_every_workflow_with_at_least_three_cases_and_twelve_total() {
    let positives = known_positives();
    assert!(
        positives.len() >= 12,
        "expected at least 12 known-positive cases total, got {}",
        positives.len()
    );
    for id in REAL_WORKFLOW_IDS {
        let count = positives
            .iter()
            .filter(|c| c.expected_workflow_id == id)
            .count();
        assert!(
            count >= 3,
            "expected at least 3 known-positive cases for workflow `{id}`, got {count}"
        );
    }
}

#[test]
fn known_negatives_has_at_least_two_unrelated_and_one_close_but_wrong_per_workflow() {
    let negatives = known_negatives();
    let unrelated_count = negatives
        .iter()
        .filter(|c| c.kind == NegativeKind::Unrelated)
        .count();
    assert!(
        unrelated_count >= 2,
        "expected at least 2 clearly-unrelated negatives, got {unrelated_count}"
    );

    for id in REAL_WORKFLOW_IDS {
        let close_count = negatives
            .iter()
            .filter(|c| c.kind == NegativeKind::CloseButWrong && c.must_not_match == id)
            .count();
        assert!(
            close_count >= 1,
            "expected at least 1 close-but-wrong negative naming `{id}`, got {close_count}"
        );
    }
}

#[test]
fn every_case_references_a_real_workflow_id() {
    for case in known_positives() {
        assert!(
            REAL_WORKFLOW_IDS.contains(&case.expected_workflow_id),
            "positive case `{}` names unknown workflow id `{}`",
            case.utterance,
            case.expected_workflow_id
        );
    }
    for case in known_negatives() {
        assert!(
            REAL_WORKFLOW_IDS.contains(&case.must_not_match),
            "negative case `{}` names unknown workflow id `{}`",
            case.utterance,
            case.must_not_match
        );
    }
}

#[test]
fn every_real_workflow_id_loads_from_the_shipped_directory_with_a_non_empty_intent() {
    let registry = real_registry();
    for id in REAL_WORKFLOW_IDS {
        let def = registry
            .lookup(id)
            .unwrap_or_else(|| panic!("expected `{id}` to resolve via lookup against the real workflows/ directory"));
        assert!(
            !def.intent.trim().is_empty(),
            "expected `{id}` to declare a non-empty intent, got: {:?}",
            def.intent
        );
    }
}

#[test]
fn no_utterance_appears_twice_across_positives_and_negatives() {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut duplicates: Vec<&str> = Vec::new();

    for case in known_positives() {
        if !seen.insert(case.utterance) {
            duplicates.push(case.utterance);
        }
    }
    for case in known_negatives() {
        if !seen.insert(case.utterance) {
            duplicates.push(case.utterance);
        }
    }

    assert!(
        duplicates.is_empty(),
        "expected no utterance to be duplicated across positives and negatives, found: {duplicates:?}"
    );
}

#[test]
fn repeated_calls_to_accessors_return_cases_in_the_same_order() {
    let first_positives: Vec<&str> = known_positives().iter().map(|c| c.utterance).collect();
    let second_positives: Vec<&str> = known_positives().iter().map(|c| c.utterance).collect();
    assert_eq!(
        first_positives, second_positives,
        "expected known_positives() to return cases in a fixed, source-order-stable sequence"
    );

    let first_negatives: Vec<&str> = known_negatives().iter().map(|c| c.utterance).collect();
    let second_negatives: Vec<&str> = known_negatives().iter().map(|c| c.utterance).collect();
    assert_eq!(
        first_negatives, second_negatives,
        "expected known_negatives() to return cases in a fixed, source-order-stable sequence"
    );
}

#[test]
fn close_but_wrong_negatives_cover_the_set_timer_countdown_pair_in_both_directions() {
    let negatives = known_negatives();
    let set_timer_covered = negatives.iter().any(|c| {
        c.kind == NegativeKind::CloseButWrong && c.must_not_match == "set_timer"
    });
    let countdown_covered = negatives.iter().any(|c| {
        c.kind == NegativeKind::CloseButWrong && c.must_not_match == "countdown"
    });

    assert!(
        set_timer_covered,
        "expected a close-but-wrong negative naming `set_timer` (the hardest real pair)"
    );
    assert!(
        countdown_covered,
        "expected a close-but-wrong negative naming `countdown`, mirroring the `set_timer` case"
    );
}
