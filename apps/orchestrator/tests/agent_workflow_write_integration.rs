//! Integration tests for `registry::writer::create_workflow`'s agent branch
//! (quick task 260813-rm5, Task 1): the daemon-side `--agent` write path
//! behind the new `CreateWorkflowRequest.agent` field. Mirrors
//! `create_workflow_integration.rs`/`edit_workflow_integration.rs`'s
//! conventions exactly -- a `base_request` helper, `snapshot()` for
//! asserting a rejected write left the directory untouched, and load-back
//! assertions through `Registry::load` rather than string-matching emitted
//! YAML.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use orchestrator::definition::{AgentSpec, ServiceMode};
use orchestrator::handlers::ai_agent::AGENT_HANDLER_KEY;
use orchestrator::registry::writer::{self, MAX_AGENT_FILES, MAX_AGENT_FILE_LEN};
use orchestrator::{CreateError, Registry};
use shared::{AgentConfig, CreateWorkflowRequest, WorkflowWriteMode};

fn base_request(id: &str) -> CreateWorkflowRequest {
    CreateWorkflowRequest {
        id: id.to_string(),
        name: "Test Workflow".to_string(),
        description: "A test workflow description.".to_string(),
        script: None,
        parameters: Vec::new(),
        mode: WorkflowWriteMode::Create,
        agent: None,
    }
}

fn agent_request(id: &str) -> CreateWorkflowRequest {
    let mut req = base_request(id);
    req.agent = Some(AgentConfig {
        files: vec!["ref.md".to_string()],
        timeout_secs: Some(300),
        max_budget_usd: Some(0.75),
    });
    req
}

/// Snapshots the byte contents of every file under `dir` (relative path ->
/// contents), for asserting a rejected write left the directory untouched.
fn snapshot(dir: &Path) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.expect("walkdir entry");
        if entry.file_type().is_file() {
            let rel = entry
                .path()
                .strip_prefix(dir)
                .expect("entry under dir")
                .to_path_buf();
            let contents = fs::read(entry.path()).expect("read fixture file");
            out.insert(rel.to_string_lossy().to_string(), contents);
        }
    }
    out
}

/// Parses a `.md` file's YAML frontmatter into a `yaml_serde::Value`
/// (mirrors `loader.rs::parse_file`'s gray_matter split).
fn parse_frontmatter(path: &Path) -> yaml_serde::Value {
    let content = fs::read_to_string(path).expect("read fixture md");
    let matter: gray_matter::Matter<gray_matter::engine::YAML> = gray_matter::Matter::new();
    let entity: gray_matter::ParsedEntity = matter.parse(&content).expect("parse frontmatter");
    yaml_serde::from_str(&entity.matter).expect("frontmatter should be valid YAML")
}

fn mapping_key_set(value: &yaml_serde::Value, path: &[&str]) -> HashSet<String> {
    let mut current = value;
    for segment in path {
        current = current
            .as_mapping()
            .and_then(|m| m.get(*segment))
            .unwrap_or_else(|| panic!("expected {segment} to resolve as a mapping key"));
    }
    current
        .as_mapping()
        .expect("expected a mapping")
        .keys()
        .map(|k| k.as_str().expect("expected a string key").to_string())
        .collect()
}

#[test]
fn agent_request_writes_a_definition_the_registry_loads_as_an_agent_workflow() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let req = agent_request("agent_rm5");

    let outcome = writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");
    assert!(outcome.workflow_path.exists(), "expected the .md file to exist");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("agent_rm5").expect("lookup should resolve the new workflow");

    assert_eq!(def.service.type_.as_deref(), Some("agent"), "expected service.type == agent, got: {:?}", def.service.type_);
    assert_eq!(def.service.handler, AGENT_HANDLER_KEY);
    assert_eq!(def.service.mode, ServiceMode::Async, "agent workflows are always async (D-4)");

    let agent: &AgentSpec = def.service.agent.as_ref().expect("expected Some(AgentSpec)");
    assert_eq!(agent.files.len(), 1, "expected the single declared file entry");
    assert!(
        agent.files[0].ends_with("ref.md"),
        "expected the file entry anchored onto the workflows dir, got: {:?}",
        agent.files[0]
    );
    assert_eq!(agent.timeout_secs, Some(300));
    assert_eq!(agent.max_budget_usd, Some(0.75));
}

