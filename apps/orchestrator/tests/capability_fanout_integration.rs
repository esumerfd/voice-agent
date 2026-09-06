//! Phase 8 plan 08-03 integration tests (D-03/D-04, FANOUT-01): proves the
//! full capability-addressing contract end-to-end over a real in-process WS
//! daemon -- a speech-shaped result field reaches only the connection whose
//! session started that run AND declared it can render it, while every
//! other connection still sees the run's shared `Activity` lifecycle with
//! the gated field stripped. Mirrors `connection_registry_integration.rs`'s
//! in-process daemon harness (`spawn_server`, `connect`, `send`, `hello`,
//! the `ASYNC_TIMER_WORKFLOW` fixture shape), and `dispatch_integration.rs`'s
//! test-only `Service` stub convention for `SpeechService` below.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use shared::ProtocolFrame;

use orchestrator::activity::ActivityRegistry;
use orchestrator::{
    Envelope, InProcessOrchestrator, InvokeStatus, InvokeWorkflowRequest, RequestPayload,
    ResponsePayload, RunHandle, RunStatus, Service, ServiceError,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A workflow declaring `service.mode: async` whose handler is
/// `speech.say` -- the fixture bound to `SpeechService` below.
const SPEECH_WORKFLOW: &str = r#"---
id: speak_it
name: Speak It
parameters:
  text:
    type: string
    required: true
service:
  type: action
  handler: speech.say
  mode: async
---
Speech fixture (service.mode: async) -- carries both an ungated "text" key
and a gated "speech" key in its result.
"#;

/// Test-only stub (mirrors `dispatch_integration.rs`'s `StubService`
/// convention): its `result()` always returns an object carrying both an
/// ungated key (`text`) and the gated `speech` key
/// (`shared::CAPABILITY_SPEECH`) -- exactly the shape D-03/D-04 must filter.
struct SpeechService;

#[async_trait]
impl Service for SpeechService {
    async fn invoke(&self, _input: serde_json::Value) -> Result<RunHandle, ServiceError> {
        Ok(RunHandle::new("speech-run-1"))
    }

    async fn status(&self, _handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        Ok(RunStatus::Completed)
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        Ok(())
    }

    async fn result(&self, _handle: &RunHandle) -> Result<Option<serde_json::Value>, ServiceError> {
        Ok(Some(json!({
            "text": "hello from claudette",
            "speech": {"audio": "base64-encoded-audio"}
        })))
    }
}

fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

fn speech_handlers() -> HashMap<String, Box<dyn Service>> {
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert("speech.say".to_string(), Box::new(SpeechService));
    handlers
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

/// Receives the next frame, skipping only this connection's own `Welcome`
/// frame (Phase 8, D-01) -- these tests deliberately want to observe
/// `Envelope::Activity` frames (bystander lifecycle assertions), not skip
/// them, so `Welcome` is the sole frame kind filtered out here (mirrors
/// `activity_registry_integration.rs::recv`).
async fn recv(ws: &mut WsStream) -> Envelope {
    loop {
        let msg = ws
            .next()
            .await
            .expect("expected a frame before the stream ended")
            .expect("expected a valid WS message");
        let text = msg.to_text().expect("expected a text frame");
        let envelope: Envelope = serde_json::from_str(text).expect("expected a valid Envelope");
        if matches!(
            envelope,
            Envelope::Protocol { frame: ProtocolFrame::Welcome { .. }, .. }
        ) {
            continue;
        }
        return envelope;
    }
}

/// Receives frames until an `Envelope::Res` arrives, skipping any
/// `Envelope::Activity` broadcasts interleaved ahead of it.
async fn recv_res(ws: &mut WsStream) -> Envelope {
    loop {
        let envelope = recv(ws).await;
        if matches!(envelope, Envelope::Res { .. }) {
            return envelope;
        }
    }
}

/// Receives frames until an `Envelope::Protocol` arrives, skipping any
/// `Envelope::Activity` frames interleaved ahead of it (e.g. a replay burst
/// delivered right after a reconnect, before the `DescribeRun` reply this
/// helper is waiting for).
async fn recv_protocol(ws: &mut WsStream) -> Envelope {
    loop {
        let envelope = recv(ws).await;
        if matches!(envelope, Envelope::Protocol { .. }) {
            return envelope;
        }
    }
}

/// Receives frames until an `Envelope::Activity` for `run_id` reaching a
/// terminal (`Success`/`Failure`) status is observed, skipping every other
/// frame in between.
async fn recv_terminal_activity_for(ws: &mut WsStream, run_id: &str) -> shared::ActivityEvent {
    loop {
        if let Envelope::Activity { event } = recv(ws).await {
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

async fn hello(ws: &mut WsStream, client_name: &str, capabilities: Vec<String>) {
    send(
        ws,
        &Envelope::Hello {
            client_name: client_name.to_string(),
            capabilities,
        },
    )
    .await;
}

fn invoke_speak_it(id: u64) -> Envelope {
    Envelope::Req {
        id,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: "speak_it".to_string(),
            payload: json!({"text": "hi"}),
        }),
    }
}

/// The Task 1 tracer proof (D-03/D-04): an owner declaring the speech
/// capability invokes a gated-field run; the owner's terminal `Event`
/// carries BOTH the ungated and gated keys; a bystander declaring nothing
/// still sees the run's Activity lifecycle reach a terminal phase, with the
/// gated key stripped from its detail; and the bystander never receives a
/// terminal `Event` for that run at all.
#[tokio::test]
async fn owner_declaring_speech_sees_the_gated_field_while_the_bystander_sees_only_the_lifecycle() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "speak_it.md", SPEECH_WORKFLOW);
    let port = spawn_server(dir.path(), speech_handlers()).await;

    let mut owner = connect(port).await;
    hello(&mut owner, "voice-front-end", vec![shared::CAPABILITY_SPEECH.to_string()]).await;
    let mut bystander = connect(port).await;
    hello(&mut bystander, "orchestrator-tui", Vec::new()).await;

    send(&mut owner, &invoke_speak_it(1)).await;
    let run_id = match recv_res(&mut owner).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on the Started ack"),
        other => panic!("expected a Started ack, got: {other:?}"),
    };

    // The owner receives the terminal Event carrying BOTH keys.
    let owner_event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Envelope::Event {
                payload: ResponsePayload::InvokeWorkflow(resp),
                ..
            } = recv(&mut owner).await
            {
                return resp;
            }
        }
    })
    .await
    .expect("expected the owner to receive the terminal Event");

    assert_eq!(owner_event.status, InvokeStatus::Completed);
    let output = owner_event.output.expect("expected Some(output) on the terminal event");
    assert_eq!(output["text"], json!("hello from claudette"));
    assert_eq!(
        output["speech"],
        json!({"audio": "base64-encoded-audio"}),
        "expected the owner, who declared the speech capability and started the run, to see the gated field"
    );

    // The bystander still sees the run's shared Activity lifecycle reach a
    // terminal phase, but with the gated key stripped from its detail.
    let bystander_activity = recv_terminal_activity_for(&mut bystander, &run_id).await;
    let detail = bystander_activity
        .log
        .last()
        .and_then(|entry| entry.detail.clone())
        .expect("expected a terminal log entry with Some(detail)");
    assert_eq!(detail["text"], json!("hello from claudette"));
    assert!(
        detail.get("speech").is_none(),
        "expected the gated speech key to be stripped from the bystander's Activity detail, got: {detail:?}"
    );

    // The bystander never receives a terminal Event for this run at all --
    // declaring no capability grants no addressing.
    let bystander_saw_no_event = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            if let Envelope::Event { .. } = recv(&mut bystander).await {
                return true;
            }
        }
    })
    .await
    .is_err();
    assert!(
        bystander_saw_no_event,
        "expected the bystander to receive no Envelope::Event at all for a run it did not start"
    );
}

