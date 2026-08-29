//! Integration tests for the real registry read path (WF-01/02/03):
//! every rejection path (D-06/D-07/D-08/D-09) plus the zero-parameter case
//! (Pitfall 3). Each rejection test asserts a `LoadError` message substring
//! naming the file+field/value — none assert a panic; the loader must
//! never panic on malformed input.

use std::fs;
use std::path::{Path, PathBuf};

use orchestrator::definition::AgentSpec;
use orchestrator::{LoadError, ParameterType, Registry, ServiceMode};
use tempfile::TempDir;

/// Writes `contents` to `dir/filename`, creating any intermediate
/// directories the fixture requests.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

#[test]
fn valid_multi_field_workflow_loads_and_lookup_returns_parameter_schema() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "multi.md",
        r#"---
id: multi_param
name: Multi Param Workflow
parameters:
  amount:
    type: int
    description: how much
    required: true
  label:
    type: string
    description: a label
    required: false
  loud:
    type: bool
    required: true
service:
  type: action
  handler: demo.multi
---
Does a multi-parameter thing.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");

    let def = registry
        .lookup("multi_param")
        .expect("lookup(\"multi_param\") should return the loaded definition");

    assert_eq!(def.name, "Multi Param Workflow");
    assert_eq!(def.description, "Does a multi-parameter thing.");

    let amount = def.parameters.get("amount").expect("amount param present");
    assert_eq!(amount.type_, ParameterType::Int);
    assert!(amount.required);

    let label = def.parameters.get("label").expect("label param present");
    assert_eq!(label.type_, ParameterType::String);
    assert!(!label.required);

    let loud = def.parameters.get("loud").expect("loud param present");
    assert_eq!(loud.type_, ParameterType::Bool);
    assert!(loud.required);
}

#[test]
fn missing_id_is_rejected_and_registry_is_empty() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "no_id.md",
        r#"---
name: No Id Workflow
service:
  type: action
  handler: demo.no_id
---
Missing the id field.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    let message = errors[0].to_string();
    assert!(
        message.contains("id"),
        "expected error to name field `id`, got: {message}"
    );
}

#[test]
fn workflow_with_absent_service_handler_defaults_to_action_immediate() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "no_handler.md",
        r#"---
id: no_handler
name: No Handler Workflow
service:
  type: action
---
Missing service.handler entirely (defaults to the run-now fallback).
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("no_handler")
        .expect("lookup(\"no_handler\") should return the loaded definition");
    assert_eq!(
        def.service.handler,
        orchestrator::definition::DEFAULT_HANDLER,
        "expected an absent service.handler to default to DEFAULT_HANDLER"
    );
}

#[test]
fn workflow_with_empty_service_handler_defaults_to_action_immediate() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "empty_handler.md",
        r#"---
id: empty_handler_default
name: Empty Handler Workflow
service:
  type: action
  handler: ""
---
Handler is an explicit empty string (defaults to the run-now fallback).
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("empty_handler_default")
        .expect("lookup(\"empty_handler_default\") should return the loaded definition");
    assert_eq!(
        def.service.handler,
        orchestrator::definition::DEFAULT_HANDLER,
        "expected an empty service.handler to default to DEFAULT_HANDLER"
    );
}

#[test]
fn empty_string_required_fields_are_rejected_as_missing() {
    let dir = TempDir::new().expect("failed to create tempdir");
    // Files load in filename order: b_ (id), c_ (name). `service.handler`'s
    // empty-string case is no longer rejected here -- it defaults to
    // DEFAULT_HANDLER (see workflow_with_empty_service_handler_defaults_to_
    // action_immediate above) -- only `id`/`name` are still hard-required.
    write_workflow(
        dir.path(),
        "b_empty_id.md",
        r#"---
id: ""
name: Empty Id Workflow
service:
  type: action
  handler: demo.empty_id
---
Id is an explicit empty string.
"#,
    );
    write_workflow(
        dir.path(),
        "c_whitespace_name.md",
        r#"---
id: whitespace_name
name: "   "
service:
  type: action
  handler: demo.whitespace_name
---
Name is whitespace-only.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 2, "expected exactly two load errors: {errors:?}");
    assert!(
        errors[0].to_string().contains("missing required field `id`"),
        "expected empty id to be rejected as missing, got: {}",
        errors[0]
    );
    assert!(
        errors[1].to_string().contains("missing required field `name`"),
        "expected whitespace-only name to be rejected as missing, got: {}",
        errors[1]
    );
    assert!(
        registry.lookup("").is_none(),
        "an empty id must never be addressable via lookup"
    );
}

