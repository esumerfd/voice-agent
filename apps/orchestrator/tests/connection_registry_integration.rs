//! Wave-0 integration test, superseded in part by Phase 8 plan 08-03
//! (D-03/D-04): this file originally proved that BOTH connected clients
//! received the SAME broadcast terminal `Envelope::Event` for a run either
//! one started -- the pre-Phase-8 "every client sees every activity"
//! broadcast contract. D-03 replaces that: the terminal `Event` is now
//! delivered addressed ONLY to the connection that started the run, never
//! broadcast to every connection. D-04 keeps the OTHER property this file
//! also proves: the shared `Activity` lifecycle broadcast still reaches
//! every connection, terminal phase and all, so a connection that did not
//! start a run still sees that it happened and how it ended -- just not its
//! addressed result. `disconnecting_one_of_two_clients_...` below is
//! unaffected by either decision: its surviving connection IS the run's
//! owner, so it still receives the addressed terminal Event exactly as
//! before (Pitfall 1/2 still hold).

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

use shared::ProtocolFrame;

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
/// itself (broadcast reaches every registered connection) -- and any
/// `Envelope::Protocol(Welcome)` frame (Phase 8, D-01): every accepted
/// connection now receives a server-minted session id as its first
/// server-to-client frame, ahead of the activity replay burst.
async fn recv(ws: &mut WsStream) -> Envelope {
    loop {
        let msg = ws
            .next()
            .await
            .expect("expected a frame before the stream ended")
            .expect("expected a valid WS message");
        let text = msg.to_text().expect("expected a text frame");
        let envelope: Envelope = serde_json::from_str(text).expect("expected a valid Envelope");
        if matches!(envelope, Envelope::Activity { .. })
            || matches!(
                envelope,
                Envelope::Protocol { frame: ProtocolFrame::Welcome { .. }, .. }
            )
        {
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

/// Receives frames on `ws` WITHOUT skipping `Envelope::Activity` (unlike
/// `recv` above) until an Activity frame for `run_id` reaches a terminal
/// (`Success`/`Failure`) status -- used by the D-04 observer assertion
/// below, which needs to see the very frames `recv` discards.
async fn recv_terminal_activity_for(ws: &mut WsStream, run_id: &str) -> shared::ActivityEvent {
    loop {
        let msg = ws
            .next()
            .await
            .expect("expected a frame before the stream ended")
            .expect("expected a valid WS message");
        let text = msg.to_text().expect("expected a text frame");
        let envelope: Envelope = serde_json::from_str(text).expect("expected a valid Envelope");
        if let Envelope::Activity { event } = envelope {
            if event.run_id == run_id
                && matches!(
                    event.status,
                    shared::ActivityStatus::Success | shared::ActivityStatus::Failure
                )
            {
                return event;
            }
        }
    }
}

/// Phase 8 plan 08-03 (D-03/D-04) contract: the terminal `Envelope::Event`
/// reaches ONLY the connection that started the run; a second connected
/// client that did not start it still sees the run's shared `Activity`
/// lifecycle reach a terminal phase, but never receives the addressed
/// terminal Event itself.
#[tokio::test]
async fn terminal_event_reaches_only_the_run_owner_while_the_observer_still_sees_the_activity_lifecycle(
) {
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
    let ack = recv(&mut runner).await;
    let run_id = match ack {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on the Started ack"),
        other => panic!("expected a Started ack, got: {other:?}"),
    };

    // D-03: the owner receives the addressed terminal Event.
    let runner_event = tokio::time::timeout(Duration::from_secs(5), recv(&mut runner))
        .await
        .expect("expected the invoking connection to receive its addressed terminal event");
    match runner_event {
        Envelope::Event {
            id,
            payload: ResponsePayload::InvokeWorkflow(resp),
        } => {
            assert_eq!(id, 1, "expected the event id to match the original req id");
            assert_eq!(
                resp.status,
                InvokeStatus::Completed,
                "expected a Completed terminal status, got: {resp:?}"
            );
        }
        other => panic!(
            "expected an Envelope::Event with a Completed InvokeWorkflow payload, got: {other:?}"
        ),
    }

    // D-04: the observer still sees the run's shared Activity lifecycle
    // reach a terminal phase...
    let observer_activity = tokio::time::timeout(
        Duration::from_secs(5),
        recv_terminal_activity_for(&mut observer, &run_id),
    )
    .await
    .expect("expected the observer to still see the run's Activity lifecycle reach a terminal phase");
    assert_eq!(observer_activity.run_id, run_id);

    // ...but D-03: the observer never receives a terminal Event for a run
    // it did not start, within a bounded wait.
    let observer_saw_no_event = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Envelope::Event { .. } = recv(&mut observer).await {
                return true;
            }
        }
    })
    .await
    .is_err();
    assert!(
        observer_saw_no_event,
        "expected the observer to receive no Envelope::Event at all for a run it did not start (D-03)"
    );
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
