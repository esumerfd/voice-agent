//! End-to-end reconnect-recovery proof (Phase 8, plan 08-01, D-01/D-02/D-05,
//! JOBS-01/JOBS-02): a client that invoked an async run, disconnected, and
//! reconnected on a fresh connection can recover that run's current state
//! and terminal result by `run_id` alone -- the identifier it already holds
//! from the original `Started` ack. Mirrors
//! `connection_registry_integration.rs`'s in-process daemon harness
//! (`spawn_server`, `connect`, `send`, `recv`, `hello`) and the
//! `ASYNC_TIMER_WORKFLOW` fixture.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use shared::{ActivityPhase, ActivityStatus, ProtocolFrame};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use orchestrator::activity::ActivityRegistry;
use orchestrator::handlers::timers::TimerService;
use orchestrator::{
    Envelope, InProcessOrchestrator, InvokeWorkflowRequest, ListWorkflowsRequest, RequestPayload,
    ResponsePayload, Service,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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

/// Receives the next frame, transparently skipping any `Envelope::Activity`
/// broadcast frame (mirrors `connection_registry_integration.rs::recv`).
/// Deliberately does NOT skip `Envelope::Protocol` frames -- this file's
/// tests assert directly on `Welcome`/`RunDescription`.
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

async fn hello(ws: &mut WsStream, client_name: &str) {
    send(
        ws,
        &Envelope::Hello {
            client_name: client_name.to_string(),
            capabilities: Vec::new(),
        },
    )
    .await;
}

fn timer_handlers() -> HashMap<String, Box<dyn Service>> {
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    handlers
}

/// The end-to-end tracer proof (JOBS-01): connect, receive a minted session
/// id as the FIRST frame, start an async run, disconnect, reconnect on a
/// fresh connection, ask for that run by id, and get back its current state
/// and terminal result.
#[tokio::test]
async fn describe_run_recovers_a_completed_run_after_reconnect() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);
    let port = spawn_server(dir.path(), timer_handlers()).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    // Welcome must be the FIRST server-to-client frame on the connection,
    // emitted before any Activity replay frame.
    let session_id = match recv(&mut ws).await {
        Envelope::Protocol {
            frame: ProtocolFrame::Welcome { session_id },
            ..
        } => {
            assert!(!session_id.is_empty(), "expected a non-empty minted session id");
            session_id
        }
        other => panic!("expected Envelope::Protocol(Welcome) as the first frame, got: {other:?}"),
    };

    let req = Envelope::Req {
        id: 1,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: "async_timer".to_string(),
            payload: json!({"duration_minutes": 1}),
        }),
    };
    send(&mut ws, &req).await;

    let run_id = match recv(&mut ws).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.clone().expect("expected a Some(run_id) on the Started ack"),
        other => panic!("expected a Started ack, got: {other:?}"),
    };

    // Disconnect before the run reaches a terminal state.
    drop(ws);

    // Give the spawned dispatch task time to reach a terminal state on the
    // server side even with no client connected to observe it.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reconnect on a FRESH connection -- this is the recovery path, never
    // keyed by the first connection's session id.
    let mut ws2 = connect(port).await;
    hello(&mut ws2, "orchestrator-cli").await;
    let second_session_id = match recv(&mut ws2).await {
        Envelope::Protocol {
            frame: ProtocolFrame::Welcome { session_id },
            ..
        } => session_id,
        other => panic!("expected Envelope::Protocol(Welcome) on the second connection, got: {other:?}"),
    };
    assert_ne!(
        session_id, second_session_id,
        "expected a freshly-minted session id on the reconnecting connection"
    );

    send(
        &mut ws2,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::DescribeRun {
                run_id: run_id.clone(),
            },
        },
    )
    .await;

    match recv(&mut ws2).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::RunDescription {
                    run_id: got_run_id,
                    found,
                    event,
                },
            ..
        } => {
            assert!(found, "expected found: true for a completed run recovered by run_id");
            assert_eq!(got_run_id, run_id, "expected the reply to name the requested run_id");
            let event = event.expect("expected Some(event) when found is true");
            assert_eq!(
                event.status,
                ActivityStatus::Success,
                "expected the recovered run's current status to be terminal Success"
            );
            assert!(
                event.log.iter().any(|entry| entry.phase == ActivityPhase::Completed),
                "expected a terminal Completed entry in the recovered event's log, got: {:?}",
                event.log
            );
        }
        other => panic!("expected Envelope::Protocol(RunDescription), got: {other:?}"),
    }
}

/// A `DescribeRun` for a run_id the registry has never seen returns
/// `found: false` with `event: None` -- a normal response, never a
/// `ServerError` and never a dropped connection -- and the connection still
/// answers a subsequent request.
#[tokio::test]
async fn describe_run_for_unknown_run_id_returns_found_false_and_connection_still_serves() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;
    let _welcome = recv(&mut ws).await; // consume this connection's own Welcome

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::DescribeRun {
                run_id: "run-does-not-exist".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame: ProtocolFrame::RunDescription { run_id, found, event },
            ..
        } => {
            assert!(!found, "expected found: false for an unknown run_id");
            assert_eq!(run_id, "run-does-not-exist");
            assert!(event.is_none(), "expected event: None for an unknown run_id");
        }
        other => panic!("expected Envelope::Protocol(RunDescription), got: {other:?}"),
    }

    // A DescribeRun for an unknown run_id must never kill the connection --
    // prove it still serves a subsequent request.
    let list_req = Envelope::Req {
        id: 2,
        payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
    };
    send(&mut ws, &list_req).await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(id, 2, "expected the connection to still answer a subsequent ListWorkflows request");
        }
        other => panic!("expected the connection to survive an unknown-run_id DescribeRun, got: {other:?}"),
    }
}

