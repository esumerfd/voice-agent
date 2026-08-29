//! Integration tests for `registry::writer::create_workflow` (quick task
//! 260807-shx, Task 2): the daemon-side filesystem write behind the new
//! `CreateWorkflow` RPC. Asserts the load-back round-trip through
//! `orchestrator::Registry::load` rather than string-matching the emitted
//! YAML -- the file contents are an implementation detail, the loadability
//! is the contract (mirrors `registry_integration.rs`'s fixture style).
//!
//! Task 3 extends this file with the over-the-wire behaviors once the RPC
//! arm exists.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use orchestrator::registry::writer::{self, MAX_CONTENT_LEN, MAX_WORKFLOW_ID_LEN};
use orchestrator::{
    activity::ActivityRegistry, CreateError, Envelope, InProcessOrchestrator, ListWorkflowsRequest,
    Registry, RequestPayload, ResponsePayload, Service,
};
use shared::{CreateWorkflowRequest, ParameterDescriptor, ParameterType};
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
        mode: shared::WorkflowWriteMode::Create,
    agent: None,
    }
}

/// Snapshots the byte contents of every file under `dir` (relative path ->
/// contents), for asserting a rejected create left the directory untouched.
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
fn markdown_only_create_round_trips_through_registry_load() {
    let dir = TempDir::new().expect("tempdir");
    let req = CreateWorkflowRequest {
        id: "note_shx".to_string(),
        name: "Note Shx".to_string(),
        description: "Some prose describing a note.".to_string(),
        script: None,
        parameters: Vec::new(),
        mode: shared::WorkflowWriteMode::Create,
    agent: None,
    };

    let outcome = writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");
    assert!(outcome.workflow_path.exists(), "expected the .md file to exist");
    assert!(outcome.script_path.is_none(), "expected no script_path for a markdown-only create");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("note_shx").expect("lookup should resolve the new workflow");
    assert_eq!(def.name, "Note Shx");
    assert_eq!(def.description, "Some prose describing a note.");
    assert_eq!(def.service.handler, orchestrator::definition::DEFAULT_HANDLER);
    assert!(def.service.command.is_none(), "expected no command for a markdown-only create");
}

#[test]
fn script_create_writes_executable_script_and_command_resolves_to_existing_file() {
    let dir = TempDir::new().expect("tempdir");
    let req = CreateWorkflowRequest {
        id: "hello_shx".to_string(),
        name: "Hello Shx".to_string(),
        description: "Says hello.".to_string(),
        script: Some("#!/bin/sh\necho hello\n".to_string()),
        parameters: Vec::new(),
        mode: shared::WorkflowWriteMode::Create,
    agent: None,
    };

    let outcome = writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");
    let script_path = outcome.script_path.clone().expect("expected a script_path");
    assert!(script_path.exists(), "expected the script file to exist");
    let contents = fs::read_to_string(&script_path).expect("read script");
    assert_eq!(contents, "#!/bin/sh\necho hello\n");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("hello_shx").expect("lookup should resolve the new workflow");
    let command = def.service.command.clone().expect("expected a resolved command");
    assert!(
        command.exists(),
        "expected the resolved command path to exist on disk (proves scripts/<id>.sh anchors correctly): {command:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script_path).expect("metadata").permissions().mode();
        assert!(mode & 0o100 != 0, "expected the owner-execute bit set, got mode {mode:o}");
    }
}

#[test]
fn declared_parameters_survive_the_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let mut req = base_request("timed_shx");
    req.parameters = vec![
        ParameterDescriptor {
            name: "dur".to_string(),
            type_: ParameterType::Int,
            required: true,
        },
        ParameterDescriptor {
            name: "note".to_string(),
            type_: ParameterType::String,
            required: false,
        },
    ];

    writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors: {errors:?}");
    let def = registry.lookup("timed_shx").expect("lookup should resolve");

    let dur = def.parameters.get("dur").expect("dur param present");
    assert_eq!(dur.type_, ParameterType::Int);
    assert!(dur.required);

    let note = def.parameters.get("note").expect("note param present");
    assert_eq!(note.type_, ParameterType::String);
    assert!(!note.required);
}

