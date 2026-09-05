//! Round-trip proof for the Phase 8 `intent`/`triggers` frontmatter fields
//! (D-06/D-07, FMT-01): a declared `intent` and `triggers` list survives
//! load -> write -> reload with byte-identical values and order, an
//! unsorted/duplicate `triggers` list is never normalized, and a hostile
//! `intent` scalar (colon, newline, YAML-significant leading character)
//! never breaks the frontmatter's YAML shape.
//!
//! Mirrors `agent_workflow_write_integration.rs`'s conventions: a `TempDir`
//! fixture written via `fs::write`, `Registry::load` for the read side, and
//! `registry::writer::create_workflow` for the write side.

use std::fs;

use orchestrator::registry::writer;
use orchestrator::Registry;
use shared::{CreateWorkflowRequest, WorkflowWriteMode};

fn base_request(id: &str) -> CreateWorkflowRequest {
    CreateWorkflowRequest {
        id: id.to_string(),
        name: "Test Workflow".to_string(),
        description: "A test workflow description.".to_string(),
        script: None,
        parameters: Vec::new(),
        mode: WorkflowWriteMode::Create,
        agent: None,
        intent: None,
        triggers: Vec::new(),
    }
}

const CALENDAR_MD: &str = r#"---
id: calendar_intent_rt
name: Calendar Intent Round Trip
service:
  handler: action.immediate
intent: "check today's calendar events"
triggers: [cli, voice]
---
Body prose.
"#;

#[test]
fn declared_intent_and_triggers_survive_load_write_reload() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    fs::write(dir.path().join("calendar_intent_rt.md"), CALENDAR_MD).expect("write fixture");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let loaded = registry
        .lookup("calendar_intent_rt")
        .expect("lookup should resolve the fixture");

    assert_eq!(loaded.intent, "check today's calendar events");
    assert_eq!(
        loaded.triggers,
        vec!["cli".to_string(), "voice".to_string()],
        "expected the exact ordered trigger list"
    );

    // Write path: a CreateWorkflowRequest carrying the same values, reloaded.
    let mut req = base_request("calendar_intent_rt_write");
    req.intent = Some(loaded.intent.clone());
    req.triggers = loaded.triggers.clone();

    writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");

    let (registry2, errors2) = Registry::load(dir.path());
    assert!(errors2.is_empty(), "unexpected load errors after write: {errors2:?}");
    let reloaded = registry2
        .lookup("calendar_intent_rt_write")
        .expect("lookup should resolve the written workflow");

    assert_eq!(
        reloaded.intent, loaded.intent,
        "expected the written-then-reloaded intent to equal the originally loaded intent"
    );
    assert_eq!(
        reloaded.triggers, loaded.triggers,
        "expected the written-then-reloaded triggers to equal the originally loaded triggers, in the same order"
    );
}

const DUPLICATE_UNSORTED_MD: &str = r#"---
id: triggers_unsorted_dup
name: Unsorted Duplicate Triggers
service:
  handler: action.immediate
triggers: [voice, cli, voice]
---
Body prose.
"#;

#[test]
fn declared_triggers_list_with_duplicate_and_unsorted_order_reloads_verbatim() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    fs::write(
        dir.path().join("triggers_unsorted_dup.md"),
        DUPLICATE_UNSORTED_MD,
    )
    .expect("write fixture");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let loaded = registry
        .lookup("triggers_unsorted_dup")
        .expect("lookup should resolve the fixture");

    assert_eq!(
        loaded.triggers,
        vec!["voice".to_string(), "cli".to_string(), "voice".to_string()],
        "expected the author's exact order and duplicate entry preserved verbatim -- no sorting, no deduping"
    );
}

#[test]
fn a_crafted_intent_with_yaml_control_characters_reloads_as_one_unchanged_scalar() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let hostile_intent = "- evil: true\nname: Hijacked\nkey: value:more".to_string();

    let mut req = base_request("intent_inject_rt");
    req.name = "Original Name".to_string();
    req.intent = Some(hostile_intent.clone());

    writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed even with a hostile intent");

    let (registry, errors) = Registry::load(dir.path());
    assert!(
        errors.is_empty(),
        "unexpected load errors -- a hostile intent must never break YAML parsing: {errors:?}"
    );
    let def = registry
        .lookup("intent_inject_rt")
        .expect("lookup should resolve");

    assert_eq!(
        def.intent, hostile_intent,
        "expected the reloaded intent to equal the original hostile string exactly, as one scalar"
    );
    assert_eq!(def.id, "intent_inject_rt", "expected id unaffected by the hostile intent");
    assert_eq!(def.name, "Original Name", "expected name unaffected by the hostile intent");
    assert_eq!(
        def.service.handler,
        orchestrator::definition::DEFAULT_HANDLER,
        "expected service.handler unaffected by the hostile intent"
    );
}