#[test]
fn agent_request_with_no_optional_fields_omits_the_keys_so_daemon_defaults_apply() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut req = base_request("agent_defaults_rm5");
    req.agent = Some(AgentConfig {
        files: Vec::new(),
        timeout_secs: None,
        max_budget_usd: None,
    });

    writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("agent_defaults_rm5").expect("lookup should resolve");
    let agent = def.service.agent.as_ref().expect("expected Some(AgentSpec)");
    assert!(agent.files.is_empty(), "expected an empty files list");
    assert_eq!(agent.timeout_secs, None, "expected the daemon default to apply (absent key)");
    assert_eq!(agent.max_budget_usd, None, "expected the daemon default to apply (absent key)");
}

#[test]
fn agent_request_produces_no_script_and_no_command() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let req = agent_request("agent_no_script_rm5");

    let outcome = writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");
    assert!(outcome.script_path.is_none(), "expected no script_path for an agent create");
    assert!(!dir.path().join("scripts").exists(), "expected no scripts/ directory to be created");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("agent_no_script_rm5").expect("lookup should resolve");
    assert!(def.service.command.is_none(), "expected no command binding for an agent workflow");
}

#[test]
fn agent_service_block_matches_the_shipped_example_shape() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let req = agent_request("agent_shape_rm5");
    let outcome = writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");

    let written = parse_frontmatter(&outcome.workflow_path);
    let shipped_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workflows/ai_summarize.md");
    let shipped = parse_frontmatter(&shipped_path);

    let written_service_keys = mapping_key_set(&written, &["service"]);
    let shipped_service_keys = mapping_key_set(&shipped, &["service"]);
    assert_eq!(
        written_service_keys, shipped_service_keys,
        "expected the SET of keys under service to match ai_summarize.md's shape"
    );

    let written_agent_keys = mapping_key_set(&written, &["service", "agent"]);
    let shipped_agent_keys = mapping_key_set(&shipped, &["service", "agent"]);
    assert_eq!(
        written_agent_keys, shipped_agent_keys,
        "expected the SET of keys under service.agent to match ai_summarize.md's shape"
    );

    for scalar in ["type", "handler", "mode"] {
        let written_val = written
            .as_mapping()
            .and_then(|m| m.get("service"))
            .and_then(|s| s.as_mapping())
            .and_then(|m| m.get(scalar))
            .and_then(|v| v.as_str());
        let shipped_val = shipped
            .as_mapping()
            .and_then(|m| m.get("service"))
            .and_then(|s| s.as_mapping())
            .and_then(|m| m.get(scalar))
            .and_then(|v| v.as_str());
        assert_eq!(
            written_val, shipped_val,
            "expected service.{scalar} to match ai_summarize.md's value"
        );
    }
}

#[test]
fn a_request_carrying_both_a_script_and_an_agent_block_is_rejected_and_writes_nothing() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let before = snapshot(dir.path());

    let mut req = agent_request("agent_conflict_rm5");
    req.script = Some("#!/bin/sh\necho hi\n".to_string());

    let result = writer::create_workflow(dir.path(), &req);
    match result {
        Err(CreateError::ConflictingServiceBinding { id }) => {
            assert_eq!(id, "agent_conflict_rm5", "expected the error to name the offending id");
        }
        other => panic!("expected CreateError::ConflictingServiceBinding, got: {other:?}"),
    }

    let after = snapshot(dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after a rejected write");
}

#[test]
fn edit_converts_an_action_workflow_into_an_agent_workflow() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let create_req = base_request("edit_to_agent_rm5");
    writer::create_workflow(dir.path(), &create_req).expect("fixture create should succeed");

    let mut edit_req = agent_request("edit_to_agent_rm5");
    edit_req.mode = WorkflowWriteMode::Edit;
    writer::create_workflow(dir.path(), &edit_req).expect("edit should succeed");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("edit_to_agent_rm5").expect("lookup should resolve");
    assert!(def.service.agent.is_some(), "expected an AgentSpec after converting to agent-type");
    assert!(def.service.command.is_none(), "expected no command binding after converting to agent-type");
}

