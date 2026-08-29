//! Integration tests for `orchestrator workflow delete` (quick task
//! 260812-qp4, Task 1). Mirrors `create_integration.rs`'s `#[path]`
//! module-inclusion trick (no `[lib]` target for `orchestrator-cli`) and its
//! `start_test_daemon`/compiled-binary invocation shapes.

#[path = "../src/delete.rs"]
mod delete;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use shared::{CreateWorkflowRequest, WorkflowDeleter};
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

// ---- end-to-end through the compiled binary ----

#[tokio::test]
async fn compiled_binary_create_then_delete_then_list_omits_the_id() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let create_output =
        run_orchestrator(&["--port", &port_str, "workflow", "create", "qp4_del", "Some prose."]).await;
    assert!(
        create_output.status.success(),
        "expected the create to succeed, got status {:?}, stdout: {}, stderr: {}",
        create_output.status,
        String::from_utf8_lossy(&create_output.stdout),
        String::from_utf8_lossy(&create_output.stderr)
    );

    let list_output = run_orchestrator(&["--port", &port_str, "list"]).await;
    let stdout = String::from_utf8(list_output.stdout).expect("stdout was not valid UTF-8");
    assert!(stdout.contains("qp4_del"), "expected the new workflow id in list output, got: {stdout:?}");

    let delete_output = run_orchestrator(&["--port", &port_str, "workflow", "delete", "qp4_del"]).await;
    assert!(
        delete_output.status.success(),
        "expected exit 0, got status {:?}, stdout: {}, stderr: {}",
        delete_output.status,
        String::from_utf8_lossy(&delete_output.stdout),
        String::from_utf8_lossy(&delete_output.stderr)
    );

    let list_output_after = run_orchestrator(&["--port", &port_str, "list"]).await;
    let stdout_after = String::from_utf8(list_output_after.stdout).expect("stdout was not valid UTF-8");
    assert!(
        !stdout_after.contains("qp4_del"),
        "expected qp4_del to be gone from list after delete (D-3, same never-restarted daemon), got: {stdout_after:?}"
    );
}

#[tokio::test]
async fn compiled_binary_delete_of_unknown_id_exits_nonzero_and_names_the_id() {
    let dir = TempDir::new().expect("tempdir");
    let port = start_test_daemon(dir.path()).await;
    let port_str = port.to_string();

    let delete_output = run_orchestrator(&["--port", &port_str, "workflow", "delete", "never_created_qp4"]).await;
    assert!(!delete_output.status.success(), "expected a delete of an unknown id to exit nonzero");
    let stdout = String::from_utf8(delete_output.stdout).expect("stdout was not valid UTF-8");
    assert!(
        stdout.contains("never_created_qp4"),
        "expected the error to mention the unknown id (D-2), got: {stdout:?}"
    );
}

// ---- end-to-end (direct delete::run against an in-process daemon) ----

#[tokio::test]
async fn direct_delete_removes_the_md_file() {
    let dir = TempDir::new().expect("tempdir");
    let req = CreateWorkflowRequest {
        id: "direct_del_qp4".to_string(),
        name: "Direct Del Qp4".to_string(),
        description: "Some prose for the direct-delete fixture.".to_string(),
        script: None,
        parameters: Vec::new(),
        mode: shared::WorkflowWriteMode::Create,
    agent: None,
    };
    let outcome = orchestrator::registry::writer::create_workflow(dir.path(), &req)
        .expect("fixture create_workflow should succeed");
    assert!(outcome.workflow_path.exists(), "expected the fixture .md to exist before delete");

    let port = start_test_daemon(dir.path()).await;
    let client = ws_client::WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect");
    let client: &dyn WorkflowDeleter = &client;

    let mut buf = Vec::new();
    let code = delete::run(client, "direct_del_qp4", &mut buf)
        .await
        .expect("delete::run should not error");
    assert_eq!(code, 0, "expected exit 0, got output: {}", String::from_utf8_lossy(&buf));

    assert!(
        !outcome.workflow_path.exists(),
        "expected the .md file to no longer exist at {:?}",
        outcome.workflow_path
    );

    let rendered: serde_json::Value =
        serde_json::from_slice(&buf).expect("expected delete::run to render valid JSON");
    assert_eq!(rendered["deleted"], serde_json::json!(true), "expected deleted: true, got: {rendered}");
}
