//! Integration tests for `--agent` on `orchestrator workflow create`/`edit`
//! (quick task 260813-rm5, Task 2). Mirrors `create_integration.rs`'s
//! `#[path]` module-inclusion trick (no `[lib]` target for
//! `orchestrator-cli`) and its `write_file`/`start_test_daemon`/
//! `run_orchestrator` helpers.

#[path = "../src/create.rs"]
mod create;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use shared::WorkflowCreator;
use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::{InProcessOrchestrator, Service};

fn write_file(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

/// Binds an ephemeral loopback WS server in-process wrapping an
/// `InProcessOrchestrator` built from `workflows_dir` (mirrors
/// `create_integration.rs::start_test_daemon`).
async fn start_test_daemon(workflows_dir: &Path) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let orchestrator = Arc::new(InProcessOrchestrator::with_handlers(workflows_dir, handlers));
    let activity_registry = Arc::new(orchestrator::activity::ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

async fn run_orchestrator(args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        cmd.args(&args);
        cmd.output().expect("failed to run the orchestrator binary")
    })
    .await
    .expect("run_orchestrator's spawn_blocking task panicked")
}

fn no_agent_flags() -> create::AgentFlags {
    create::AgentFlags::default()
}

// ---- pure helper: agent_config_from_flags (no daemon, no filesystem) ----

#[test]
fn no_agent_flag_and_no_agent_options_yields_no_agent_config() {
    let flags = create::AgentFlags::default();
    let config = create::agent_config_from_flags(&flags).expect("should not error");
    assert!(config.is_none(), "expected no agent config when --agent is absent");
}

#[test]
fn agent_flag_alone_yields_empty_file_list_and_absent_numeric_fields() {
    let flags = create::AgentFlags {
        agent: true,
        ..Default::default()
    };
    let config = create::agent_config_from_flags(&flags)
        .expect("should not error")
        .expect("expected Some(AgentConfig)");
    assert!(config.files.is_empty(), "expected an empty files list");
    assert_eq!(config.timeout_secs, None, "expected the daemon default to apply (D-5)");
    assert_eq!(config.max_budget_usd, None, "expected the daemon default to apply (D-5)");
}

#[test]
fn repeated_agent_file_flags_preserve_declared_order_with_no_filesystem_access() {
    let flags = create::AgentFlags {
        agent: true,
        agent_file: vec![
            "agent/first.md".to_string(),
            "/this/path/does/not/exist/on/this/machine.md".to_string(),
            "agent/third.md".to_string(),
        ],
        ..Default::default()
    };
    let config = create::agent_config_from_flags(&flags)
        .expect("should not error")
        .expect("expected Some(AgentConfig)");
    assert_eq!(
        config.files,
        vec![
            "agent/first.md".to_string(),
            "/this/path/does/not/exist/on/this/machine.md".to_string(),
            "agent/third.md".to_string(),
        ],
        "expected declared order preserved verbatim -- a nonexistent local path must survive unchanged (D-3)"
    );
}

#[test]
fn agent_file_without_agent_flag_is_a_descriptive_error_naming_the_flag() {
    let flags = create::AgentFlags {
        agent: false,
        agent_file: vec!["ref.md".to_string()],
        ..Default::default()
    };
    let err = create::agent_config_from_flags(&flags).expect_err("should error");
    assert!(
        err.contains("--agent-file") && err.contains("--agent"),
        "expected the error to name the offending flag and --agent, got: {err}"
    );
}

#[test]
fn timeout_secs_without_agent_flag_is_a_descriptive_error_naming_the_flag() {
    let flags = create::AgentFlags {
        agent: false,
        timeout_secs: Some(30),
        ..Default::default()
    };
    let err = create::agent_config_from_flags(&flags).expect_err("should error");
    assert!(
        err.contains("--timeout-secs") && err.contains("--agent"),
        "expected the error to name the offending flag and --agent, got: {err}"
    );
}

#[test]
fn max_budget_usd_without_agent_flag_is_a_descriptive_error_naming_the_flag() {
    let flags = create::AgentFlags {
        agent: false,
        max_budget_usd: Some(0.25),
        ..Default::default()
    };
    let err = create::agent_config_from_flags(&flags).expect_err("should error");
    assert!(
        err.contains("--max-budget-usd") && err.contains("--agent"),
        "expected the error to name the offending flag and --agent, got: {err}"
    );
}