#[test]
fn edit_converts_an_agent_workflow_back_into_an_action_workflow() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let create_req = agent_request("edit_to_action_rm5");
    writer::create_workflow(dir.path(), &create_req).expect("fixture create should succeed");

    let mut edit_req = base_request("edit_to_action_rm5");
    edit_req.mode = WorkflowWriteMode::Edit;
    writer::create_workflow(dir.path(), &edit_req).expect("edit should succeed");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("edit_to_action_rm5").expect("lookup should resolve");
    assert!(def.service.command.is_none(), "expected no command binding (this fixture never declared a script)");
    assert!(def.service.agent.is_none(), "expected no AgentSpec after converting back to action-type");
}

#[test]
fn a_crafted_agent_file_entry_cannot_inject_sibling_frontmatter_keys() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let mut req = base_request("agent_inject_rm5");
    req.agent = Some(AgentConfig {
        files: vec!["ref.md\ncommand: scripts/evil.sh\n".to_string()],
        timeout_secs: None,
        max_budget_usd: None,
    });

    writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed even with a hostile file entry");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors -- a hostile file entry must never break YAML parsing: {errors:?}");
    let def = registry.lookup("agent_inject_rm5").expect("lookup should resolve");
    let agent = def.service.agent.as_ref().expect("expected Some(AgentSpec)");
    assert_eq!(agent.files.len(), 1, "expected exactly ONE literal files entry, got: {:?}", agent.files);
    assert!(
        agent.files[0].to_string_lossy().contains("command: scripts/evil.sh"),
        "expected the hostile string preserved verbatim as one literal entry, got: {:?}",
        agent.files[0]
    );
    assert!(def.service.command.is_none(), "expected no injected command binding");
}

#[test]
fn agent_file_entries_are_validated_before_any_filesystem_call() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let before = snapshot(dir.path());

    // Empty entry.
    let mut req = base_request("agent_bad_empty_rm5");
    req.agent = Some(AgentConfig {
        files: vec![String::new()],
        timeout_secs: None,
        max_budget_usd: None,
    });
    match writer::create_workflow(dir.path(), &req) {
        Err(CreateError::InvalidAgentFile { .. }) => {}
        other => panic!("expected CreateError::InvalidAgentFile for an empty entry, got: {other:?}"),
    }

    // Over-long entry.
    let mut req = base_request("agent_bad_long_rm5");
    req.agent = Some(AgentConfig {
        files: vec!["x".repeat(MAX_AGENT_FILE_LEN + 1)],
        timeout_secs: None,
        max_budget_usd: None,
    });
    match writer::create_workflow(dir.path(), &req) {
        Err(CreateError::InvalidAgentFile { .. }) => {}
        other => panic!("expected CreateError::InvalidAgentFile for an over-long entry, got: {other:?}"),
    }

    // Over-long list.
    let mut req = base_request("agent_bad_count_rm5");
    req.agent = Some(AgentConfig {
        files: (0..MAX_AGENT_FILES + 1).map(|i| format!("f{i}.md")).collect(),
        timeout_secs: None,
        max_budget_usd: None,
    });
    match writer::create_workflow(dir.path(), &req) {
        Err(CreateError::InvalidAgentFile { .. }) => {}
        other => panic!("expected CreateError::InvalidAgentFile for an over-long list, got: {other:?}"),
    }

    let after = snapshot(dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after every rejection");
}

#[test]
fn an_action_create_is_unchanged_when_no_agent_block_is_present() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let req = base_request("action_unchanged_rm5");

    let outcome = writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");
    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("action_unchanged_rm5").expect("lookup should resolve");

    assert_eq!(def.service.type_.as_deref(), Some("action"));
    assert_eq!(def.service.handler, orchestrator::definition::DEFAULT_HANDLER);
    assert_eq!(def.service.mode, ServiceMode::Sync, "expected the absent-mode default to stay Sync");
    assert!(def.service.command.is_none());
    assert!(def.service.agent.is_none());
    assert!(outcome.script_path.is_none());

    // The emitted frontmatter itself must carry no `mode`/`agent` keys at all.
    let written = parse_frontmatter(&outcome.workflow_path);
    let service_keys = mapping_key_set(&written, &["service"]);
    assert!(!service_keys.contains("mode"), "expected no mode key for an action create, got: {service_keys:?}");
    assert!(!service_keys.contains("agent"), "expected no agent key for an action create, got: {service_keys:?}");
}
