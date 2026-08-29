//! End-to-end skeleton test (Wave-0, RED): drives the full read path
//! (load -> enumerate -> lookup) against a real `set_timer.md`-shaped
//! fixture written to a tempdir.
//!
//! This test MUST fail against the Task-2 stub `Registry::load`, which
//! always returns an empty registry. Plan 02 implements the real loader
//! and turns this test GREEN — do not implement the loader in this plan.

use std::fs;

use orchestrator::{ParameterType, Registry};
use tempfile::TempDir;

const SET_TIMER_MD: &str = r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    description: how many minutes the timer should run for
    required: true
service:
  type: action
  handler: timers.start
---
Sets a local countdown timer and speaks a confirmation when it expires.
"#;

#[test]
fn registry_loads_enumerates_and_looks_up_set_timer() {
    let dir = TempDir::new().expect("failed to create tempdir");
    fs::write(dir.path().join("set_timer.md"), SET_TIMER_MD).expect("failed to write fixture");

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

    let summaries = registry.enumerate();
    assert_eq!(
        summaries.len(),
        1,
        "expected exactly 1 enumerated workflow, got {}: {summaries:?}",
        summaries.len()
    );

    let def = registry
        .lookup("set_timer")
        .expect("lookup(\"set_timer\") should return the loaded definition");

    assert_eq!(def.name, "Set Timer");
    assert_eq!(
        def.description,
        "Sets a local countdown timer and speaks a confirmation when it expires."
    );

    let param = def
        .parameters
        .get("duration_minutes")
        .expect("parameters should contain duration_minutes");
    assert_eq!(param.type_, ParameterType::Int);
    assert!(param.required);
}
