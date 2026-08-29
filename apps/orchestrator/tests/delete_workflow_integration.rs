//! Integration tests for `registry::writer::delete_workflow` (quick task
//! 260812-qp4, Task 2): companion script removal, containment refusal, and
//! the unparseable-`.md` escape hatch (D-8). Modelled on
//! `create_workflow_integration.rs` -- reuses its `snapshot(dir)` helper for
//! "directory left unchanged" assertions and its raw-envelope `send`/`recv`
//! daemon helpers for the one wire-level test.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use orchestrator::registry::writer;
use orchestrator::{activity::ActivityRegistry, DeleteError, Envelope, InProcessOrchestrator, RequestPayload, ResponsePayload, Service};
use shared::{CreateWorkflowRequest, DeleteWorkflowRequest, ListWorkflowsRequest};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    fs::write(path, contents).expect("failed to write fixture");
}

/// Snapshots the byte contents of every file under `dir` (relative path ->
/// contents), for asserting a rejected/unknown delete left the directory
/// untouched.
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

#[test]
fn script_backed_delete_removes_both_the_md_and_the_companion_script() {
    let dir = TempDir::new().expect("tempdir");
    let req = CreateWorkflowRequest {
        id: "script_del_qp4".to_string(),
        name: "Script Del Qp4".to_string(),
        description: "Says hello.".to_string(),
        script: Some("#!/bin/sh\necho hi\n".to_string()),
        parameters: Vec::new(),
        mode: shared::WorkflowWriteMode::Create,
    agent: None,
    };
    let create_outcome = writer::create_workflow(dir.path(), &req).expect("fixture create should succeed");
    let script_fixture_path = create_outcome.script_path.clone().expect("expected a script_path");
    assert!(create_outcome.workflow_path.exists());
    assert!(script_fixture_path.exists());

    let outcome = writer::delete_workflow(dir.path(), "script_del_qp4").expect("delete_workflow should succeed");

    assert!(!create_outcome.workflow_path.exists(), "expected the .md to be removed");
    assert!(!script_fixture_path.exists(), "expected the companion script to be removed (D-5)");
    assert_eq!(outcome.workflow_path, create_outcome.workflow_path);
    assert_eq!(
        outcome.script_path,
        Some(script_fixture_path),
        "expected DeleteOutcome::script_path to name the removed script"
    );
}

#[test]
fn agent_declared_files_are_left_untouched() {
    let dir = TempDir::new().expect("tempdir");
    write_workflow(dir.path(), "dep.txt", "some declared dependency content\n");
    write_workflow(
        dir.path(),
        "agent_del_qp4.md",
        r#"---
id: agent_del_qp4
name: Agent Del Qp4
service:
  handler: agent.claude
  agent:
    files:
      - dep.txt
---
An agent workflow declaring one file dependency, must be left untouched by delete (D-5).
"#,
    );
    let dep_path = dir.path().join("dep.txt");
    let md_path = dir.path().join("agent_del_qp4.md");
    assert!(dep_path.exists());
    assert!(md_path.exists());

    writer::delete_workflow(dir.path(), "agent_del_qp4").expect("delete_workflow should succeed");

    assert!(!md_path.exists(), "expected the .md to be removed");
    assert!(dep_path.exists(), "expected the declared agent dependency file to be left untouched (D-5)");
}

