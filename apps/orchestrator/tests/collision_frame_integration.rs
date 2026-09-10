//! End-to-end proof of the save-time semantic-collision wire seam (Phase 10
//! plan 10-02, D-03/D-04): a `CheckIntentCollision` frame sent over a real WS
//! connection to an in-process daemon returns an `IntentCollisionResult`
//! frame correlated by `id`, with every failure mode (router not configured,
//! unreachable Ollama) degrading to a normal reply rather than an error or a
//! dropped connection. Mirrors `route_frame_integration.rs`'s in-process
//! daemon harness (`spawn_server`, `connect`, `send`, `recv`, `hello`)
//! wholesale -- including its read-only-guarantee convention of asserting
//! against real `ActivityRegistry` state, never trusting the wire reply
//! alone.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use shared::ProtocolFrame;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use orchestrator::activity::ActivityRegistry;
use orchestrator::router::ollama_client::{HttpOllamaClient, OllamaApi};
use orchestrator::router::Router;
use orchestrator::{Envelope, InProcessOrchestrator, RouterError, Service};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const COFFEE_WORKFLOW: &str = r#"---
id: make_coffee
name: Make Coffee
parameters: {}
service:
  type: action
  handler: action.immediate
intent: "make a pot of coffee"
---
Coffee fixture.
"#;

fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

fn write_fixture_workflows(dir: &Path) {
    write_workflow(dir, "make_coffee.md", COFFEE_WORKFLOW);
}

/// Deterministic stub `OllamaApi` -- these tests need no live Ollama server.
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
        Ok(inputs.iter().map(|i| self.vectors.get(i).cloned().unwrap_or_default()).collect())
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

fn stub_router(vectors: HashMap<String, Vec<f32>>) -> Arc<Router> {
    let client: Arc<dyn OllamaApi> = Arc::new(StubOllama::new(vectors));
    Arc::new(Router::new(client, "nomic-embed-text", "llama3.2:3b"))
}

/// Spawns an in-process daemon over `workflows_dir`, mirroring
/// `route_frame_integration.rs::spawn_server` exactly, including its
/// `Arc<ActivityRegistry>` return so the read-only-guarantee tests below can
/// snapshot real daemon state directly, not just the wire reply.
async fn spawn_server(workflows_dir: &Path, router: Option<Arc<Router>>) -> (u16, Arc<ActivityRegistry>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener.local_addr().expect("failed to read the bound local_addr").port();
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let orchestrator = match router {
        Some(router) => Arc::new(InProcessOrchestrator::with_router(workflows_dir, handlers, router)),
        None => Arc::new(InProcessOrchestrator::with_handlers(workflows_dir, handlers)),
    };
    let activity_registry = Arc::new(ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, Arc::clone(&activity_registry)));
    (port, activity_registry)
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

