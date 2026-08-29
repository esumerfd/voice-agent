//! Integration tests for `WsOrchestratorClient` (D-01/D-06): included via
//! the same `#[path]` trick as `run_integration.rs`/`list_integration.rs`
//! (there is no `[lib]` target for `orchestrator-cli`) so these tests
//! exercise the exact client `main.rs` constructs.
//!
//! `InProcessOrchestrator` appears ONLY in this test file, standing up the
//! in-process WS server the client connects against (D-07 -- never a
//! spawned `orchestratord` subprocess). It is never referenced by
//! `ws_client.rs` itself -- that file is grep-gated to prove the D-06 CLI
//! cutover (Tasks 1-2's `InProcessOrchestrator == 0` acceptance criterion
//! applies to the CLI *source* files only, never test files; see the
//! `04-05-PLAN.md` planner-discipline-allow note).

#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::{InProcessOrchestrator, ListWorkflowsRequest, OrchestratorClient, Service};

use ws_client::WsOrchestratorClient;

/// Writes `contents` to `dir/filename`, creating any intermediate
/// directories the fixture requests. Mirrors `run_integration.rs`.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

/// Binds `orchestrator::server::serve` to an OS-assigned ephemeral loopback
/// port (D-07) over an `InProcessOrchestrator` built from `workflows_dir`,
/// and returns the bound port.
async fn spawn_test_daemon(workflows_dir: &Path) -> u16 {
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

#[tokio::test]
async fn ws_client_list_workflows_returns_the_fixture_over_a_real_ws_connection() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "alpha.md",
        r#"---
id: alpha
name: Alpha Workflow
parameters: {}
service:
  type: action
  handler: demo.alpha
---
Fixture workflow.
"#,
    );
    let port = spawn_test_daemon(dir.path()).await;

    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");
    let response = client.list_workflows(ListWorkflowsRequest::default()).await;

    let ids: Vec<String> = response.workflows.iter().map(|w| w.id.clone()).collect();
    assert!(
        ids.contains(&"alpha".to_string()),
        "expected the fixture workflow `alpha` in the list, got: {ids:?}"
    );
}

#[tokio::test]
async fn ws_client_connect_to_a_dead_port_returns_err() {
    // Bind and drop a listener to reserve a very-likely-free port, then
    // connect to it with nothing listening -- `connect` must return `Err`
    // rather than hang or panic (D-04's precondition for a clear CLI error).
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port to reserve a free one");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    drop(listener);

    let result = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}")).await;
    assert!(
        result.is_err(),
        "expected connect to a dead port to return Err, got Ok"
    );
}
