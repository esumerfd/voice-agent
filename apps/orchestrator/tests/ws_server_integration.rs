//! Wave-0 WS server integration tests (D-01/D-03/D-07/D-08): binds
//! `orchestrator::server::serve` in-process to an OS-assigned ephemeral
//! loopback port (D-07 -- never spawns the compiled `orchestratord` binary
//! as a subprocess) and drives the full req/res/event wire contract over a
//! real `tokio_tungstenite` client connection. Mirrors
//! `dispatch_integration.rs`'s `TimerService::with_unit(short_duration)`
//! convention (Pitfall 1 -- no test ever waits a real minute) and
//! `run_integration.rs`'s fixture-writing/assertion conventions.
//!
//! This file is authored RED (Task 1) against `server::serve`,
//! `InvokeStatus::Started`, and `InvokeWorkflowResponse.run_id`, none of
//! which exist yet -- Task 2 makes it GREEN.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use orchestrator::activity::ActivityRegistry;
use orchestrator::handlers::timers::TimerService;
use orchestrator::{
    Envelope, InProcessOrchestrator, InvokeStatus, InvokeWorkflowRequest, ListWorkflowsRequest,
    RequestPayload, ResponsePayload, Service,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A workflow with no `service.mode` field -- defaults to Sync (D-10).
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

/// A workflow declaring `service.mode: async` (D-10).
const ASYNC_TIMER_WORKFLOW: &str = r#"---
id: async_timer
name: Async Timer
parameters:
  duration_minutes:
    type: int
    required: true
service:
  type: action
  handler: timers.start
  mode: async
---
Async timer fixture (service.mode: async).
"#;

/// Writes `contents` to `dir/filename`, creating any intermediate
/// directories the fixture requests. Mirrors `run_integration.rs`.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

/// Binds `server::serve` to an OS-assigned ephemeral loopback port (D-07)
/// against an `InProcessOrchestrator` built from `workflows_dir`/`handlers`,
/// and returns the bound port. The `TcpListener::bind` call itself already
/// puts the socket in the OS listen backlog, so a subsequent `connect_async`
/// never races the spawned accept-loop task.
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

/// Receives the next frame, transparently skipping any `Envelope::Activity`
/// broadcast frames (05-05, D-01): activity broadcasts are a new, additive
/// frame type that can arrive interleaved with the Res/Event frames these
/// pre-existing tests assert on -- including on the very connection that
/// triggered the activity, since broadcast reaches every registered
/// connection (D-01's "every client sees every activity").
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
async fn list_workflows_round_trips_over_ws() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "sync_timer.md", SYNC_TIMER_WORKFLOW);

    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;
    let mut ws = connect(port).await;

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
            assert_eq!(id, 1, "expected the res id to match the req id");
            let ids: Vec<String> = resp.workflows.iter().map(|w| w.id.clone()).collect();
            assert!(
                ids.contains(&"sync_timer".to_string()),
                "expected the fixture workflow in the list, got: {ids:?}"
            );
        }
        other => panic!("expected Envelope::Res with a ListWorkflows payload, got: {other:?}"),
    }
}

#[tokio::test]
async fn sync_invoke_over_ws_runs_dispatch_to_completed_result() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "sync_timer.md", SYNC_TIMER_WORKFLOW);

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = spawn_server(dir.path(), handlers).await;
    let mut ws = connect(port).await;

    let req = Envelope::Req {
        id: 2,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: "sync_timer".to_string(),
            payload: json!({"duration_minutes": 1}),
        }),
    };
    send(&mut ws, &req).await;

    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::InvokeWorkflow(resp),
        } => {
            assert_eq!(id, 2, "expected the res id to match the req id");
            assert_eq!(
                resp.status,
                InvokeStatus::Completed,
                "expected a terminal Completed status for sync mode, got: {resp:?}"
            );
            assert!(
                resp.output.as_ref().is_some_and(|v| v.is_object()),
                "expected structured JSON object output, got: {:?}",
                resp.output
            );
            assert!(
                resp.run_id.as_ref().is_some_and(|id| !id.is_empty()),
                "expected a sync-mode invocation to also carry a populated run_id (D-02), got: {:?}",
                resp.run_id
            );
        }
        other => panic!("expected Envelope::Res with a Completed InvokeWorkflow payload, got: {other:?}"),
    }
}