#[test]
fn unknown_parameter_type_is_rejected_naming_the_value() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "bad_type.md",
        r#"---
id: bad_type
name: Bad Type Workflow
parameters:
  amount:
    type: float
    required: true
service:
  type: action
  handler: demo.bad_type
---
Has an unsupported parameter type.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    let message = errors[0].to_string();
    assert!(
        message.contains("float"),
        "expected error to name the offending value `float`, got: {message}"
    );
}

#[test]
fn one_bad_one_good_file_loads_good_and_collects_exactly_one_error() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "good.md",
        r#"---
id: good_workflow
name: Good Workflow
parameters: {}
service:
  type: action
  handler: demo.good
---
This one is fine.
"#,
    );
    write_workflow(
        dir.path(),
        "bad.md",
        r#"---
name: Missing Id Again
service:
  type: action
  handler: demo.bad
---
This one is missing an id.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    let summaries = registry.enumerate();
    assert_eq!(summaries.len(), 1, "expected exactly 1 loaded workflow: {summaries:?}");
    assert_eq!(errors.len(), 1, "expected exactly 1 load error: {errors:?}");
    assert!(registry.lookup("good_workflow").is_some());
}

#[test]
fn duplicate_id_across_two_files_is_reported_and_first_wins() {
    let dir = TempDir::new().expect("failed to create tempdir");
    // "a_first.md" sorts before "b_second.md" so it is the first-loaded
    // definition and should win per D-09.
    write_workflow(
        dir.path(),
        "a_first.md",
        r#"---
id: dup_id
name: First Definition
parameters: {}
service:
  type: action
  handler: demo.first
---
The first one loaded.
"#,
    );
    write_workflow(
        dir.path(),
        "b_second.md",
        r#"---
id: dup_id
name: Second Definition
parameters: {}
service:
  type: action
  handler: demo.second
---
The second one loaded.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    let summaries = registry.enumerate();
    assert_eq!(summaries.len(), 1, "expected exactly 1 loaded workflow: {summaries:?}");
    assert_eq!(errors.len(), 1, "expected exactly one duplicate-id error: {errors:?}");
    let message = errors[0].to_string();
    assert!(
        message.contains("dup_id"),
        "expected error to name the duplicate id, got: {message}"
    );

    let def = registry
        .lookup("dup_id")
        .expect("lookup(\"dup_id\") should return the first-loaded definition");
    assert_eq!(def.name, "First Definition");
}

#[test]
fn zero_parameter_workflow_loads_with_empty_parameter_map() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "get_time.md",
        r#"---
id: get_time
name: Get Current Time
parameters: {}
service:
  type: action
  handler: clock.current_time
---
Speaks the current local time.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("get_time")
        .expect("lookup(\"get_time\") should return the loaded definition");
    assert!(def.parameters.is_empty(), "expected an empty parameter map");
}

#[test]
fn file_without_frontmatter_is_rejected_with_no_frontmatter_error() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "plain.md",
        "Just prose. No frontmatter delimiters anywhere.\n",
    );

    let (registry, errors) = Registry::load(dir.path());

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    let message = errors[0].to_string();
    assert!(
        message.contains("no YAML frontmatter"),
        "expected a NoFrontmatter error, got: {message}"
    );
}

#[test]
fn malformed_yaml_frontmatter_is_rejected_with_parse_error() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "broken.md",
        r#"---
id: [unclosed
name: Broken Workflow
service:
  handler: demo.broken
---
Frontmatter YAML is malformed.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    let message = errors[0].to_string();
    assert!(
        message.contains("parse error"),
        "expected a ParseError for malformed YAML, got: {message}"
    );
}

#[test]
fn workflow_in_nested_subdirectory_is_scanned_and_loaded() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "nested/deeper/inner.md",
        r#"---
id: nested_workflow
name: Nested Workflow
service:
  type: action
  handler: demo.nested
---
Lives two directories deep.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("nested_workflow")
        .expect("lookup(\"nested_workflow\") should return the nested definition");
    assert_eq!(def.name, "Nested Workflow");
}

