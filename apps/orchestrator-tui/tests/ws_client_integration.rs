//! Integration test for `TuiWsClient` (05-06 Task 1; extended Phase 10 plan
//! 10-04 Task 1 for the TUI's first write capability). Included via the
//! same `#[path]` trick `orchestrator-cli/tests/ws_client_integration.rs`
//! uses (there is no `[lib]` target for `orchestrator-tui`) so these tests
//! exercise the exact client `main.rs` constructs. The CLI's own
//! `ws_client.rs` is ALSO pulled in by `#[path]` for the wire-shape-parity
//! test below, which needs both concrete client types side by side --
//! neither module references the other, so both compile independently in
//! this one test binary.
//!
//! The lightweight in-test WS server is a raw `tokio-tungstenite` accept
//! loop on a loopback `TcpListener` -- no `orchestrator` crate needed for
//! frame-shape assertions. Tests that need a REAL write path (create,
//! collision-check) spin up an in-process daemon instead, mirroring
//! `orchestrator-cli/tests/create_wizard_integration.rs::spawn_daemon`.
//!
//! Original (05-06) assertions preserved unmodified: (1) the first frame the
//! client sends is `Envelope::Hello` naming `orchestrator-tui` (D-04), and
//! (2) a server-pushed `Envelope::Activity` frame is delivered to the
//! client's activity-events receiver.

#[path = "../src/ws_client.rs"]
mod ws_client;
#[path = "../../orchestrator-cli/src/ws_client.rs"]
mod cli_ws_client;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

use shared::{
    ActivityEvent, ActivityLogEvent, ActivityPhase, ActivityStatus, CreateWorkflowRequest,
    CreateWorkflowResponse, Envelope, IntentCollisionChecker, ListWorkflowsResponse,
    ResponsePayload, WorkflowCreator, WorkflowWriteMode,
};

use orchestrator::router::ollama_client::OllamaApi;
use orchestrator::router::Router;
use orchestrator::{InProcessOrchestrator, RouterError, Service};

use ws_client::TuiWsClient;