/// Task 2 (D-04): a client connecting AFTER a gated-field run has already
/// completed is replayed that run's `Activity` frame on connect -- the
/// replay burst must be filtered exactly like the live broadcast is. A
/// client declaring no capabilities sees the ungated key but never the
/// gated one in the replayed detail.
#[tokio::test]
async fn replay_burst_for_a_client_declaring_nothing_omits_the_gated_field_from_a_completed_run() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "speak_it.md", SPEECH_WORKFLOW);
    let port = spawn_server(dir.path(), speech_handlers()).await;

    // First connection: owner declares speech, runs the workflow to
    // completion, then disconnects.
    let mut owner = connect(port).await;
    hello(&mut owner, "voice-front-end", vec![shared::CAPABILITY_SPEECH.to_string()]).await;
    send(&mut owner, &invoke_speak_it(1)).await;
    let run_id = match recv_res(&mut owner).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on the Started ack"),
        other => panic!("expected a Started ack, got: {other:?}"),
    };
    let _ = recv_terminal_activity_for(&mut owner, &run_id).await;
    drop(owner);

    // Second connection, arriving AFTER the run is already complete,
    // declares nothing -- its replay burst must still be filtered.
    let mut late_client = connect(port).await;
    hello(&mut late_client, "orchestrator-tui", Vec::new()).await;

    let replayed = tokio::time::timeout(
        Duration::from_secs(5),
        recv_terminal_activity_for(&mut late_client, &run_id),
    )
    .await
    .expect("expected the late-connecting client's replay burst to include the completed run");

    let detail = replayed
        .log
        .last()
        .and_then(|entry| entry.detail.clone())
        .expect("expected a terminal log entry with Some(detail)");
    assert_eq!(detail["text"], json!("hello from claudette"));
    assert!(
        detail.get("speech").is_none(),
        "expected the replay burst to omit the gated speech key for a client declaring nothing, got: {detail:?}"
    );
}