#[tokio::test]
async fn async_invoke_over_ws_acks_started_then_delivers_completed_event() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = spawn_server(dir.path(), handlers).await;
    let mut ws = connect(port).await;

    let req = Envelope::Req {
        id: 5,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: "async_timer".to_string(),
            payload: json!({"duration_minutes": 1}),
        }),
    };
    send(&mut ws, &req).await;

    let run_id = match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::InvokeWorkflow(resp),
        } => {
            assert_eq!(id, 5, "expected the ack res id to match the req id");
            assert_eq!(
                resp.status,
                InvokeStatus::Started,
                "expected an immediate Started ack for async mode, got: {resp:?}"
            );
            resp.run_id
                .clone()
                .expect("expected a Some(run_id) on the Started ack")
        }
        other => panic!("expected Envelope::Res with a Started InvokeWorkflow ack, got: {other:?}"),
    };
    assert!(!run_id.is_empty(), "expected a non-empty run_id");

    let event = tokio::time::timeout(Duration::from_secs(5), recv(&mut ws))
        .await
        .expect("expected a terminal event frame before the timeout");
    match event {
        Envelope::Event {
            id,
            payload: ResponsePayload::InvokeWorkflow(resp),
        } => {
            assert_eq!(id, 5, "expected the event id to match the original req id");
            assert_eq!(
                resp.status,
                InvokeStatus::Completed,
                "expected the async run to reach Completed, got: {resp:?}"
            );
        }
        other => panic!("expected Envelope::Event with a Completed InvokeWorkflow payload, got: {other:?}"),
    }
}

#[tokio::test]
async fn malformed_frame_errors_this_connection_only_daemon_keeps_serving() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "sync_timer.md", SYNC_TIMER_WORKFLOW);

    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;

    let mut bad_ws = connect(port).await;
    bad_ws
        .send(Message::text("not json at all"))
        .await
        .expect("failed to send a malformed frame");

    // Either an error res or the connection closing is acceptable here --
    // the real assertion is that a FRESH connection still answers a list
    // req afterward, proving the daemon did not crash (T-04-01).
    let _ = tokio::time::timeout(Duration::from_millis(500), bad_ws.next()).await;

    let mut fresh_ws = connect(port).await;
    let req = Envelope::Req {
        id: 99,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut fresh_ws, &req).await;

    match recv(&mut fresh_ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(id, 99, "expected the fresh connection's res id to match");
        }
        other => panic!(
            "expected the daemon to still answer a fresh connection's list req after a malformed frame, got: {other:?}"
        ),
    }
}

#[tokio::test]
async fn disconnect_mid_async_run_does_not_panic_daemon() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = spawn_server(dir.path(), handlers).await;

    {
        let mut ws = connect(port).await;
        let req = Envelope::Req {
            id: 10,
            payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
                workflow_id: "async_timer".to_string(),
                payload: json!({"duration_minutes": 1}),
            }),
        };
        send(&mut ws, &req).await;
        let _ack = recv(&mut ws).await; // consume the Started ack
        // `ws` drops here -- the client disconnects before the terminal
        // event would ever arrive.
    }

    // Give the spawned dispatch task time to reach completion and attempt
    // (and fail) its event send against the now-dropped connection.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut fresh_ws = connect(port).await;
    let req = Envelope::Req {
        id: 11,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut fresh_ws, &req).await;

    match recv(&mut fresh_ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(
                id, 11,
                "expected the daemon to still be alive and answer a fresh connection after a mid-async disconnect"
            );
        }
        other => panic!("expected the daemon to survive a mid-async-run disconnect, got: {other:?}"),
    }
}
