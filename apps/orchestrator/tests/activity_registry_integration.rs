//! Wave-0 integration tests (D-01/D-02/D-05, Pitfall 5 regression guard):
//! proves a newly-connecting client is replayed the current activity
//! snapshot BEFORE it receives any live event (D-01's "replay history then
//! subscribe to live updates"), and that two clients connected at once each
//! invoking a workflow mint distinct, non-colliding run_ids (D-02/D-05) --
//! the exact Pitfall 5 regression this plan's server/mod.rs rewiring fixes.
//! Mirrors `ws_server_integration.rs`'s in-process daemon harness.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use shared::ActivityStatus;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use orchestrator::activity::ActivityRegistry;
use orchestrator::handlers::timers::TimerService;
use orchestrator::{
    Envelope, InProcessOrchestrator, InvokeWorkflowRequest, RequestPayload, ResponsePayload,
    Service,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
/// directories the fixture requests. Mirrors `ws_server_integration.rs`.
fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

/// Binds `server::serve` to an OS-assigned ephemeral loopback port against
/// an `InProcessOrchestrator` built from `workflows_dir`/`handlers`, backed
/// by the given `activity_registry`, and returns the bound port. Shared by
/// `spawn_server` (fresh in-memory registry) and the daemon-restart test
/// below, which needs to control the registry's construction directly
/// (`ActivityRegistry::from_store`) to simulate two daemon instances
/// sharing one on-disk activity directory (D-12).
async fn spawn_server_with_registry(
    workflows_dir: &Path,
    handlers: HashMap<String, Box<dyn Service>>,
    activity_registry: Arc<ActivityRegistry>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let orchestrator = Arc::new(InProcessOrchestrator::with_handlers(workflows_dir, handlers));
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

/// Binds `server::serve` to an OS-assigned ephemeral loopback port against
/// an `InProcessOrchestrator` built from `workflows_dir`/`handlers`, backed
/// by a fresh in-memory `ActivityRegistry`, and returns the bound port.
async fn spawn_server(workflows_dir: &Path, handlers: HashMap<String, Box<dyn Service>>) -> u16 {
    spawn_server_with_registry(workflows_dir, handlers, Arc::new(ActivityRegistry::new())).await
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

async fn hello(ws: &mut WsStream, client_name: &str) {
    send(
        ws,
        &Envelope::Hello {
            client_name: client_name.to_string(),
        },
    )
    .await;
}

/// Receives the next raw frame, with no filtering -- unlike
/// `ws_server_integration.rs`'s `recv`, these tests deliberately want to
/// observe `Envelope::Activity` frames, not skip them.
async fn recv(ws: &mut WsStream) -> Envelope {
    let msg = ws
        .next()
        .await
        .expect("expected a frame before the stream ended")
        .expect("expected a valid WS message");
    let text = msg.to_text().expect("expected a text frame");
    serde_json::from_str(text).expect("expected a valid Envelope")
}

/// Receives frames until an `Envelope::Res` arrives, skipping any
/// `Envelope::Activity` broadcasts interleaved ahead of it (D-01: the
/// invoking connection is registered for broadcast before its own request
/// is dispatched, so it can observe its own Invoked activity too).
async fn recv_res(ws: &mut WsStream) -> Envelope {
    loop {
        let envelope = recv(ws).await;
        if matches!(envelope, Envelope::Res { .. }) {
            return envelope;
        }
    }
}

fn invoke_async_timer_req(id: u64) -> Envelope {
    Envelope::Req {
        id,
        payload: RequestPayload::InvokeWorkflow(InvokeWorkflowRequest {
            workflow_id: "async_timer".to_string(),
            payload: json!({"duration_minutes": 1}),
        }),
    }
}

#[tokio::test]
async fn replay_burst_precedes_live_events_for_a_newly_connecting_client() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(300))),
    );
    let port = spawn_server(dir.path(), handlers).await;

    // Client A starts a run that stays in flight for a while.
    let mut client_a = connect(port).await;
    hello(&mut client_a, "orchestrator-cli").await;
    send(&mut client_a, &invoke_async_timer_req(1)).await;

    let run_id = match recv_res(&mut client_a).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on the Started ack"),
        other => panic!("expected a Res ack, got: {other:?}"),
    };

    // Client B connects WHILE the run is still in flight -- the very first
    // frame it receives must be the replay burst (D-01), not yet a live
    // update, proving replay-then-subscribe ordering rather than a lucky
    // race.
    let mut client_b = connect(port).await;
    hello(&mut client_b, "orchestrator-tui").await;

    match recv(&mut client_b).await {
        Envelope::Activity { event } => {
            assert_eq!(
                event.run_id, run_id,
                "expected the replay burst to carry client A's in-flight run"
            );
            assert_ne!(
                event.status,
                ActivityStatus::Success,
                "expected the replayed snapshot to still be in-flight (not yet terminal) -- \
                 proves this is the pre-registration replay burst, not a coincidental live event"
            );
        }
        other => panic!("expected a replayed Envelope::Activity frame first, got: {other:?}"),
    }

    // A live update for the same run must follow afterward, once
    // client B is registered for broadcast.
    let terminal = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Envelope::Activity { event } = recv(&mut client_b).await {
                if event.run_id == run_id && event.status == ActivityStatus::Success {
                    return event;
                }
            }
        }
    })
    .await
    .expect("expected a live terminal Activity event to follow the replay burst");

    assert_eq!(terminal.run_id, run_id);
}