/// Task 2 (D-04): a client that invokes an async gated-field run, disconnects
/// before it terminates, then reconnects declaring the speech capability and
/// asks for the run via `DescribeRun`, gets back a `RunDescription` carrying
/// the gated key -- recovery honors the RECONNECTING connection's own
/// declaration, not whatever the original connection declared (in this case,
/// identical, but the point is the reply is filtered fresh each time).
#[tokio::test]
async fn describe_run_recovery_for_a_reconnecting_client_declaring_speech_carries_the_gated_field() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "speak_it.md", SPEECH_WORKFLOW);
    let port = spawn_server(dir.path(), speech_handlers()).await;

    let mut owner = connect(port).await;
    hello(&mut owner, "voice-front-end", vec![shared::CAPABILITY_SPEECH.to_string()]).await;
    send(&mut owner, &invoke_speak_it(1)).await;
    let run_id = match recv_res(&mut owner).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on the Started ack"),
        other => panic!("expected a Started ack, got: {other:?}"),
    };

    // Disconnect before the run reaches a terminal state.
    drop(owner);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Reconnect on a FRESH connection, declaring speech again.
    let mut reconnected = connect(port).await;
    hello(&mut reconnected, "voice-front-end", vec![shared::CAPABILITY_SPEECH.to_string()]).await;

    send(
        &mut reconnected,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::DescribeRun {
                run_id: run_id.clone(),
            },
        },
    )
    .await;

    match recv_protocol(&mut reconnected).await {
        Envelope::Protocol {
            frame: ProtocolFrame::RunDescription { found, event, .. },
            ..
        } => {
            assert!(found, "expected found: true for a completed run recovered by run_id");
            let event = event.expect("expected Some(event) when found is true");
            let detail = event
                .log
                .last()
                .and_then(|entry| entry.detail.clone())
                .expect("expected a terminal log entry with Some(detail)");
            assert_eq!(detail["text"], json!("hello from claudette"));
            assert_eq!(
                detail["speech"],
                json!({"audio": "base64-encoded-audio"}),
                "expected the reconnecting connection's own speech declaration to unlock the gated field, got: {detail:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(RunDescription), got: {other:?}"),
    }
}
