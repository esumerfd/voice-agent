//! Integration tests for `registry::writer::create_workflow`'s edit mode
//! (quick task 260812-qpn, Task 1): the daemon-side inverse-guard overwrite
//! behind `WorkflowWriteMode::Edit`. Mirrors `create_workflow_integration.rs`'s
//! conventions exactly -- `base_request` helper, `snapshot()` for asserting a
//! rejected write left the directory untouched, load-back assertions through
//! `Registry::load` rather than string-matching emitted YAML.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use orchestrator::registry::writer;
use orchestrator::{
    activity::ActivityRegistry, CreateError, Envelope, InProcessOrchestrator, Registry,
    RequestPayload, ResponsePayload, Service,
};
use shared::{CreateWorkflowRequest, WorkflowWriteMode};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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

fn edit_request(id: &str) -> CreateWorkflowRequest {
    let mut req = base_request(id);
    req.mode = WorkflowWriteMode::Edit;
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

#[test]
fn edit_of_an_unknown_id_is_rejected_and_leaves_the_directory_untouched() {
    let dir = TempDir::new().expect("tempdir");
    let before = snapshot(dir.path());

    let req = edit_request("no_such_qpn");
    let result = writer::create_workflow(dir.path(), &req);
    match result {
        Err(CreateError::NotFound { id, .. }) => {
            assert_eq!(id, "no_such_qpn");
        }
        other => panic!("expected CreateError::NotFound, got: {other:?}"),
    }

    let after = snapshot(dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after a rejected edit");
}

#[test]
fn edit_overwrites_an_existing_markdown_workflow_in_place() {
    let dir = TempDir::new().expect("tempdir");
    let create_req = base_request("edit_md_qpn");
    writer::create_workflow(dir.path(), &create_req).expect("fixture create should succeed");

    let mut edit_req = edit_request("edit_md_qpn");
    edit_req.name = "New Name".to_string();
    edit_req.description = "New description prose.".to_string();
    edit_req.parameters = vec![shared::ParameterDescriptor {
        name: "note".to_string(),
        type_: shared::ParameterType::String,
        required: true,
    }];
    writer::create_workflow(dir.path(), &edit_req).expect("edit should succeed");

    let md_count = walkdir::WalkDir::new(dir.path())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().map(|ext| ext == "md").unwrap_or(false))
        .count();
    assert_eq!(md_count, 1, "expected exactly one .md file to exist after the edit");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("edit_md_qpn").expect("lookup should resolve the edited workflow");
    assert_eq!(def.name, "New Name");
    assert_eq!(def.description, "New description prose.");
    let note = def.parameters.get("note").expect("note param present after edit");
    assert_eq!(note.type_, orchestrator::ParameterType::String);
    assert!(note.required);
}

#[test]
fn edit_replaces_the_companion_script_contents_and_keeps_it_executable() {
    let dir = TempDir::new().expect("tempdir");
    let mut create_req = base_request("edit_script_qpn");
    create_req.script = Some("#!/bin/sh\necho old\n".to_string());
    writer::create_workflow(dir.path(), &create_req).expect("fixture create should succeed");

    let mut edit_req = edit_request("edit_script_qpn");
    edit_req.script = Some("#!/bin/sh\necho new\n".to_string());
    let outcome = writer::create_workflow(dir.path(), &edit_req).expect("edit should succeed");

    let script_path = outcome.script_path.expect("expected a script_path");
    let contents = fs::read_to_string(&script_path).expect("read script");
    assert_eq!(contents, "#!/bin/sh\necho new\n");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script_path).expect("metadata").permissions().mode();
        assert!(mode & 0o100 != 0, "expected the owner-execute bit set, got mode {mode:o}");
    }
}