fn sample_activity_event() -> ActivityEvent {
    ActivityEvent {
        run_id: "run-1".to_string(),
        workflow_id: "set_timer".to_string(),
        client_name: "orchestrator-tui".to_string(),
        session_id: "sess-1".to_string(),
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
            Envelope::Hello { client_name, .. } => client_name,
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

// -----------------------------------------------------------------------
// Phase 10, plan 10-04, Task 1: TuiWsClient's first write capability
// -----------------------------------------------------------------------

/// Deterministic stub `OllamaApi` -- these tests need no live Ollama server,
/// mirroring `orchestrator-cli/tests/create_wizard_integration.rs::StubOllama`.
struct StubOllama {
    vectors: HashMap<String, Vec<f32>>,
}

impl StubOllama {
    fn new(vectors: HashMap<String, Vec<f32>>) -> Self {
        Self { vectors }
    }
}

#[async_trait]
impl OllamaApi for StubOllama {
    async fn embed(&self, _model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError> {
        Ok(inputs
            .iter()
            .map(|i| self.vectors.get(i).cloned().unwrap_or_default())
            .collect())
    }

    async fn generate_json(
        &self,
        _model: &str,
        _system: &str,
        _prompt: &str,
        _schema: &serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        Ok(serde_json::json!({}))
    }
}

/// Spawns a real in-process daemon over `workflows_dir` with a stub-`OllamaApi`
/// router, mirroring `create_wizard_integration.rs::spawn_daemon` exactly --
/// every test below that needs a REAL write path or a real collision check
/// uses this, never a mock `WorkflowCreator`/`IntentCollisionChecker`.
async fn spawn_daemon(workflows_dir: &Path, vectors: HashMap<String, Vec<f32>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let client: Arc<dyn OllamaApi> = Arc::new(StubOllama::new(vectors));
    let router = Arc::new(Router::new(client, "nomic-embed-text", "llama3.2:3b"));
    let orchestrator = Arc::new(InProcessOrchestrator::with_router(workflows_dir, handlers, router));
    let activity_registry = Arc::new(orchestrator::activity::ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

/// A complete, valid `CreateWorkflowRequest` for `id` -- the shared fixture
/// every write-path test below starts from.
fn sample_create_request(id: &str) -> CreateWorkflowRequest {
    CreateWorkflowRequest {
        id: id.to_string(),
        name: "Brew Coffee".to_string(),
        description: "brews a fresh pot".to_string(),
        script: None,
        parameters: Vec::new(),
        mode: WorkflowWriteMode::Create,
        agent: None,
        intent: Some("brew a fresh pot of coffee".to_string()),
        triggers: vec!["cli".to_string()],
    }
}

#[tokio::test]
async fn create_workflow_round_trip_writes_the_md_and_returns_created_true() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let client = TuiWsClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let resp = client.create_workflow(sample_create_request("brew_coffee")).await;

    assert!(resp.created, "expected created:true, got: {resp:?}");
    assert!(
        dir.path().join("brew_coffee.md").exists(),
        "expected brew_coffee.md to exist in the workflows dir"
    );
}

#[tokio::test]
async fn create_for_an_existing_id_returns_created_false_with_the_daemons_error_text() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let client = TuiWsClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let first = client.create_workflow(sample_create_request("brew_coffee")).await;
    assert!(first.created, "setup: the first create must succeed: {first:?}");

    let second = client.create_workflow(sample_create_request("brew_coffee")).await;

    assert!(!second.created, "a duplicate id must be refused, never a panic");
    assert!(
        second.error.is_some(),
        "expected the daemon's own error text to be surfaced, got: {second:?}"
    );
}

/// Lightweight raw-server capture: accepts one connection, optionally
/// consumes a leading `Hello` frame (`TuiWsClient` sends one, D-04;
/// `WsOrchestratorClient` does not), then decodes the next frame as an
/// `Envelope::Req`, replies with a synthesized `created: true` response so
/// the caller's `.await` resolves, and returns the captured request's
/// `payload` as a `serde_json::Value` for structural comparison.
async fn spawn_capture_server(expect_hello: bool) -> (String, tokio::task::JoinHandle<serde_json::Value>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let addr = format!("ws://127.0.0.1:{port}");

    let handle = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws handshake");

        if expect_hello {
            let _ = ws
                .next()
                .await
                .expect("expected a Hello frame")
                .expect("Hello frame should be a valid WS message");
        }

        let req_msg = ws
            .next()
            .await
            .expect("expected a Req frame")
            .expect("Req frame should be a valid WS message");
        let text = req_msg.to_text().expect("Req frame should be text").to_string();
        let envelope: Envelope = serde_json::from_str(&text).expect("Req frame should decode as a valid Envelope");
        let (id, payload_value) = match envelope {
            Envelope::Req { id, payload } => (id, serde_json::to_value(&payload).expect("serialize payload")),
            other => panic!("expected Envelope::Req, got: {other:?}"),
        };

        let reply = Envelope::Res {
            id,
            payload: ResponsePayload::CreateWorkflow(CreateWorkflowResponse {
                created: true,
                workflow_path: Some("/mock/workflows/brew_coffee.md".to_string()),
                script_path: None,
                error: None,
            }),
        };
        let reply_text = serde_json::to_string(&reply).expect("serialize reply");
        let _ = ws.send(Message::text(reply_text)).await;

        payload_value
    });

    (addr, handle)
}

/// Wire-shape parity (T-10 D-01): the request `TuiWsClient` sends for a
/// given `CreateWorkflowRequest` is field-for-field equal, on the wire, to
/// the one `WsOrchestratorClient` (the CLI's client) sends for the SAME
/// request -- neither surface invents a parallel request type or a bespoke
/// wire call. Compared as `serde_json::Value` (neither `RequestPayload` nor
/// `CreateWorkflowRequest` derive `PartialEq`) so this is a real structural
/// comparison of the bytes each client actually put on the wire.
#[tokio::test]
async fn wire_shape_parity_tui_and_cli_clients_build_the_identical_create_workflow_request() {
    let req = sample_create_request("brew_coffee");

    let (tui_addr, tui_handle) = spawn_capture_server(true).await;
    let tui_client = TuiWsClient::connect(&tui_addr)
        .await
        .expect("connect should succeed against the capture server");
    let _ = tui_client.create_workflow(req.clone()).await;
    let tui_payload = timeout(Duration::from_secs(2), tui_handle)
        .await
        .expect("capture server timed out")
        .expect("capture server task panicked");

    let (cli_addr, cli_handle) = spawn_capture_server(false).await;
    let cli_client = cli_ws_client::WsOrchestratorClient::connect(&cli_addr)
        .await
        .expect("connect should succeed against the capture server");
    let _ = cli_client.create_workflow(req.clone()).await;
    let cli_payload = timeout(Duration::from_secs(2), cli_handle)
        .await
        .expect("capture server timed out")
        .expect("capture server task panicked");

    assert_eq!(
        tui_payload, cli_payload,
        "the TUI and CLI clients must build field-for-field identical CreateWorkflow requests for the same answers"
    );
}

/// Unexpected-variant discipline (mirrors `ws_client.rs`'s established
/// convention): a response payload of the wrong variant yields a
/// `created: false` response carrying a descriptive error, never a panic.
#[tokio::test]
async fn create_workflow_with_an_unexpected_response_variant_yields_created_false_with_a_descriptive_error() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let addr = format!("ws://127.0.0.1:{port}");

    let server = tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws handshake");

        let _hello = ws
            .next()
            .await
            .expect("expected a Hello frame")
            .expect("Hello frame should be a valid WS message");

        let req_msg = ws
            .next()
            .await
            .expect("expected a Req frame")
            .expect("Req frame should be a valid WS message");
        let text = req_msg.to_text().expect("Req frame should be text").to_string();
        let envelope: Envelope = serde_json::from_str(&text).expect("Req frame should decode as a valid Envelope");
        let id = match envelope {
            Envelope::Req { id, .. } => id,
            other => panic!("expected Envelope::Req, got: {other:?}"),
        };

        let reply = Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(ListWorkflowsResponse {
                workflows: Vec::new(),
                warnings: Vec::new(),
            }),
        };
        let reply_text = serde_json::to_string(&reply).expect("serialize reply");
        ws.send(Message::text(reply_text)).await.expect("send reply");
    });

    let client = TuiWsClient::connect(&addr)
        .await
        .expect("connect should succeed against the in-test WS server");
    let resp = client.create_workflow(sample_create_request("brew_coffee")).await;

    timeout(Duration::from_secs(2), server)
        .await
        .expect("server task timed out")
        .expect("server task panicked");

    assert!(!resp.created, "expected created:false for an unexpected response variant, got: {resp:?}");
    let error = resp.error.expect("expected a descriptive error");
    assert!(
        error.contains("unexpected response payload"),
        "error should describe the unexpected variant: {error}"
    );
}