#[test]
fn md_whose_command_escapes_the_workflows_dir_is_refused_and_nothing_is_removed() {
    let workflows_dir = TempDir::new().expect("workflows tempdir");
    let sibling_dir = TempDir::new().expect("sibling tempdir");
    let sentinel_path = sibling_dir.path().join("sentinel.sh");
    fs::write(&sentinel_path, "#!/bin/sh\necho sentinel\n").expect("write sentinel");

    write_workflow(
        workflows_dir.path(),
        "escape_qp4.md",
        &format!(
            r#"---
id: escape_qp4
name: Escape Qp4
service:
  type: action
  handler: action.immediate
  command: {}
---
A hand-authored workflow whose command escapes the workflows dir (T-qp4-02).
"#,
            sentinel_path.display()
        ),
    );

    let before = snapshot(workflows_dir.path());

    let result = writer::delete_workflow(workflows_dir.path(), "escape_qp4");
    match result {
        Err(DeleteError::PathEscape { .. }) => {}
        other => panic!("expected DeleteError::PathEscape, got: {other:?}"),
    }

    assert!(sentinel_path.exists(), "expected the sentinel file outside workflows_dir to survive");
    let after = snapshot(workflows_dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after the refusal");
}

#[test]
fn unparseable_md_matching_the_id_is_still_deletable() {
    let dir = TempDir::new().expect("tempdir");
    let md_path = dir.path().join("broken_del_qp4.md");
    fs::write(&md_path, "not valid frontmatter at all").expect("write broken fixture");
    let script_path = dir.path().join("scripts").join("broken_del_qp4.sh");
    fs::create_dir_all(script_path.parent().expect("scripts dir")).expect("create scripts dir");
    fs::write(&script_path, "#!/bin/sh\necho broken\n").expect("write companion script fixture");

    let outcome =
        writer::delete_workflow(dir.path(), "broken_del_qp4").expect("delete_workflow should succeed (D-8)");

    assert!(!md_path.exists(), "expected the unparseable .md to be removed (D-8)");
    assert!(!script_path.exists(), "expected the companion scripts/<id>.sh to be removed too (D-8)");
    assert_eq!(outcome.workflow_path, md_path);
    assert_eq!(outcome.script_path, Some(script_path));
}

#[test]
fn unknown_id_with_no_matching_file_leaves_the_directory_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    write_workflow(
        dir.path(),
        "other_qp4.md",
        r#"---
id: other_qp4
name: Other Qp4
service:
  type: action
  handler: action.immediate
---
A sibling workflow unrelated to the delete under test.
"#,
    );
    let before = snapshot(dir.path());

    let result = writer::delete_workflow(dir.path(), "does_not_exist_qp4");
    match result {
        Err(DeleteError::NotFound { id }) => assert_eq!(id, "does_not_exist_qp4"),
        other => panic!("expected DeleteError::NotFound, got: {other:?}"),
    }

    let after = snapshot(dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after NotFound");
}

#[test]
fn traversal_id_is_rejected_before_any_filesystem_call() {
    let dir = TempDir::new().expect("tempdir");
    let before = snapshot(dir.path());

    for bad_id in ["..", "../escape", "/absolute", "-leading-dash", "has/slash"] {
        let result = writer::delete_workflow(dir.path(), bad_id);
        assert!(result.is_err(), "expected a traversal-shaped id `{bad_id}` to be rejected (T-qp4-01)");
    }

    let after = snapshot(dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after every traversal rejection");
}

// ---- wire-level test ----

/// Binds `server::serve` to an OS-assigned ephemeral loopback port over an
/// `InProcessOrchestrator` rooted at `workflows_dir`, mirroring
/// `create_workflow_integration.rs`'s `spawn_test_daemon` helper.
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
    let activity_registry = Arc::new(ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

async fn connect(port: u16) -> WsStream {
    let (mut ws, _response) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect to the in-process WS server");
    let hello = Envelope::Hello {
        client_name: "delete_workflow_integration_test".to_string(),
    };
    send(&mut ws, &hello).await;
    ws
}

async fn send(ws: &mut WsStream, envelope: &Envelope) {
    let text = serde_json::to_string(envelope).expect("failed to encode envelope");
    ws.send(Message::text(text)).await.expect("failed to send frame");
}

async fn recv(ws: &mut WsStream) -> Envelope {
    loop {
        let msg = ws
            .next()
            .await
            .expect("expected a frame before the stream ended")
            .expect("expected a valid WS message");
        let text = msg.to_text().expect("expected a text frame");
        let envelope: Envelope = serde_json::from_str(text).expect("expected a valid Envelope");
        if matches!(envelope, Envelope::Activity { .. }) {
            continue;
        }
        return envelope;
    }
}

#[tokio::test]
async fn wire_delete_workflow_removes_the_file_and_connection_stays_alive() {
    let dir = TempDir::new().expect("tempdir");
    let req = CreateWorkflowRequest {
        id: "wire_del_qp4".to_string(),
        name: "Wire Del Qp4".to_string(),
        description: "Says hi.".to_string(),
        script: Some("#!/bin/sh\necho hi\n".to_string()),
        parameters: Vec::new(),
        mode: shared::WorkflowWriteMode::Create,
    agent: None,
    };
    let create_outcome = writer::create_workflow(dir.path(), &req).expect("fixture create should succeed");
    let script_path = create_outcome.script_path.clone().expect("expected a script_path");

    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let delete_req = Envelope::Req {
        id: 1,
        payload: RequestPayload::DeleteWorkflow(DeleteWorkflowRequest {
            id: "wire_del_qp4".to_string(),
        }),
    };
    send(&mut ws, &delete_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::DeleteWorkflow(resp),
        } => {
            assert_eq!(id, 1);
            assert!(resp.deleted, "expected deleted: true, got: {resp:?}");
        }
        other => panic!("expected Envelope::Res with a DeleteWorkflow payload, got: {other:?}"),
    }

    assert!(!create_outcome.workflow_path.exists(), "expected the .md to be removed");
    assert!(!script_path.exists(), "expected the companion script to be removed");

    // A delete must never kill the connection -- prove it still serves a
    // subsequent request, and that the deleted id is gone.
    let list_req = Envelope::Req {
        id: 2,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &list_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(resp),
        } => {
            assert_eq!(id, 2, "expected the connection to still answer a subsequent ListWorkflows request");
            let ids: Vec<String> = resp.workflows.iter().map(|w| w.id.clone()).collect();
            assert!(
                !ids.contains(&"wire_del_qp4".to_string()),
                "expected the just-deleted workflow to be immediately absent with NO daemon restart, got: {ids:?}"
            );
        }
        other => panic!("expected the connection to survive a delete, got: {other:?}"),
    }
}
