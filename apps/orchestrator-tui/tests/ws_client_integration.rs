//! Integration test for `TuiWsClient` (05-06 Task 1): included via the same
//! `#[path]` trick `orchestrator-cli/tests/ws_client_integration.rs` uses
//! (there is no `[lib]` target for `orchestrator-tui`) so these tests
//! exercise the exact client `main.rs` constructs.
//!
//! The in-test WS server is a LIGHTWEIGHT raw `tokio-tungstenite` accept
//! loop on a loopback `TcpListener` -- no `orchestrator` crate needed, which
//! keeps this plan independent of the server-side plans (05-04/05-05).
//! Asserts: (1) the first frame the client sends is `Envelope::Hello`
//! naming `orchestrator-tui` (D-04), and (2) a server-pushed
//! `Envelope::Activity` frame is delivered to the client's activity-events
//! receiver.

#[path = "../src/ws_client.rs"]
mod ws_client;

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

use shared::{ActivityEvent, ActivityLogEvent, ActivityPhase, ActivityStatus, Envelope};

use ws_client::TuiWsClient;

fn sample_activity_event() -> ActivityEvent {
    ActivityEvent {
        run_id: "run-1".to_string(),
        workflow_id: "set_timer".to_string(),
        client_name: "orchestrator-tui".to_string(),
        status: ActivityStatus::Running,
        started_at_ms: 1_000,
        log: vec![ActivityLogEvent {
            phase: ActivityPhase::Invoked,
            at_ms: 1_000,
            detail: None,
        }],
    }
}

#[tokio::test]
async fn client_sends_hello_first_then_receives_pushed_activity_event() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let addr = format!("ws://127.0.0.1:{port}");

    // Lightweight in-test WS server: accept one connection, read exactly
    // one frame (must be Hello), then push an Activity frame.
    let server = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("ws handshake");

        let first = ws
            .next()
            .await
            .expect("expected a first frame from the client")
            .expect("first frame should be a valid WS message");
        let text = first.to_text().expect("first frame should be text");
        let envelope: Envelope =
            serde_json::from_str(text).expect("first frame should decode as a valid Envelope");
        let client_name = match envelope {
            Envelope::Hello { client_name } => client_name,
            other => panic!("expected Hello as the first frame, got: {other:?}"),
        };

        let activity_envelope = Envelope::Activity {
            event: sample_activity_event(),
        };
        let payload =
            serde_json::to_string(&activity_envelope).expect("serialize activity envelope");
        ws.send(Message::text(payload))
            .await
            .expect("send activity frame");

        client_name
    });

    let client = TuiWsClient::connect(&addr)
        .await
        .expect("connect should succeed against the in-test WS server");

    let client_name = timeout(Duration::from_secs(2), server)
        .await
        .expect("server task timed out waiting for the client's first frame")
        .expect("server task panicked");
    assert_eq!(
        client_name, "orchestrator-tui",
        "expected the Hello frame to name orchestrator-tui, got: {client_name}"
    );

    let received = timeout(Duration::from_secs(2), client.next_activity_event())
        .await
        .expect("timed out waiting for the pushed Activity event")
        .expect("activity-events channel closed unexpectedly");
    assert_eq!(received.run_id, "run-1", "expected the pushed ActivityEvent's run_id to arrive intact");
    assert_eq!(
        received.workflow_id, "set_timer",
        "expected the pushed ActivityEvent's workflow_id to arrive intact"
    );
}