// ---- end-to-end (direct create::run against an in-process daemon) ----

#[tokio::test]
async fn direct_agent_create_writes_an_agent_definition() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let flags = create::AgentFlags {
        agent: true,
        agent_file: vec!["ref.md".to_string()],
        timeout_secs: Some(120),
        max_budget_usd: Some(0.25),
    };

    let mut buf = Vec::new();
    let code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "direct_agent_rm5",
        "You are an unattended agent. Do the thing.",
        None,
        None,
        &[],
        false,
        false,
        &flags,
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    let rendered: serde_json::Value =
        serde_json::from_slice(&buf).expect("expected create::run to render valid JSON");
    assert_eq!(rendered["created"], serde_json::json!(true), "expected created: true, got: {rendered}");
    assert!(rendered["script_path"].is_null(), "expected a null script_path for an agent create, got: {rendered}");

    let (registry, errors) = orchestrator::Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("direct_agent_rm5").expect("expected the new workflow to resolve");
    assert_eq!(def.service.handler, orchestrator::handlers::ai_agent::AGENT_HANDLER_KEY);
    assert_eq!(def.service.mode, orchestrator::ServiceMode::Async);
    let agent = def.service.agent.as_ref().expect("expected Some(AgentSpec)");
    assert_eq!(agent.timeout_secs, Some(120));
    assert_eq!(agent.max_budget_usd, Some(0.25));
}

#[tokio::test]
async fn agent_create_reads_the_prompt_body_from_a_local_source_file() {
    let dir = TempDir::new().expect("tempdir");
    let src_dir = TempDir::new().expect("tempdir for source fixture");
    write_file(src_dir.path(), "prompt.md", "Summarize the attached reference file.");
    let source_path = src_dir.path().join("prompt.md").to_string_lossy().to_string();

    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let flags = create::AgentFlags {
        agent: true,
        ..Default::default()
    };

    let mut buf = Vec::new();
    let code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "agent_source_file_rm5",
        &source_path,
        None,
        None,
        &[],
        false,
        false,
        &flags,
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    let (registry, _errors) = orchestrator::Registry::load(dir.path());
    let def = registry.lookup("agent_source_file_rm5").expect("expected the new workflow to resolve");
    assert_eq!(def.description, "Summarize the attached reference file.");
}

#[tokio::test]
async fn agent_create_sends_agent_file_paths_verbatim_daemon_side() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let flags = create::AgentFlags {
        agent: true,
        agent_file: vec!["nested/ref.md".to_string()],
        ..Default::default()
    };

    let mut buf = Vec::new();
    let code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "agent_verbatim_rm5",
        "Prompt body.",
        None,
        None,
        &[],
        false,
        false,
        &flags,
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    let (registry, _errors) = orchestrator::Registry::load(dir.path());
    let def = registry.lookup("agent_verbatim_rm5").expect("expected the new workflow to resolve");
    let agent = def.service.agent.as_ref().expect("expected Some(AgentSpec)");
    let resolved = &agent.files[0];
    assert!(
        resolved.starts_with(dir.path()),
        "expected the agent-file entry to resolve against the DAEMON's workflows dir ({:?}), got: {resolved:?}",
        dir.path()
    );
    assert!(
        resolved.ends_with("nested/ref.md"),
        "expected the relative entry preserved under the daemon dir, got: {resolved:?}"
    );
}

#[tokio::test]
async fn agent_create_with_a_description_override_uses_it_as_the_prompt_body() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let flags = create::AgentFlags {
        agent: true,
        ..Default::default()
    };

    let mut buf = Vec::new();
    let code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "agent_desc_override_rm5",
        "this source content should be overridden",
        None,
        Some("This is the real prompt body."),
        &[],
        false,
        false,
        &flags,
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    let (registry, _errors) = orchestrator::Registry::load(dir.path());
    let def = registry.lookup("agent_desc_override_rm5").expect("expected the new workflow to resolve");
    assert_eq!(def.description, "This is the real prompt body.");
}

