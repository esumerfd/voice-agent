//! Wave-0 integration test (D-01): proves the daemon's cross-connection
//! broadcast wiring -- a second connected client receives the same
//! terminal `Envelope::Event` as the connection that started an async run,
//! and a disconnected third connection never blocks or breaks delivery to
//! the survivor (Pitfall 1/2). Mirrors `ws_server_integration.rs`'s
//! in-process daemon harness, driving multiple simultaneous connections.

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
    Envelope, InProcessOrchestrator, InvokeStatus, InvokeWorkflowRequest, RequestPayload,
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
/// broadcast frames (05-05, D-01) -- these now interleave with the Res/Event
/// frames this file's tests assert on, including on the invoking connection
/// itself (broadcast reaches every registered connection).
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
        },
    )
    .await;
}

#[tokio::test]
async fn fanout_delivers_the_same_broadcast_event_to_a_second_connected_client() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = spawn_server(dir.path(), handlers).await;

    let mut runner = connect(port).await;
    hello(&mut runner, "orchestrator-cli").await;
    let mut observer = connect(port).await;
    hello(&mut observer, "orchestrator-tui").await;

    let req = Envelope::Req {
        id: 1,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: "async_timer".to_string(),
            payload: json!({"duration_minutes": 1}),
        }),
    };
    send(&mut runner, &req).await;

    // Consume the immediate Started ack on the invoking connection.
    let _ack = recv(&mut runner).await;

    let runner_event = tokio::time::timeout(Duration::from_secs(5), recv(&mut runner))
        .await
        .expect("expected the invoking connection to receive the terminal event");
    let observer_event = tokio::time::timeout(Duration::from_secs(5), recv(&mut observer))
        .await
        .expect(
            "expected the OTHER connected client to also receive the terminal event (D-01 fan-out)",
        );

    for (label, event) in [("runner", runner_event), ("observer", observer_event)] {
        match event {
            Envelope::Event {
                id,
                payload: ResponsePayload::InvokeWorkflow(resp),
            } => {
                assert_eq!(
                    id, 1,
                    "expected the event id to match the original req id ({label})"
                );
                assert_eq!(
                    resp.status,
                    InvokeStatus::Completed,
                    "expected a Completed terminal status ({label}), got: {resp:?}"
                );
            }
            other => panic!(
                "expected an Envelope::Event with a Completed InvokeWorkflow payload ({label}), got: {other:?}"
            ),
        }
    }
}

#[tokio::test]
async fn disconnecting_one_of_two_clients_does_not_prevent_the_other_from_receiving_broadcasts() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = spawn_server(dir.path(), handlers).await;

    let mut runner = connect(port).await;
    hello(&mut runner, "orchestrator-cli").await;

    {
        let mut doomed = connect(port).await;
        hello(&mut doomed, "orchestrator-tui").await;
        // `doomed` drops here -- deregister must run unconditionally
        // (Pitfall 2), never leaking a dead entry that could later panic a
        // broadcast.
    }
    // Give the connection's own read-loop task time to observe the close
    // and run its unconditional cleanup.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let req = Envelope::Req {
        id: 2,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: "async_timer".to_string(),
            payload: json!({"duration_minutes": 1}),
        }),
    };
    send(&mut runner, &req).await;
    let _ack = recv(&mut runner).await;

    let event = tokio::time::timeout(Duration::from_secs(5), recv(&mut runner))
        .await
        .expect(
            "expected the surviving connection to still receive the broadcast after the other disconnected",
        );
    match event {
        Envelope::Event {
            id,
            payload: ResponsePayload::InvokeWorkflow(resp),
        } => {
            assert_eq!(id, 2, "expected the event id to match the original req id");
            assert_eq!(
                resp.status,
                InvokeStatus::Completed,
                "expected a Completed terminal status, got: {resp:?}"
            );
        }
        other => panic!("expected Envelope::Event with a Completed InvokeWorkflow payload, got: {other:?}"),
    }
}