#[test]
fn invalid_ids_are_rejected_and_directory_is_left_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    let before = snapshot(dir.path());

    let bad_ids = [
        "has/slash",
        "has..dots",
        "has.dot",
        "/absolute",
        "",
        &"x".repeat(MAX_WORKFLOW_ID_LEN + 1),
        "-leading-dash",
    ];

    for bad_id in bad_ids {
        let req = base_request(bad_id);
        let result = writer::create_workflow(dir.path(), &req);
        match result {
            Err(CreateError::InvalidId { .. }) => {}
            other => panic!("expected CreateError::InvalidId for id {bad_id:?}, got: {other:?}"),
        }
    }

    let after = snapshot(dir.path());
    assert_eq!(before, after, "expected the workflows directory to be byte-identical after all rejections");
}

#[test]
fn duplicate_id_already_resolved_by_registry_is_rejected_and_file_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    let req = base_request("dup_shx");
    writer::create_workflow(dir.path(), &req).expect("first create should succeed");

    let md_path = dir.path().join("dup_shx.md");
    let before = fs::read(&md_path).expect("read existing file");

    let dup_req = base_request("dup_shx");
    let result = writer::create_workflow(dir.path(), &dup_req);
    match result {
        Err(CreateError::AlreadyExists { .. }) => {}
        other => panic!("expected CreateError::AlreadyExists, got: {other:?}"),
    }

    let after = fs::read(&md_path).expect("read existing file after rejected duplicate");
    assert_eq!(before, after, "expected the existing file to be unmodified");
}

#[test]
fn existing_but_unparseable_md_file_is_still_treated_as_already_exists() {
    let dir = TempDir::new().expect("tempdir");
    let md_path = dir.path().join("broken_shx.md");
    fs::write(&md_path, "not valid frontmatter at all").expect("write broken fixture");
    let before = fs::read(&md_path).expect("read fixture");

    let req = base_request("broken_shx");
    let result = writer::create_workflow(dir.path(), &req);
    match result {
        Err(CreateError::AlreadyExists { .. }) => {}
        other => panic!("expected CreateError::AlreadyExists for an existing-but-unparseable file, got: {other:?}"),
    }

    let after = fs::read(&md_path).expect("read fixture after rejected create");
    assert_eq!(before, after, "expected the existing unparseable file to be unmodified");
}

#[test]
fn oversized_description_is_rejected_and_writes_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let mut req = base_request("too_big_shx");
    req.description = "x".repeat(MAX_CONTENT_LEN + 1);

    let result = writer::create_workflow(dir.path(), &req);
    match result {
        Err(CreateError::ContentTooLarge { .. }) => {}
        other => panic!("expected CreateError::ContentTooLarge, got: {other:?}"),
    }
    assert!(snapshot(dir.path()).is_empty(), "expected nothing written for an oversized description");
}

#[test]
fn oversized_script_is_rejected_and_writes_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let mut req = base_request("too_big_script_shx");
    req.script = Some("x".repeat(MAX_CONTENT_LEN + 1));

    let result = writer::create_workflow(dir.path(), &req);
    match result {
        Err(CreateError::ContentTooLarge { .. }) => {}
        other => panic!("expected CreateError::ContentTooLarge, got: {other:?}"),
    }
    assert!(snapshot(dir.path()).is_empty(), "expected nothing written for an oversized script");
}

