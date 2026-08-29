//! Integration tests for `invoke_workflow`'s top-level payload shape check
//! (CR-01): a non-object, non-null payload must be rejected with a clear
//! error naming the workflow, never silently coerced to `{}`. JSON `null`
//! stays lenient (Postel) and is treated as an empty parameter map.

use std::fs;
use std::path::Path;

use orchestrator::{InProcessOrchestrator, InvokeStatus, InvokeWorkflowRequest, OrchestratorClient};
use serde_json::json;
use tempfile::TempDir;

/// Writes `contents` to `dir/filename`, creating any intermediate
/// directories the fixture requests. Mirrors `registry_integration.rs`.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

fn demo_workflow_dir() -> TempDir {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(
        dir.path(),
        "demo.md",
        r#"---
id: demo
name: Demo
parameters: {}
service:
  type: action
  handler: demo.handler
---
A demo zero-parameter workflow.
"#,
    );
    dir
}

#[tokio::test]
async fn non_object_payload_is_rejected_with_clear_error() {
    let dir = demo_workflow_dir();
    let orchestrator = InProcessOrchestrator::new(dir.path());

    let resp = orchestrator
        .invoke_workflow(InvokeWorkflowRequest {
            workflow_id: "demo".into(),
            payload: json!(["garbage"]),
        })
        .await;

    assert!(
        matches!(resp.status, InvokeStatus::Failed),
        "expected Failed status, got {:?}",
        resp.status
    );
    assert!(resp.output.is_none(), "expected no output on rejection");
    let error = resp.error.expect("expected an error message on rejection");
    assert!(
        error.contains("must be a JSON object"),
        "expected error to mention the required shape, got: {error}"
    );
    assert!(
        error.contains("demo"),
        "expected error to name the workflow `demo`, got: {error}"
    );
}

#[tokio::test]
async fn null_payload_is_accepted_as_empty() {
    let dir = demo_workflow_dir();
    let orchestrator = InProcessOrchestrator::new(dir.path());

    let resp = orchestrator
        .invoke_workflow(InvokeWorkflowRequest {
            workflow_id: "demo".into(),
            payload: json!(null),
        })
        .await;

    // No handler is registered for `demo.handler`, so dispatch still fails —
    // but the failure must come from HandlerNotFound, never from the
    // top-level shape check, proving null was accepted as an empty map.
    if let Some(error) = &resp.error {
        assert!(
            !error.contains("must be a JSON object"),
            "null payload must not be rejected by the shape check, got: {error}"
        );
    }
}