#[tokio::test]
async fn direct_agent_edit_converts_an_action_workflow_to_agent_and_back() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let mut create_buf = Vec::new();
    let create_code = create::run(
        client,
        shared::WorkflowWriteMode::Create,
        "agent_convert_rm5",
        "original prose",
        None,
        None,
        &[],
        false,
        true,
        &no_agent_flags(),
        &mut create_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(create_code, 0, "expected the fixture create to succeed");

    let agent_flags = create::AgentFlags {
        agent: true,
        ..Default::default()
    };
    let mut edit_buf = Vec::new();
    let edit_code = create::run(
        client,
        shared::WorkflowWriteMode::Edit,
        "agent_convert_rm5",
        "agent prompt body",
        None,
        None,
        &[],
        false,
        false,
        &agent_flags,
        &mut edit_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(edit_code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&edit_buf));

    let (registry, _errors) = orchestrator::Registry::load(dir.path());
    let def = registry.lookup("agent_convert_rm5").expect("expected the workflow to resolve");
    assert!(def.service.agent.is_some(), "expected agent-typed after the edit");

    let mut back_buf = Vec::new();
    let back_code = create::run(
        client,
        shared::WorkflowWriteMode::Edit,
        "agent_convert_rm5",
        "back to markdown",
        None,
        None,
        &[],
        false,
        true,
        &no_agent_flags(),
        &mut back_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(back_code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&back_buf));

    let (registry, _errors) = orchestrator::Registry::load(dir.path());
    let def = registry.lookup("agent_convert_rm5").expect("expected the workflow to resolve");
    assert!(def.service.agent.is_none(), "expected action-typed after converting back");
}

// ---- end-to-end through the compiled binary ----

#[tokio::test]
async fn compiled_binary_agent_create_then_list_shows_the_workflow() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let create_output = run_orchestrator(&[
        "--port",
        &port_str,
        "workflow",
        "create",
        "cli_agent_rm5",
        "Prompt body for the compiled-binary test.",
        "--agent",
        "--agent-file",
        "ref.md",
        "--timeout-secs",
        "60",
        "--max-budget-usd",
        "0.10",
    ])
    .await;
    assert!(
        create_output.status.success(),
        "expected exit 0, got status {:?}, stdout: {}, stderr: {}",
        create_output.status,
        String::from_utf8_lossy(&create_output.stdout),
        String::from_utf8_lossy(&create_output.stderr)
    );

    let list_output = run_orchestrator(&["--port", &port_str, "list"]).await;
    let stdout = String::from_utf8(list_output.stdout).expect("stdout was not valid UTF-8");
    assert!(stdout.contains("cli_agent_rm5"), "expected the new workflow id in list output, got: {stdout:?}");
}

#[tokio::test]
async fn compiled_binary_rejects_agent_combined_with_script() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let output = run_orchestrator(&[
        "--port",
        &port_str,
        "workflow",
        "create",
        "cli_agent_conflict_script_rm5",
        "some prose",
        "--agent",
        "--script",
    ])
    .await;
    assert!(!output.status.success(), "expected a nonzero exit for --agent combined with --script");
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    assert!(
        stderr.contains("--agent") && stderr.contains("--script"),
        "expected clap's conflict message naming both flags, got: {stderr:?}"
    );
}

#[tokio::test]
async fn compiled_binary_rejects_agent_combined_with_markdown() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let output = run_orchestrator(&[
        "--port",
        &port_str,
        "workflow",
        "create",
        "cli_agent_conflict_markdown_rm5",
        "some prose",
        "--agent",
        "--markdown",
    ])
    .await;
    assert!(!output.status.success(), "expected a nonzero exit for --agent combined with --markdown");
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    assert!(
        stderr.contains("--agent") && stderr.contains("--markdown"),
        "expected clap's conflict message naming both flags, got: {stderr:?}"
    );
}

#[tokio::test]
async fn compiled_binary_agent_edit_of_an_unknown_id_still_exits_nonzero() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let output = run_orchestrator(&[
        "--port",
        &port_str,
        "workflow",
        "edit",
        "never_created_agent_rm5",
        "prompt body",
        "--agent",
    ])
    .await;
    assert!(!output.status.success(), "expected an edit of an unknown id to exit nonzero even with --agent");
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    assert!(
        stdout.contains("never_created_agent_rm5"),
        "expected the error to mention the unknown id, got: {stdout:?}"
    );
}