#[test]
fn name_with_newline_and_yaml_metacharacters_round_trips_verbatim() {
    let dir = TempDir::new().expect("tempdir");
    let mut req = base_request("meta_shx");
    req.name = "evil\nname: injected\n---\nid: hijacked".to_string();

    writer::create_workflow(dir.path(), &req).expect("create_workflow should succeed even with a hostile name");

    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "unexpected load errors -- a hostile name must never break YAML parsing: {errors:?}");
    let def = registry.lookup("meta_shx").expect("lookup should resolve the new workflow");
    assert_eq!(
        def.name, "evil\nname: injected\n---\nid: hijacked",
        "expected the hostile name to be preserved verbatim, not interpreted as YAML"
    );
}

// ---- Task 3: over-the-wire behaviors (InProcessOrchestrator::create_workflow + server arm) ----

/// Binds `server::serve` to an OS-assigned ephemeral loopback port over an
/// `InProcessOrchestrator` rooted at `workflows_dir`, mirroring
/// `ws_server_integration.rs`'s `spawn_server` helper.
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

/// Connects and sends the mandatory Hello-first frame (D-04), matching
/// `server/mod.rs::read_hello`'s handshake requirement.
async fn connect(port: u16) -> WsStream {
    let (mut ws, _response) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect to the in-process WS server");
    let hello = Envelope::Hello {
        client_name: "create_workflow_integration_test".to_string(),
    };
    send(&mut ws, &hello).await;
    ws
}

async fn send(ws: &mut WsStream, envelope: &Envelope) {
    let text = serde_json::to_string(envelope).expect("failed to encode envelope");
    ws.send(Message::text(text)).await.expect("failed to send frame");
}

/// Receives the next frame, transparently skipping any `Envelope::Activity`
/// broadcast frame (mirrors `ws_server_integration.rs::recv`).
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
async fn wire_create_workflow_markdown_only_succeeds_and_file_exists() {
    let dir = TempDir::new().expect("tempdir");
    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let req = Envelope::Req {
        id: 1,
        payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
            id: "note_shx".to_string(),
            name: "Note Shx".to_string(),
            description: "Some prose.".to_string(),
            script: None,
            parameters: Vec::new(),
            mode: shared::WorkflowWriteMode::Create,
        agent: None,
        }),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::CreateWorkflow(resp),
        } => {
            assert_eq!(id, 1, "expected the res id to match the req id");
            assert!(resp.created, "expected created: true, got: {resp:?}");
            assert!(resp.error.is_none(), "expected no error, got: {resp:?}");
            let workflow_path = resp.workflow_path.expect("expected Some(workflow_path)");
            assert!(
                std::path::Path::new(&workflow_path).exists(),
                "expected the .md to exist at {workflow_path}"
            );
            assert!(resp.script_path.is_none(), "expected no script_path for a markdown-only create");
        }
        other => panic!("expected Envelope::Res with a CreateWorkflow payload, got: {other:?}"),
    }
}

#[tokio::test]
async fn wire_create_workflow_with_script_succeeds_and_both_files_exist() {
    let dir = TempDir::new().expect("tempdir");
    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let req = Envelope::Req {
        id: 2,
        payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
            id: "hello_shx".to_string(),
            name: "Hello Shx".to_string(),
            description: "Says hello.".to_string(),
            script: Some("#!/bin/sh\necho hi\n".to_string()),
            parameters: Vec::new(),
            mode: shared::WorkflowWriteMode::Create,
        agent: None,
        }),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::CreateWorkflow(resp),
        } => {
            assert_eq!(id, 2, "expected the res id to match the req id");
            assert!(resp.created, "expected created: true, got: {resp:?}");
            let workflow_path = resp.workflow_path.expect("expected Some(workflow_path)");
            let script_path = resp.script_path.expect("expected Some(script_path)");
            assert!(std::path::Path::new(&workflow_path).exists(), "expected the .md to exist");
            assert!(std::path::Path::new(&script_path).exists(), "expected the script to exist");
        }
        other => panic!("expected Envelope::Res with a CreateWorkflow payload, got: {other:?}"),
    }
}