/// T-10-19 (the 09-03 frame-id correlation hazard, ported to `call_protocol`):
/// the daemon's own `Welcome` frame arrives at id 0 as the very first
/// server-to-client frame on every connection, before any request is ever
/// made. Issuing a `check_intent_collision` call as literally the
/// connection's first request races directly against that push -- it must
/// still resolve to its OWN `IntentCollisionResult`, never be corrupted by
/// the `Welcome`. Mirrors `orchestrator-cli/tests/route_integration.rs`'s
/// `a_route_issued_as_the_connections_first_request_still_receives_its_own_route_result`.
#[tokio::test]
async fn call_protocol_resolves_its_own_reply_even_though_a_welcome_arrives_at_id_zero_first() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let client = TuiWsClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    // No other call on this connection yet -- this IS the connection's
    // first request, racing directly against the daemon's own just-sent
    // Welcome frame.
    let report = client.check_intent_collision("brew a fresh pot of coffee").await;

    assert!(
        report.colliding_workflow_id.is_none()
            && report.colliding_intent.is_none()
            && report.similarity_score.is_none()
            && report.detail.is_none(),
        "expected a clean 'no prior workflows, no collision' report -- a value corrupted by \
         the Welcome push would surface as an 'unexpected response frame' detail instead: {report:?}"
    );
}

/// `IntentCollisionChecker` round trip: against a stub-router daemon that
/// already has a workflow whose intent embeds identically to the checked
/// intent (cosine similarity 1.0, comfortably above `MATCH_THRESHOLD`), the
/// report names the colliding workflow and its intent.
#[tokio::test]
async fn check_intent_collision_against_a_stub_router_daemon_names_the_colliding_workflow() {
    const EXISTING_INTENT: &str = "brew a fresh pot of coffee";
    const NEW_INTENT: &str = "make a fresh pot of coffee";
    let mut vectors = HashMap::new();
    vectors.insert(EXISTING_INTENT.to_string(), vec![1.0, 0.0, 0.0]);
    vectors.insert(NEW_INTENT.to_string(), vec![1.0, 0.0, 0.0]);

    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), vectors).await;
    let client = TuiWsClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let mut req = sample_create_request("brew_coffee");
    req.intent = Some(EXISTING_INTENT.to_string());
    let created = client.create_workflow(req).await;
    assert!(created.created, "setup: expected the existing workflow to be written: {created:?}");

    let report = client.check_intent_collision(NEW_INTENT).await;

    assert_eq!(
        report.colliding_workflow_id.as_deref(),
        Some("brew_coffee"),
        "expected the pre-existing workflow to be named as the collision: {report:?}"
    );
    assert_eq!(
        report.colliding_intent.as_deref(),
        Some(EXISTING_INTENT),
        "expected the colliding workflow's own intent to be carried: {report:?}"
    );
    assert!(
        report.similarity_score.unwrap_or_default() > 0.9,
        "expected a near-1.0 similarity score for identical embeddings: {report:?}"
    );
}

/// `IntentCollisionChecker` transport-failure mapping: a send failure on an
/// already-closed connection returns a report with all three collision
/// fields `None` and the failure text in `detail` -- never a panic.
#[tokio::test]
async fn check_intent_collision_transport_failure_returns_a_report_with_none_fields_and_detail() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let addr = format!("ws://127.0.0.1:{port}");
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream).await.expect("ws handshake");
        let _hello = ws.next().await; // consume Hello, then close without ever replying
        drop(ws);
        let _ = closed_tx.send(());
    });

    let client = TuiWsClient::connect(&addr)
        .await
        .expect("connect should succeed against the in-test WS server");
    closed_rx.await.expect("server task should signal it closed the connection");
    // A brief pause so the OS fully tears down the socket before the
    // client's next write attempts it -- otherwise the first write can
    // still land in the local send buffer and appear to succeed.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let report = client.check_intent_collision("brew a fresh pot of coffee").await;

    assert!(report.colliding_workflow_id.is_none());
    assert!(report.colliding_intent.is_none());
    assert!(report.similarity_score.is_none());
    assert!(
        report.detail.is_some(),
        "expected the transport failure text in detail, got: {report:?}"
    );
}