/// 06-05 (SVC-03): a `.md` file living directly inside a workflow-directory
/// subdirectory named `agent/` is reference/prose material staged into a
/// scratch directory for an AI-agent workflow to read (via
/// `service.agent.files`) -- never itself a workflow definition. It has no
/// YAML frontmatter (it is plain prose), so without this exclusion the
/// loader's recursive scan would report it as a broken workflow
/// (`LoadError::NoFrontmatter`) on every load -- exactly the
/// "mistakenly discoverable via `orchestrator list`" failure mode quick
/// task 260807-s3i already fixed once for `get_time.md`, recurring here for
/// a companion doc instead of a stub handler. Mirrors why
/// `workflows/scripts/*.sh` companion files never collide with the scan:
/// there they escape it via extension; here, via the well-known `agent/`
/// directory name.
#[test]
fn md_file_directly_inside_an_agent_subdirectory_is_never_scanned_as_a_workflow() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "agent/some_reference.md",
        "# Reference material\n\nJust prose, no frontmatter at all.\n",
    );
    write_workflow(
        dir.path(),
        "real_workflow.md",
        r#"---
id: real_workflow
name: Real Workflow
service:
  type: action
  handler: demo.real
---
A genuine workflow, sibling to the excluded `agent/` directory.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(
        errors.is_empty(),
        "expected the agent/ subdirectory's non-frontmatter .md to be silently excluded, not reported as a load error: {errors:?}"
    );
    assert!(
        registry.lookup("real_workflow").is_some(),
        "expected the sibling real workflow to still load normally"
    );
}

#[test]
fn nonexistent_scan_directory_produces_an_io_error() {
    let (registry, errors) = Registry::load("/definitely/does/not/exist");

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    let message = errors[0].to_string();
    assert!(
        message.contains("IO error"),
        "expected an IO error for the missing scan directory, got: {message}"
    );
}

#[cfg(unix)]
#[test]
fn unreadable_subdirectory_produces_an_io_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().expect("failed to create tempdir");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("failed to create subdir");
    write_workflow(
        &locked,
        "hidden.md",
        r#"---
id: hidden
name: Hidden Workflow
service:
  type: action
  handler: demo.hidden
---
Lives inside an unreadable subdirectory.
"#,
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
        .expect("failed to lock subdir");

    let (registry, errors) = Registry::load(dir.path());

    // Restore permissions so TempDir cleanup can remove the fixture.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))
        .expect("failed to unlock subdir");

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    let message = errors[0].to_string();
    assert!(
        message.contains("IO error"),
        "expected an IO error for the unreadable subdirectory, got: {message}"
    );
}

#[test]
fn workflow_with_absent_service_mode_defaults_to_sync() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "no_mode.md",
        r#"---
id: no_mode
name: No Mode Workflow
service:
  type: action
  handler: demo.no_mode
---
Has no service.mode field at all (D-10 non-breaking default).
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("no_mode")
        .expect("lookup(\"no_mode\") should return the loaded definition");
    assert_eq!(
        def.service.mode,
        ServiceMode::Sync,
        "expected an absent service.mode to default to Sync"
    );
}

#[test]
fn workflow_declaring_service_mode_async_resolves_to_async() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "async_mode.md",
        r#"---
id: async_mode
name: Async Mode Workflow
service:
  type: action
  handler: demo.async_mode
  mode: async
---
Declares service.mode: async explicitly.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("async_mode")
        .expect("lookup(\"async_mode\") should return the loaded definition");
    assert_eq!(
        def.service.mode,
        ServiceMode::Async,
        "expected service.mode: async to resolve to ServiceMode::Async"
    );
}

#[test]
fn workflow_declaring_unknown_service_mode_is_rejected_naming_the_value() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "bogus_mode.md",
        r#"---
id: bogus_mode
name: Bogus Mode Workflow
service:
  type: action
  handler: demo.bogus_mode
  mode: bogus
---
Declares an unrecognized service.mode value.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    match &errors[0] {
        LoadError::UnknownServiceMode { value, .. } => {
            assert_eq!(value, "bogus", "expected the offending value to be named");
        }
        other => panic!("expected UnknownServiceMode, got: {other:?}"),
    }
    let message = errors[0].to_string();
    assert!(
        message.contains("bogus"),
        "expected error message to name the offending value `bogus`, got: {message}"
    );
}