#[tokio::test]
async fn wire_create_workflow_traversal_id_rejected_and_connection_stays_alive() {
    let dir = TempDir::new().expect("tempdir");
    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let req = Envelope::Req {
        id: 3,
        payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
            id: "../../evil".to_string(),
            name: "Evil".to_string(),
            description: "x".to_string(),
            script: None,
            parameters: Vec::new(),
            mode: shared::WorkflowWriteMode::Create,
        agent: None,
        }),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::CreateWorkflow(resp),
        } => {
            assert_eq!(id, 3, "expected the res id to match the req id");
            assert!(!resp.created, "expected created: false for a traversal-shaped id, got: {resp:?}");
            assert!(resp.error.is_some(), "expected a non-empty error, got: {resp:?}");
        }
        other => panic!("expected Envelope::Res with a rejected CreateWorkflow payload, got: {other:?}"),
    }

    // A rejected create must never kill the connection -- prove it still
    // serves a subsequent request.
    let list_req = Envelope::Req {
        id: 4,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &list_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(id, 4, "expected the connection to still answer a subsequent ListWorkflows request");
        }
        other => panic!("expected the connection to survive a rejected create, got: {other:?}"),
    }
}

#[tokio::test]
async fn wire_create_workflow_then_list_on_same_connection_shows_new_id_with_no_restart() {
    let dir = TempDir::new().expect("tempdir");
    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let create_req = Envelope::Req {
        id: 5,
        payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
            id: "fresh_shx".to_string(),
            name: "Fresh Shx".to_string(),
            description: "Fresh.".to_string(),
            script: None,
            parameters: Vec::new(),
            mode: shared::WorkflowWriteMode::Create,
        agent: None,
        }),
    };
    send(&mut ws, &create_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            payload: ResponsePayload::CreateWorkflow(resp),
            ..
        } => assert!(resp.created, "expected the create to succeed, got: {resp:?}"),
        other => panic!("expected a CreateWorkflow response, got: {other:?}"),
    }

    let list_req = Envelope::Req {
        id: 6,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &list_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            payload: ResponsePayload::ListWorkflows(resp),
            ..
        } => {
            let ids: Vec<String> = resp.workflows.iter().map(|w| w.id.clone()).collect();
            assert!(
                ids.contains(&"fresh_shx".to_string()),
                "expected the just-created workflow to be immediately listable with NO daemon restart, got: {ids:?}"
            );
        }
        other => panic!("expected a ListWorkflows response, got: {other:?}"),
    }
}

#[tokio::test]
async fn wire_duplicate_create_returns_created_false_with_error_naming_the_id() {
    let dir = TempDir::new().expect("tempdir");
    let port = spawn_test_daemon(dir.path()).await;
    let mut ws = connect(port).await;

    let req = |id: u64| Envelope::Req {
        id,
        payload: RequestPayload::CreateWorkflow(CreateWorkflowRequest {
            id: "dup_wire_shx".to_string(),
            name: "Dup Wire Shx".to_string(),
            description: "First.".to_string(),
            script: None,
            parameters: Vec::new(),
            mode: shared::WorkflowWriteMode::Create,
        agent: None,
        }),
    };

    send(&mut ws, &req(7)).await;
    match recv(&mut ws).await {
        Envelope::Res {
            payload: ResponsePayload::CreateWorkflow(resp),
            ..
        } => assert!(resp.created, "expected the first create to succeed, got: {resp:?}"),
        other => panic!("expected a CreateWorkflow response, got: {other:?}"),
    }

    send(&mut ws, &req(8)).await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::CreateWorkflow(resp),
        } => {
            assert_eq!(id, 8);
            assert!(!resp.created, "expected the duplicate create to fail, got: {resp:?}");
            let error = resp.error.expect("expected a non-empty error");
            assert!(
                error.contains("dup_wire_shx"),
                "expected the error to name the existing id, got: {error}"
            );
        }
        other => panic!("expected a CreateWorkflow response, got: {other:?}"),
    }
}