#[test]
fn edit_from_script_to_markdown_leaves_the_orphaned_script_file_on_disk() {
    let dir = TempDir::new().expect("tempdir");
    let mut create_req = base_request("edit_orphan_qpn");
    create_req.script = Some("#!/bin/sh\necho hi\n".to_string());
    let create_outcome = writer::create_workflow(dir.path(), &create_req).expect("fixture create should succeed");
    let script_path = create_outcome.script_path.expect("expected a script_path");
    assert!(script_path.exists(), "expected the fixture script to exist before the edit");

    let mut edit_req = edit_request("edit_orphan_qpn");
    edit_req.script = None;
    writer::create_workflow(dir.path(), &edit_req).expect("edit should succeed");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("edit_orphan_qpn").expect("lookup should resolve the edited workflow");
    assert!(def.service.command.is_none(), "expected no command after editing to markdown-only (D-5)");
    assert!(script_path.exists(), "expected the orphaned script file to remain on disk (D-5)");
}

#[test]
fn edit_still_rejects_an_id_that_fails_validation() {
    let dir = TempDir::new().expect("tempdir");
    let before = snapshot(dir.path());

    let mut req = edit_request("has/slash");
    req.name = "Evil".to_string();
    let result = writer::create_workflow(dir.path(), &req);
    match result {
        Err(CreateError::InvalidId { .. }) => {}
        other => panic!("expected CreateError::InvalidId, got: {other:?}"),
    }

    let after = snapshot(dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after a rejected edit (T-shx-01 guard survives)");
}

#[test]
fn create_mode_still_refuses_an_existing_id() {
    let dir = TempDir::new().expect("tempdir");
    let req = base_request("regress_qpn");
    writer::create_workflow(dir.path(), &req).expect("first create should succeed");

    let dup_req = base_request("regress_qpn");
    let result = writer::create_workflow(dir.path(), &dup_req);
    match result {
        Err(CreateError::AlreadyExists { .. }) => {}
        other => panic!("expected CreateError::AlreadyExists (create mode unchanged), got: {other:?}"),
    }
}

// ---- over-the-wire ----

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
        client_name: "edit_workflow_integration_test".to_string(),
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
async fn wire_edit_of_an_existing_id_succeeds_and_write_is_reported() {
    let dir = TempDir::new().expect("tempdir");
    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let create_req = Envelope::Req {
        id: 1,
        payload: RequestPayload::CreateWorkflow(base_request("wire_edit_qpn")),
    };
    send(&mut ws, &create_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            payload: ResponsePayload::CreateWorkflow(resp),
            ..
        } => assert!(resp.created, "expected the fixture create to succeed, got: {resp:?}"),
        other => panic!("expected a CreateWorkflow response, got: {other:?}"),
    }

    let mut edit_req_payload = edit_request("wire_edit_qpn");
    edit_req_payload.name = "Wire Edited".to_string();
    let edit_req = Envelope::Req {
        id: 2,
        payload: RequestPayload::CreateWorkflow(edit_req_payload),
    };
    send(&mut ws, &edit_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::CreateWorkflow(resp),
        } => {
            assert_eq!(id, 2, "expected the res id to match the req id");
            assert!(resp.created, "expected the write to be reported as succeeded, got: {resp:?}");
            assert!(resp.error.is_none(), "expected no error, got: {resp:?}");
        }
        other => panic!("expected Envelope::Res with a CreateWorkflow payload, got: {other:?}"),
    }
}

#[tokio::test]
async fn wire_edit_of_an_unknown_id_reports_failure_naming_the_id() {
    let dir = TempDir::new().expect("tempdir");
    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let edit_req = Envelope::Req {
        id: 1,
        payload: RequestPayload::CreateWorkflow(edit_request("unknown_wire_qpn")),
    };
    send(&mut ws, &edit_req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::CreateWorkflow(resp),
        } => {
            assert_eq!(id, 1, "expected the res id to match the req id");
            assert!(!resp.created, "expected the write to be reported as failed, got: {resp:?}");
            let error = resp.error.expect("expected a non-empty error");
            assert!(
                error.contains("unknown_wire_qpn"),
                "expected the error to name the unknown id, got: {error}"
            );
        }
        other => panic!("expected Envelope::Res with a rejected CreateWorkflow payload, got: {other:?}"),
    }
}