/// Receives the next frame, skipping `Envelope::Activity` broadcasts and the
/// connection's own `Welcome` frame -- mirrors
/// `route_frame_integration.rs::recv`.
async fn recv(ws: &mut WsStream) -> Envelope {
    loop {
        let msg = ws.next().await.expect("expected a frame before the stream ended").expect("expected a valid WS message");
        let text = msg.to_text().expect("expected a text frame");
        let envelope: Envelope = serde_json::from_str(text).expect("expected a valid Envelope");
        match envelope {
            Envelope::Activity { .. } => continue,
            Envelope::Protocol {
                frame: ProtocolFrame::Welcome { .. },
                ..
            } => continue,
            other => return other,
        }
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

#[tokio::test]
async fn check_intent_collision_frame_correlates_to_intent_collision_result_by_the_same_id() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let mut vectors = HashMap::new();
    vectors.insert("make a pot of coffee".to_string(), vec![1.0, 0.0, 0.0]);
    vectors.insert("brew a fresh pot of coffee".to_string(), vec![0.99, 0.01, 0.0]);
    let router = stub_router(vectors);
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 42,
            frame: ProtocolFrame::CheckIntentCollision {
                intent: "brew a fresh pot of coffee".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            id,
            frame: ProtocolFrame::IntentCollisionResult { .. },
        } => {
            assert_eq!(id, 42, "expected the IntentCollisionResult to correlate by the same id as the request");
        }
        other => panic!("expected Envelope::Protocol(IntentCollisionResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn a_colliding_intent_names_the_colliding_workflow_id_intent_and_score() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let mut vectors = HashMap::new();
    vectors.insert("make a pot of coffee".to_string(), vec![1.0, 0.0, 0.0]);
    vectors.insert("brew a fresh pot of coffee".to_string(), vec![0.99, 0.01, 0.0]);
    let router = stub_router(vectors);
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::CheckIntentCollision {
                intent: "brew a fresh pot of coffee".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::IntentCollisionResult {
                    colliding_workflow_id,
                    colliding_intent,
                    similarity_score,
                    detail,
                    ..
                },
            ..
        } => {
            assert_eq!(colliding_workflow_id, Some("make_coffee".to_string()));
            assert_eq!(colliding_intent, Some("make a pot of coffee".to_string()));
            assert!(similarity_score.is_some(), "expected Some(similarity_score) for a detected collision");
            assert_eq!(detail, None, "expected no detail on a clean detected-collision reply");
        }
        other => panic!("expected Envelope::Protocol(IntentCollisionResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn a_distinguishable_intent_reports_no_collision_with_no_detail() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let mut vectors = HashMap::new();
    vectors.insert("make a pot of coffee".to_string(), vec![1.0, 0.0, 0.0]);
    vectors.insert("water the garden".to_string(), vec![0.0, 1.0, 0.0]);
    let router = stub_router(vectors);
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::CheckIntentCollision {
                intent: "water the garden".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::IntentCollisionResult {
                    colliding_workflow_id,
                    colliding_intent,
                    similarity_score,
                    detail,
                    ..
                },
            ..
        } => {
            assert_eq!(colliding_workflow_id, None);
            assert_eq!(colliding_intent, None);
            assert_eq!(similarity_score, None);
            assert_eq!(detail, None, "expected a clean no-collision reply, not a degrade detail");
        }
        other => panic!("expected Envelope::Protocol(IntentCollisionResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn a_daemon_without_a_router_configured_answers_with_a_detail_naming_that() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let (port, _activity_registry) = spawn_server(dir.path(), None).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::CheckIntentCollision {
                intent: "anything at all".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::IntentCollisionResult {
                    colliding_workflow_id,
                    detail,
                    ..
                },
            ..
        } => {
            assert_eq!(colliding_workflow_id, None);
            let detail = detail.expect("expected a detail explaining the degraded router");
            assert!(
                detail.to_lowercase().contains("router"),
                "expected the detail to name the router as the cause, got: {detail:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(IntentCollisionResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn a_daemon_with_unreachable_ollama_replies_with_a_detail_and_keeps_the_connection_alive() {
    // Reserve then drop a listener to get a very-likely-free, definitely
    // closed port -- mirrors `route_frame_integration.rs`'s dead-port
    // pattern.
    let reserved = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port to reserve a closed one");
    let closed_port = reserved.local_addr().expect("failed to read local_addr").port();
    drop(reserved);

    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let base_url = format!("http://127.0.0.1:{closed_port}");
    let client: Arc<dyn OllamaApi> = Arc::new(HttpOllamaClient::new(base_url));
    let router = Arc::new(Router::new(client, "nomic-embed-text", "llama3.2:3b"));
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::CheckIntentCollision {
                intent: "brew a fresh pot of coffee".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::IntentCollisionResult {
                    colliding_workflow_id,
                    detail,
                    ..
                },
            ..
        } => {
            assert_eq!(colliding_workflow_id, None);
            let detail = detail.expect("expected a detail naming the unreachable endpoint");
            assert!(
                detail.contains(&closed_port.to_string()) || detail.to_lowercase().contains("connect"),
                "expected the detail to name the unreachable endpoint, got: {detail:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(IntentCollisionResult), got: {other:?}"),
    }

    // Prove the connection survived: a second, unrelated frame still gets a
    // reply on the SAME connection.
    send(
        &mut ws,
        &Envelope::Req {
            id: 2,
            payload: shared::RequestPayload::ListWorkflows(shared::ListWorkflowsRequest {}),
        },
    )
    .await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: shared::ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(id, 2, "expected the connection to still answer a subsequent request");
        }
        other => panic!(
            "expected the connection to survive an unreachable-Ollama CheckIntentCollision, got: {other:?}"
        ),
    }
}

#[tokio::test]
async fn the_activity_registry_snapshot_is_byte_identical_before_and_after_a_detected_collision() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let mut vectors = HashMap::new();
    vectors.insert("make a pot of coffee".to_string(), vec![1.0, 0.0, 0.0]);
    vectors.insert("brew a fresh pot of coffee".to_string(), vec![0.99, 0.01, 0.0]);
    let router = stub_router(vectors);
    let (port, activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    let before = activity_registry.snapshot();
    assert!(before.is_empty(), "expected an empty snapshot before any check call, got: {before:?}");

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::CheckIntentCollision {
                intent: "brew a fresh pot of coffee".to_string(),
            },
        },
    )
    .await;
    match recv(&mut ws).await {
        Envelope::Protocol {
            frame: ProtocolFrame::IntentCollisionResult { colliding_workflow_id, .. },
            ..
        } => {
            assert_eq!(colliding_workflow_id, Some("make_coffee".to_string()));
        }
        other => panic!("expected Envelope::Protocol(IntentCollisionResult), got: {other:?}"),
    }

    let after = activity_registry.snapshot();
    let before_json = serde_json::to_value(&before).expect("serialize before snapshot");
    let after_json = serde_json::to_value(&after).expect("serialize after snapshot");
    assert_eq!(
        before_json, after_json,
        "expected the activity snapshot to be byte-identical after a collision check -- a started \
         run always appears here, so an unchanged snapshot proves nothing was started (D-03/D-04)"
    );
    assert!(after.is_empty(), "expected the snapshot to still be empty, got: {after:?}");
}

#[tokio::test]
async fn a_client_sent_intent_collision_result_is_rejected_and_the_connection_survives_for_a_subsequent_check() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let mut vectors = HashMap::new();
    vectors.insert("make a pot of coffee".to_string(), vec![1.0, 0.0, 0.0]);
    vectors.insert("brew a fresh pot of coffee".to_string(), vec![0.99, 0.01, 0.0]);
    let router = stub_router(vectors);
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    // `IntentCollisionResult` is server-push-only -- a client sending one is
    // a protocol misuse, matching every other server-push-only arm's
    // treatment (mirrors `RouteResult`'s equivalent test, T-09-11).
    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::IntentCollisionResult {
                intent: "forged".to_string(),
                colliding_workflow_id: None,
                colliding_intent: None,
                similarity_score: None,
                detail: None,
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Res {
            payload: shared::ResponsePayload::InvokeWorkflow(resp),
            ..
        } => {
            let error = resp.error.expect("expected an error detail on the rejection reply");
            assert!(
                error.to_lowercase().contains("unexpected") && error.contains("intent_collision_result"),
                "expected an UnexpectedFrameType error naming intent_collision_result, got: {error:?}"
            );
        }
        other => panic!("expected a graceful error Res, got: {other:?}"),
    }

    // The connection must survive: a subsequent, legitimate
    // CheckIntentCollision still succeeds on the SAME connection.
    send(
        &mut ws,
        &Envelope::Protocol {
            id: 2,
            frame: ProtocolFrame::CheckIntentCollision {
                intent: "brew a fresh pot of coffee".to_string(),
            },
        },
    )
    .await;
    match recv(&mut ws).await {
        Envelope::Protocol {
            frame: ProtocolFrame::IntentCollisionResult { colliding_workflow_id, .. },
            ..
        } => {
            assert_eq!(
                colliding_workflow_id,
                Some("make_coffee".to_string()),
                "expected the connection to still serve a legitimate CheckIntentCollision after the rejection"
            );
        }
        other => panic!("expected Envelope::Protocol(IntentCollisionResult), got: {other:?}"),
    }
}