#[test]
fn real_workflows_directory_loads_with_zero_errors_and_all_four_ids_present() {
    let workflows_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows");

    let (registry, errors) = Registry::load(&workflows_dir);

    assert!(
        errors.is_empty(),
        "expected the committed workflows/ dir to load with zero errors: {errors:?}"
    );
    let ids: Vec<String> = registry.enumerate().into_iter().map(|s| s.id).collect();
    // ai_summarize (06-05, SVC-03): the first real AI-agent workflow --
    // its dependency `workflows/agent/ai_summarize_reference.md` lives in
    // a nested subdirectory that must NOT itself be scanned as a workflow
    // (it has no `.md` frontmatter that would satisfy the loader anyway,
    // but this asserts the real directory tree stays load-error-free).
    for expected_id in ["set_timer", "calendar_today", "countdown", "ai_summarize"] {
        assert!(
            ids.contains(&expected_id.to_string()),
            "expected {expected_id} to be loaded, got ids: {ids:?}"
        );
    }

    let removed_stub_id = "get_time";
    assert!(
        !ids.contains(&removed_stub_id.to_string()),
        "expected {removed_stub_id} to be absent -- it was an unimplemented stub whose handler was never registered, got ids: {ids:?}"
    );
}

#[test]
fn loaded_definition_source_path_file_name_matches_the_fixture_file_it_was_written_as() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "srclog.md",
        r#"---
id: srclog
name: Source Log Workflow
service:
  type: action
  handler: demo.srclog
---
Used to assert source_path tracks the scanned file.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("srclog")
        .expect("lookup(\"srclog\") should return the loaded definition");
    assert_eq!(
        def.source_path.file_name().and_then(|n| n.to_str()),
        Some("srclog.md"),
        "expected source_path's file name to equal the fixture file name it was written as"
    );
}

#[test]
fn zero_parameter_workflow_with_absent_parameters_key_loads_successfully() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "no_params_key.md",
        r#"---
id: no_params_key
name: No Params Key Workflow
service:
  type: action
  handler: demo.no_params
---
The parameters key is entirely absent.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("no_params_key")
        .expect("lookup(\"no_params_key\") should return the loaded definition");
    assert!(def.parameters.is_empty(), "expected an empty parameter map");
}

#[test]
fn service_command_with_no_args_loads_into_service_spec() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "with_command.md",
        r#"---
id: with_command
name: With Command Workflow
service:
  type: action
  handler: demo.with_command
  command: workflows/scripts/foo.sh
---
Declares service.command with no args.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("with_command")
        .expect("lookup(\"with_command\") should return the loaded definition");
    assert_eq!(
        def.service.command,
        Some(dir.path().join("workflows/scripts/foo.sh")),
        "expected service.command to parse into Some(PathBuf(..)) anchored to the workflow \
         file's own directory"
    );
    assert_eq!(
        def.service.args,
        Vec::<String>::new(),
        "expected service.args to default to empty when absent"
    );
}

#[test]
fn service_command_with_args_loads_args_in_order() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "with_command_args.md",
        r#"---
id: with_command_args
name: With Command Args Workflow
service:
  type: action
  handler: demo.with_command_args
  command: workflows/scripts/foo.sh
  args:
    - "--flag"
    - "value"
---
Declares service.command with a two-element args list.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("with_command_args")
        .expect("lookup(\"with_command_args\") should return the loaded definition");
    assert_eq!(
        def.service.command,
        Some(dir.path().join("workflows/scripts/foo.sh")),
        "expected service.command to parse into Some(PathBuf(..)) anchored to the workflow \
         file's own directory"
    );
    assert_eq!(
        def.service.args,
        vec!["--flag".to_string(), "value".to_string()],
        "expected service.args to preserve declared order"
    );
}

#[test]
fn service_without_command_loads_with_none() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "no_command.md",
        r#"---
id: no_command
name: No Command Workflow
service:
  type: action
  handler: demo.no_command
---
Has no service.command key at all.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("no_command")
        .expect("lookup(\"no_command\") should return the loaded definition");
    assert_eq!(
        def.service.command, None,
        "expected an absent service.command to load as None"
    );
    assert_eq!(
        def.service.args,
        Vec::<String>::new(),
        "expected service.args to be empty when command is absent"
    );
}

#[test]
fn relative_service_command_is_anchored_to_the_workflow_files_own_directory() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "nested/deep.md",
        r#"---
id: nested_command
name: Nested Command Workflow
service:
  type: action
  handler: demo.nested_command
  command: scripts/foo.sh