#[tokio::test]
async fn concurrent_clients_mint_distinct_non_colliding_run_ids() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_workflow(dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);

    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let port = spawn_server(dir.path(), handlers).await;

    let mut client_a = connect(port).await;
    hello(&mut client_a, "orchestrator-cli").await;
    let mut client_b = connect(port).await;
    hello(&mut client_b, "orchestrator-tui").await;

    // Each client's own envelope id independently starts at 1 -- exactly
    // the historical Pitfall 5 collision scenario (`format!("run-{id}")`
    // derived from this per-connection id).
    send(&mut client_a, &invoke_async_timer_req(1)).await;
    send(&mut client_b, &invoke_async_timer_req(1)).await;

    let run_id_a = match recv_res(&mut client_a).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on client A's Started ack"),
        other => panic!("expected a Res ack for client A, got: {other:?}"),
    };
    let run_id_b = match recv_res(&mut client_b).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on client B's Started ack"),
        other => panic!("expected a Res ack for client B, got: {other:?}"),
    };

    assert_ne!(
        run_id_a, run_id_b,
        "expected two concurrent clients' run_ids to never collide (Pitfall 5 regression guard)"
    );
}

/// Proves the *binary-level* wiring gap this plan's Task 2 closes: two
/// separate `orchestratord` process lifetimes (simulated here as two
/// `spawn_server_with_registry` calls, each backed by its own FRESH
/// `ActivityRegistry::from_store` call against the SAME on-disk activity
/// directory) reproduce the same activity history across a restart (D-12).
/// `ActivityRegistry::from_store`'s rebuild-from-JSONL correctness is
/// already unit-proven at the library level
/// (`from_store_rebuild_reproduces_live_snapshot` in `src/activity/mod.rs`)
/// -- this test instead exercises the daemon's *startup wiring*: that a
/// fresh registry constructed from the persisted store, handed to a new
/// `server::serve` instance, actually replays the prior instance's
/// completed activity to a newly-connecting client.
#[tokio::test]
async fn daemon_restart_rebuilds_activity_history_from_the_same_on_disk_store() {
    let workflows_dir = TempDir::new().expect("failed to create workflows tempdir");
    write_workflow(workflows_dir.path(), "async_timer.md", ASYNC_TIMER_WORKFLOW);
    let activity_dir = TempDir::new().expect("failed to create activity-store tempdir");

    // "First daemon instance": backed by an on-disk store rooted at
    // `activity_dir`.
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let registry_1 = Arc::new(
        ActivityRegistry::from_store(activity_dir.path(), 7)
            .expect("failed to construct the first from_store-backed registry"),
    );
    let port_1 = spawn_server_with_registry(workflows_dir.path(), handlers, registry_1).await;

    let mut client = connect(port_1).await;
    hello(&mut client, "orchestrator-cli").await;
    send(&mut client, &invoke_async_timer_req(1)).await;
    let run_id = match recv_res(&mut client).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => resp.run_id.expect("expected a Some(run_id) on the Started ack"),
        other => panic!("expected a Res ack, got: {other:?}"),
    };

    // Wait for the run to reach a terminal state before "restarting" so the
    // persisted history includes the full Invoked/Running/Completed chain.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Envelope::Activity { event } = recv(&mut client).await {
                if event.run_id == run_id && event.status == ActivityStatus::Success {
                    return;
                }
            }
        }
    })
    .await
    .expect("expected the first instance's run to reach a terminal state before restart");

    drop(client);

    // "Second daemon instance": a FRESH `ActivityRegistry::from_store` call
    // against the SAME on-disk directory, simulating a daemon restart
    // (D-12) -- the exact rebuild step orchestratord's startup must
    // perform.
    let mut handlers_2: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers_2.insert(
        "timers.start".to_string(),
        Box::new(TimerService::with_unit(Duration::from_millis(50))),
    );
    let registry_2 = Arc::new(
        ActivityRegistry::from_store(activity_dir.path(), 7)
            .expect("failed to construct the restarted from_store-backed registry"),
    );
    let port_2 = spawn_server_with_registry(workflows_dir.path(), handlers_2, registry_2).await;

    let mut restarted_client = connect(port_2).await;
    hello(&mut restarted_client, "orchestrator-tui").await;

    let replayed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Envelope::Activity { event } = recv(&mut restarted_client).await {
                if event.run_id == run_id {
                    return event;
                }
            }
        }
    })
    .await
    .expect(
        "expected the restarted instance's replay burst to include the completed run from \
         before restart (D-12)",
    );

    assert_eq!(
        replayed.status,
        ActivityStatus::Success,
        "expected the rebuilt registry to reproduce the completed activity's terminal status"
    );
}
