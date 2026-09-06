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
//!
//! Task 2 (backward compatibility): every pre-Phase-8 workflow `.md` file
//! must keep loading with zero warnings, and an absent/empty/null `intent`
//! or `triggers` value must resolve to the same non-breaking default as
//! every other absent value in this schema -- never a `LoadError`.

use std::fs;
use std::path::Path;

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

/// Phase 9 (D-04): every shipped workflow `.md` file now declares a
/// non-empty, trimmed `intent` sentence authored against its real behaviour
/// -- the calibration corpus (`router_calibration/fixtures.rs`) is measured
/// against these exact values, so a load that silently resolves an intent
/// back to empty (a renamed/reverted `intent:` key) must fail loudly here,
/// naming the offending id. `triggers` remains untouched by this phase and
/// still resolves to the empty non-breaking default. Located relative to
/// `CARGO_MANIFEST_DIR` (mirrors `registry_integration.rs`'s existing
/// `real_workflows_directory_loads_with_zero_errors_and_all_four_ids_present`
/// precedent) rather than a hardcoded absolute path.
#[test]
fn real_workflows_directory_loads_with_zero_errors_and_every_workflow_declares_an_intent() {
    let workflows_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows");

    let (registry, errors) = Registry::load(&workflows_dir);

    assert!(
        errors.is_empty(),
        "expected the committed workflows/ dir to load with zero errors: {errors:?}"
    );

    let ids: Vec<String> = registry.enumerate().into_iter().map(|s| s.id).collect();
    assert!(
        !ids.is_empty(),
        "expected at least one pre-Phase-8 workflow to be loaded"
    );

    for id in &ids {
        let def = registry
            .lookup(id)
            .unwrap_or_else(|| panic!("expected {id} to resolve via lookup"));
        assert!(
            !def.intent.trim().is_empty(),
            "expected shipped workflow {id} to declare a non-empty intent (D-04), got: {:?}",
            def.intent
        );
        assert!(
            def.triggers.is_empty(),
            "expected shipped workflow {id} to resolve to an empty triggers list (untouched by this phase), got: {:?}",
            def.triggers
        );
    }
}

const TRIGGERS_ABSENT_MD: &str = r#"---
id: triggers_absent
name: Triggers Absent
service:
  handler: action.immediate
---
Body prose.
"#;

const TRIGGERS_EMPTY_LIST_MD: &str = r#"---
id: triggers_empty_list
name: Triggers Empty List
service:
  handler: action.immediate
triggers: []
---
Body prose.
"#;

const TRIGGERS_NULL_MD: &str = r#"---
id: triggers_null
name: Triggers Null
service:
  handler: action.immediate
triggers: null
---
Body prose.
"#;

/// An absent `triggers:` key, an empty `triggers: []`, and an explicit YAML
/// `null` all resolve to the same empty list -- none is a `LoadError`.
#[test]
fn absent_empty_list_and_explicit_null_triggers_all_resolve_to_the_same_empty_list() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    fs::write(dir.path().join("triggers_absent.md"), TRIGGERS_ABSENT_MD).expect("write fixture");
    fs::write(
        dir.path().join("triggers_empty_list.md"),
        TRIGGERS_EMPTY_LIST_MD,
    )
    .expect("write fixture");
    fs::write(dir.path().join("triggers_null.md"), TRIGGERS_NULL_MD).expect("write fixture");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

    for id in ["triggers_absent", "triggers_empty_list", "triggers_null"] {
        let def = registry
            .lookup(id)
            .unwrap_or_else(|| panic!("expected {id} to resolve via lookup"));
        assert!(
            def.triggers.is_empty(),
            "expected {id}'s triggers to resolve to an empty list, got: {:?}",
            def.triggers
        );
    }
}

const INTENT_ABSENT_MD: &str = r#"---
id: intent_absent
name: Intent Absent
service:
  handler: action.immediate
---
Body prose.
"#;

const INTENT_EMPTY_STRING_MD: &str = r#"---
id: intent_empty_string
name: Intent Empty String
service:
  handler: action.immediate
intent: ""
---
Body prose.
"#;

/// An absent `intent:` key and an explicit empty-string `intent: ""` both
/// resolve to the empty string -- neither is a `LoadError`.
#[test]
fn absent_intent_and_empty_string_intent_both_resolve_to_empty_string_with_no_load_error() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    fs::write(dir.path().join("intent_absent.md"), INTENT_ABSENT_MD).expect("write fixture");
    fs::write(
        dir.path().join("intent_empty_string.md"),
        INTENT_EMPTY_STRING_MD,
    )
    .expect("write fixture");

    let (registry, errors) = Registry::load(dir.path());
    assert!(
        errors.is_empty(),
        "an absent or empty-string intent must never produce a LoadError: {errors:?}"
    );

    for id in ["intent_absent", "intent_empty_string"] {
        let def = registry
            .lookup(id)
            .unwrap_or_else(|| panic!("expected {id} to resolve via lookup"));
        assert_eq!(
            def.intent, "",
            "expected {id}'s intent to resolve to the empty string, got: {:?}",
            def.intent
        );
    }
}