---
Declares a relative service.command from a nested workflow file.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("nested_command")
        .expect("lookup(\"nested_command\") should return the loaded definition");

    let expected = dir.path().join("nested/scripts/foo.sh");
    assert_eq!(
        def.service.command,
        Some(expected.clone()),
        "expected relative service.command to be anchored to the workflow file's own \
         directory (nested/), got: {:?}",
        def.service.command
    );
    assert!(
        def.service
            .command
            .as_ref()
            .expect("service.command present")
            .starts_with(dir.path()),
        "expected resolved service.command to start with the TempDir root {:?}, got: {:?} \
         (this fails if the command is still the bare unanchored relative path)",
        dir.path(),
        def.service.command
    );
}

#[test]
fn real_countdown_workflow_resolves_a_service_command_that_exists_on_disk() {
    let workflows_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows");

    let (registry, errors) = Registry::load(&workflows_dir);
    assert!(
        errors.is_empty(),
        "expected the committed workflows/ dir to load with zero errors: {errors:?}"
    );

    let def = registry
        .lookup("countdown")
        .expect("lookup(\"countdown\") should return the loaded definition");
    let command = def
        .service
        .command
        .as_ref()
        .expect("countdown.md declares service.command");

    assert!(
        command.exists(),
        "expected countdown's resolved service.command to exist on disk regardless of the \
         loading process's CWD, got: {command:?}"
    );
}

#[test]
fn absolute_service_command_is_stored_unchanged_never_double_joined() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "with_absolute_command.md",
        r#"---
id: absolute_command
name: Absolute Command Workflow
service:
  type: action
  handler: demo.absolute_command
  command: /usr/bin/true
---
Declares an absolute service.command.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("absolute_command")
        .expect("lookup(\"absolute_command\") should return the loaded definition");
    assert_eq!(
        def.service.command,
        Some(PathBuf::from("/usr/bin/true")),
        "expected an absolute service.command to be stored byte-identical, got: {:?}",
        def.service.command
    );
}

// --- 06-01 SVC-03: `service.agent` loading (AI-agent handler binding) ---

#[test]
fn empty_agent_block_loads_as_default_agent_spec() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "agent_minimal.md",
        r#"---
id: agent_minimal
name: Agent Minimal Workflow
service:
  handler: agent.claude
  agent: {}
---
An agent workflow with no declared options.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("agent_minimal")
        .expect("lookup(\"agent_minimal\") should return the loaded definition");

    assert_eq!(
        def.service.agent,
        Some(AgentSpec::default()),
        "an empty `service.agent: {{}}` block should load as AgentSpec::default(), got: {:?}",
        def.service.agent
    );
}

#[test]
fn agent_files_relative_entry_anchors_to_workflow_directory_absolute_entry_stored_verbatim() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "sub/agent_files.md",
        r#"---
id: agent_files
name: Agent Files Workflow
service:
  handler: agent.claude
  agent:
    files:
      - reference.md
      - /etc/hosts
---
An agent workflow declaring one relative and one absolute file dependency.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry
        .lookup("agent_files")
        .expect("lookup(\"agent_files\") should return the loaded definition");
    let agent = def
        .service
        .agent
        .as_ref()
        .expect("service.agent should be Some for a workflow declaring an agent block");

    assert_eq!(
        agent.files,
        vec![
            dir.path().join("sub").join("reference.md"),
            PathBuf::from("/etc/hosts"),
        ],
        "expected the relative entry anchored to the workflow's own directory and the absolute entry stored verbatim, got: {:?}",
        agent.files
    );
}

#[test]
fn workflow_declaring_both_command_and_agent_is_rejected() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "conflicting.md",
        r#"---
id: conflicting
name: Conflicting Workflow
service:
  handler: demo.conflicting
  command: ./run.sh
  agent: {}
---
Declares both service.command and service.agent -- must be rejected.
"#,
    );

    let (registry, errors) = Registry::load(dir.path());

    assert_eq!(registry.enumerate().len(), 0, "no workflow should have loaded");
    assert_eq!(errors.len(), 1, "expected exactly one load error: {errors:?}");
    match &errors[0] {
        LoadError::ConflictingServiceBinding { path } => {
            assert!(
                path.ends_with("conflicting.md"),
                "expected the error to name the offending file, got: {path:?}"
            );
        }
        other => panic!("expected ConflictingServiceBinding, got: {other:?}"),
    }
    let message = errors[0].to_string();
    assert!(
        message.contains("service.command") && message.contains("service.agent"),
        "expected the error message to name both keys, got: {message}"
    );
}