/// Two connections reporting the identical `client_name` receive distinct
/// session ids on `Welcome` (D-02) -- `client_name` is a display label
/// only, never an identity/dedup key.
#[tokio::test]
async fn two_connections_with_identical_client_name_get_distinct_welcome_session_ids() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let port = spawn_server(dir.path(), handlers).await;

    let mut ws1 = connect(port).await;
    hello(&mut ws1, "orchestrator-cli").await;
    let session_1 = match recv(&mut ws1).await {
        Envelope::Protocol {
            frame: ProtocolFrame::Welcome { session_id },
            ..
        } => session_id,
        other => panic!("expected Envelope::Protocol(Welcome), got: {other:?}"),
    };

    let mut ws2 = connect(port).await;
    hello(&mut ws2, "orchestrator-cli").await;
    let session_2 = match recv(&mut ws2).await {
        Envelope::Protocol {
            frame: ProtocolFrame::Welcome { session_id },
            ..
        } => session_id,
        other => panic!("expected Envelope::Protocol(Welcome), got: {other:?}"),
    };

    assert_ne!(
        session_1, session_2,
        "expected distinct session ids for two connections sharing the same client_name"
    );
}

/// Receives frames until an `Envelope::Activity` for `run_id` is observed,
/// skipping every other frame kind (`Welcome`, unrelated `Activity`
/// snapshots for other runs). Used to capture the `session_id` recorded on
/// a specific run's activity, as opposed to `recv`'s Welcome-first-frame
/// assertions above.
async fn recv_activity_for(ws: &mut WsStream, run_id: &str) -> shared::ActivityEvent {
    loop {
        let msg = ws
            .next()
            .await
            .expect("expected a frame before the stream ended")
            .expect("expected a valid WS message");
        let text = msg.to_text().expect("expected a text frame");
        let envelope: Envelope = serde_json::from_str(text).expect("expected a valid Envelope");
        if let Envelope::Activity { event } = envelope {
            if event.run_id == run_id {
                return event;
            }
        }
    }
}

/// D-01/D-02 (Task 2): two live WS connections both sending the identical
/// `client_name` each invoke a workflow; the two resulting
/// `Envelope::Activity` snapshots must carry different `session_id`
/// values -- every run is attributable to the session that started it, not
/// just the display-label `client_name`.
#[tokio::test]
async fn two_connections_with_same_client_name_invoking_workflows_record_distinct_session_ids() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);
    let port = spawn_server(dir.path(), timer_handlers()).await;

    let mut ws1 = connect(port).await;
    hello(&mut ws1, "orchestrator-cli").await;
    let _welcome1 = match recv(&mut ws1).await {
        Envelope::Protocol { frame: ProtocolFrame::Welcome { session_id }, .. } => session_id,
        other => panic!("expected Envelope::Protocol(Welcome), got: {other:?}"),
    };

    let mut ws2 = connect(port).await;
    hello(&mut ws2, "orchestrator-cli").await;
    let _welcome2 = match recv(&mut ws2).await {
        Envelope::Protocol { frame: ProtocolFrame::Welcome { session_id }, .. } => session_id,
        other => panic!("expected Envelope::Protocol(Welcome), got: {other:?}"),
    };

    let invoke = |workflow_id: &str| Envelope::Req {
        id: 1,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: workflow_id.to_string(),
            payload: json!({"duration_minutes": 1}),
        }),
    };

    send(&mut ws1, &invoke("async_timer")).await;
    let run_id_1 = match recv(&mut ws1).await {
        Envelope::Res { payload: ResponsePayload::InvokeWorkflow(resp), .. } => {
            resp.run_id.clone().expect("expected a Some(run_id) on the Started ack")
        }
        other => panic!("expected a Started ack, got: {other:?}"),
    };

    send(&mut ws2, &invoke("async_timer")).await;
    let run_id_2 = match recv(&mut ws2).await {
        Envelope::Res { payload: ResponsePayload::InvokeWorkflow(resp), .. } => {
            resp.run_id.clone().expect("expected a Some(run_id) on the Started ack")
        }
        other => panic!("expected a Started ack, got: {other:?}"),
    };

    let event_1 = recv_activity_for(&mut ws1, &run_id_1).await;
    let event_2 = recv_activity_for(&mut ws2, &run_id_2).await;

    assert_eq!(event_1.client_name, "orchestrator-cli");
    assert_eq!(event_2.client_name, "orchestrator-cli");
    assert_ne!(
        event_1.session_id, event_2.session_id,
        "two connections sharing the identical client_name must record distinct session ids on their activities"
    );
}
