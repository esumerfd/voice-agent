//! Integration tests for `orchestrator workflow edit` (quick task
//! 260812-qpn, Task 2). Mirrors `create_integration.rs`'s `#[path]`
//! module-inclusion trick (no `[lib]` target for `orchestrator-cli`) and its
//! `start_test_daemon`/compiled-binary invocation shapes.

#[path = "../src/create.rs"]
mod create;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::fs;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use shared::{OrchestratorClient, WorkflowCreator, WorkflowWriteMode};
use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::{InProcessOrchestrator, Service};

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

// ---- end-to-end (direct create::run in edit mode against an in-process daemon) ----

#[tokio::test]
async fn direct_edit_of_an_unknown_id_exits_nonzero_and_names_the_id() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let mut buf = Vec::new();
    let code = create::run(
        client,
        WorkflowWriteMode::Edit,
        "never_created_qpn",
        "new prose",
        None,
        None,
        &[],
        false,
        true,
        &create::AgentFlags::default(),
        &mut buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(code, 1, "expected a nonzero exit for an edit of an unknown id, got output: {}", String::from_utf8_lossy(&buf));

    let rendered: serde_json::Value =
        serde_json::from_slice(&buf).expect("expected create::run to render valid JSON");
    let error = rendered["error"].as_str().expect("expected an error string");
    assert!(
        error.contains("never_created_qpn"),
        "expected the error to name the unknown id, got: {error}"
    );
}

#[tokio::test]
async fn direct_edit_replaces_the_definition_and_reports_it_as_updated() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let mut create_buf = Vec::new();
    let create_code = create::run(
        client,
        WorkflowWriteMode::Create,
        "edit_me_qpn",
        "original prose",
        None,
        None,
        &[],
        false,
        true,
        &create::AgentFlags::default(),
        &mut create_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(create_code, 0, "expected the fixture create to succeed");

    let mut edit_buf = Vec::new();
    let edit_code = create::run(
        client,
        WorkflowWriteMode::Edit,
        "edit_me_qpn",
        "edited prose",
        Some("Edited Display Name"),
        None,
        &[],
        false,
        true,
        &create::AgentFlags::default(),
        &mut edit_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(edit_code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&edit_buf));

    let rendered: serde_json::Value =
        serde_json::from_slice(&edit_buf).expect("expected create::run to render valid JSON");
    assert_eq!(
        rendered["updated"],
        serde_json::json!(true),
        "expected the edit-mode success key `updated: true` (not `created`), got: {rendered}"
    );
    assert!(rendered["workflow_path"].is_string(), "expected a populated workflow_path, got: {rendered}");

    let (registry, errors) = orchestrator::Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("edit_me_qpn").expect("expected the edited workflow to resolve");
    assert_eq!(def.name, "Edited Display Name");
    assert_eq!(def.description, "edited prose");
}

#[tokio::test]
async fn edit_is_visible_to_the_same_live_daemon_without_a_restart() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");

    let mut create_buf = Vec::new();
    let create_code = create::run(
        &client as &dyn WorkflowCreator,
        WorkflowWriteMode::Create,
        "live_edit_qpn",
        "original prose",
        None,
        None,
        &[],
        false,
        true,
        &create::AgentFlags::default(),
        &mut create_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(create_code, 0, "expected the fixture create to succeed");

    let mut edit_buf = Vec::new();
    let edit_code = create::run(
        &client as &dyn WorkflowCreator,
        WorkflowWriteMode::Edit,
        "live_edit_qpn",
        "new prose",
        Some("Live Edited Name"),
        None,
        &[],
        false,
        true,
        &create::AgentFlags::default(),
        &mut edit_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(edit_code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&edit_buf));

    let list_resp = (&client as &dyn OrchestratorClient)
        .list_workflows(shared::ListWorkflowsRequest {})
        .await;
    let found = list_resp
        .workflows
        .iter()
        .find(|w| w.id == "live_edit_qpn")
        .expect("expected the edited workflow to still be listable over the SAME connection");
    assert_eq!(
        found.name, "Live Edited Name",
        "expected list_workflows over the same connection to show the NEW name with no daemon restart (D-6)"
    );
}

#[tokio::test]
async fn direct_edit_of_a_script_workflow_replaces_the_script_file() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowCreator = &client;

    let mut create_buf = Vec::new();
    let create_code = create::run(
        client,
        WorkflowWriteMode::Create,
        "edit_script_qpn",
        "#!/bin/sh\necho old\n",
        None,
        None,
        &[],
        false,
        false,
        &create::AgentFlags::default(),
        &mut create_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(create_code, 0, "expected the fixture create to succeed");

    let mut edit_buf = Vec::new();
    let edit_code = create::run(
        client,
        WorkflowWriteMode::Edit,
        "edit_script_qpn",
        "#!/bin/sh\necho new\n",
        None,
        None,
        &[],
        false,
        false,
        &create::AgentFlags::default(),
        &mut edit_buf,
    )
    .await
    .expect("create::run should not error");
    assert_eq!(edit_code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&edit_buf));

    let script_path = dir.path().join("scripts").join("edit_script_qpn.sh");
    let contents = fs::read_to_string(&script_path).expect("read script");
    assert_eq!(contents, "#!/bin/sh\necho new\n", "expected the daemon-side script to hold the new content");
}

// ---- end-to-end through the compiled binary ----

#[tokio::test]
async fn compiled_binary_edit_then_list_shows_the_updated_name() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let create_output =
        run_orchestrator(&["--port", &port_str, "workflow", "create", "cli_edit_qpn", "Some prose."]).await;
    assert!(create_output.status.success(), "expected the fixture create to succeed");

    let edit_output = run_orchestrator(&[
        "--port",
        &port_str,
        "workflow",
        "edit",
        "cli_edit_qpn",
        "Updated prose.",
        "--display-name",
        "Updated Name",
    ])
    .await;
    assert!(
        edit_output.status.success(),
        "expected exit 0, got status {:?}, stdout: {}, stderr: {}",
        edit_output.status,
        String::from_utf8_lossy(&edit_output.stdout),
        String::from_utf8_lossy(&edit_output.stderr)
    );

    let list_output = run_orchestrator(&["--port", &port_str, "list"]).await;
    let stdout = String::from_utf8(list_output.stdout).expect("stdout was not valid UTF-8");
    assert!(
        stdout.contains("Updated Name"),
        "expected the updated display name in list output, got: {stdout:?}"
    );
}

#[tokio::test]
async fn compiled_binary_edit_of_an_unknown_id_exits_nonzero() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let edit_output = run_orchestrator(&[
        "--port",
        &port_str,
        "workflow",
        "edit",
        "never_created_cli_qpn",
        "prose",
    ])
    .await;
    assert!(!edit_output.status.success(), "expected an edit of an unknown id to exit nonzero");
    let stdout = String::from_utf8(edit_output.stdout).expect("stdout was not valid UTF-8");
    assert!(
        stdout.contains("never_created_cli_qpn"),
        "expected the error to mention the unknown id, got: {stdout:?}"
    );
}
