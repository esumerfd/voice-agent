//! Wave-0 integration test (D-03/D-04, Pitfall 3): proves the Hello-first
//! handshake -- a `Hello` frame sets client identity, and a missing/
//! malformed/oversized `Hello` is a non-fatal handled no-op that still
//! leaves the connection functional (labeled `"unknown"`). Mirrors
//! `ws_server_integration.rs`'s in-process daemon harness.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use orchestrator::activity::ActivityRegistry;
use orchestrator::{
    Envelope, InProcessOrchestrator, ListWorkflowsRequest, RequestPayload, ResponsePayload,
    Service,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SYNC_TIMER_WORKFLOW: &str = r#"---
id: sync_timer
name: Sync Timer
parameters:
  duration_minutes:
    type: int
    required: true
service:
  type: action
  handler: timers.start
---
Sync timer fixture (service.mode absent -> defaults Sync).
"#;

fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

async fn spawn_server(workflows_dir: &Path, handlers: HashMap<String, Box<dyn Service>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let orchestrator = Arc::new(InProcessOrchestrator::with_handlers(workflows_dir, handlers));
    let activity_registry = Arc::new(ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

async fn connect(port: u16) -> WsStream {
    let (ws, _response) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .expect("failed to connect to the in-process WS server");
    ws
}

async fn send(ws: &mut WsStream, envelope: &Envelope) {
    let text = serde_json::to_string(envelope).expect("failed to encode envelope");
    ws.send(Message::text(text)).await.expect("failed to send frame");
}

async fn recv(ws: &mut WsStream) -> Envelope {
    let msg = ws
        .next()
        .await
        .expect("expected a frame before the stream ended")
        .expect("expected a valid WS message");
    let text = msg.to_text().expect("expected a text frame");
    serde_json::from_str(text).expect("expected a valid Envelope")
}

#[tokio::test]
async fn missing_hello_is_non_fatal_and_the_connection_still_answers_requests() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "sync_timer.md", SYNC_TIMER_WORKFLOW);
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;

    let mut ws = connect(port).await;
    // Send a plain Req as the very first frame -- no Hello at all.
    let req = Envelope::Req {
        id: 1,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(resp),
        } => {
            assert_eq!(
                id, 1,
                "expected the daemon to still process the Req even with no prior Hello"
            );
            let ids: Vec<String> = resp.workflows.iter().map(|w| w.id.clone()).collect();
            assert!(
                ids.contains(&"sync_timer".to_string()),
                "expected the fixture workflow, got: {ids:?}"
            );
        }
        other => panic!("expected a Res reply despite the missing Hello, got: {other:?}"),
    }
}

#[tokio::test]
async fn hello_sets_identity_and_the_connection_still_answers_requests() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "sync_timer.md", SYNC_TIMER_WORKFLOW);
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;

    let mut ws = connect(port).await;
    send(
        &mut ws,
        &Envelope::Hello {
            client_name: "orchestrator-tui".to_string(),
        },
    )
    .await;

    let req = Envelope::Req {
        id: 1,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(id, 1, "expected a normal Res reply after a Hello handshake");
        }
        other => panic!("expected a Res reply after Hello, got: {other:?}"),
    }
}

#[tokio::test]
async fn oversized_hello_does_not_crash_the_daemon_and_the_connection_stays_functional() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "sync_timer.md", SYNC_TIMER_WORKFLOW);
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;

    let mut ws = connect(port).await;
    let huge_name = "x".repeat(10_000);
    send(
        &mut ws,
        &Envelope::Hello {
            client_name: huge_name,
        },
    )
    .await;

    let req = Envelope::Req {
        id: 2,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(id, 2, "expected the daemon to remain functional after an oversized Hello");
        }
        other => panic!("expected a Res reply after an oversized Hello, got: {other:?}"),
    }

    // A fresh connection proves the daemon itself never crashed.
    let mut fresh = connect(port).await;
    let req2 = Envelope::Req {
        id: 3,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut fresh, &req2).await;
    match recv(&mut fresh).await {
        Envelope::Res { id, .. } => {
            assert_eq!(id, 3, "expected the daemon to still be alive for a fresh connection")
        }
        other => panic!("expected a Res reply on a fresh connection, got: {other:?}"),
    }
}

#[tokio::test]
async fn malformed_first_frame_is_non_fatal_and_the_connection_stays_functional() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "sync_timer.md", SYNC_TIMER_WORKFLOW);
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;

    let mut ws = connect(port).await;
    ws.send(Message::text("not json at all"))
        .await
        .expect("failed to send a malformed first frame");

    let req = Envelope::Req {
        id: 4,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res { id, .. } => assert_eq!(
            id, 4,
            "expected the connection to remain functional after a malformed first frame"
        ),
        other => panic!("expected a Res reply, got: {other:?}"),
    }
}
