//! End-to-end proof of the dry-run router trial surface (plan 09-03,
//! D-02/D-03, ROUT-01/ROUT-04): a `RouteUtterance` frame sent over a real WS
//! connection to an in-process daemon returns a `RouteResult` frame
//! correlated by `id`, with every failure mode (no match, no router
//! configured, unreachable Ollama) degrading to a normal reply rather than
//! an error or a dropped connection. Mirrors
//! `describe_run_integration.rs`'s in-process daemon harness (`spawn_server`,
//! `connect`, `send`, `recv`, `hello`) wholesale.
//!
//! Task 3 extends this file with the dispatch-prohibition tests (D-03's
//! guarantee that the trial surface never starts a run).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use orchestrator::{
    Envelope, InProcessOrchestrator, ListWorkflowsRequest, RequestPayload, ResponsePayload,
    RouterError, Service,
};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CALENDAR_WORKFLOW: &str = r#"---
id: calendar_today
name: Calendar Today
parameters: {}
service:
  type: integration
  handler: process.calendar_today
intent: "check today's calendar events"
---
Calendar fixture.
"#;

const TIMER_WORKFLOW: &str = r#"---
id: set_timer
name: Set Timer
parameters:
  duration_minutes:
    type: int
    required: true
service:
  type: action
  handler: timers.start
intent: "set a countdown timer"
---
Timer fixture.
"#;

fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

fn write_fixture_workflows(dir: &Path) {
    write_workflow(dir, "calendar_today.md", CALENDAR_WORKFLOW);
    write_workflow(dir, "set_timer.md", TIMER_WORKFLOW);
}

/// Deterministic stub `OllamaApi` -- these tests need no live Ollama server.
/// An unregistered input embeds to an empty vector, which
/// `cosine_similarity` refuses to compare against any real (non-empty)
/// candidate embedding (dimension mismatch) -- this is exactly the
/// "matches nothing" shape, with no separate orthogonal vector to maintain.
struct StubOllama {
    vectors: HashMap<String, Vec<f32>>,
    embed_calls: AtomicUsize,
}

impl StubOllama {
    fn new(vectors: HashMap<String, Vec<f32>>) -> Self {
        Self {
            vectors,
            embed_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl OllamaApi for StubOllama {
    async fn embed(&self, _model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError> {
        self.embed_calls.fetch_add(1, Ordering::SeqCst);
        Ok(inputs
            .iter()
            .map(|i| self.vectors.get(i).cloned().unwrap_or_default())
            .collect())
    }
}

fn distinguishable_vectors() -> HashMap<String, Vec<f32>> {
    let mut v = HashMap::new();
    v.insert("check today's calendar events".to_string(), vec![1.0, 0.0, 0.0]);
    v.insert("set a countdown timer".to_string(), vec![0.0, 1.0, 0.0]);
    v.insert("what's on my calendar today".to_string(), vec![0.9, 0.1, 0.0]);
    v
}

fn stub_router(vectors: HashMap<String, Vec<f32>>) -> Arc<Router> {
    let client: Arc<dyn OllamaApi> = Arc::new(StubOllama::new(vectors));
    Arc::new(Router::new(client, "nomic-embed-text"))
}

/// Spawns an in-process daemon over `workflows_dir`. `router: None` mirrors
/// a daemon that never resolved the router flags at all -- exercises the
/// "router not configured" degrade path (Task 1's own behaviour bullet).
async fn spawn_server(workflows_dir: &Path, router: Option<Arc<Router>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let orchestrator = match router {
        Some(router) => Arc::new(InProcessOrchestrator::with_router(workflows_dir, handlers, router)),
        None => Arc::new(InProcessOrchestrator::with_handlers(workflows_dir, handlers)),
    };
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

/// Receives the next frame, skipping `Envelope::Activity` broadcasts and the
/// connection's own `Welcome` frame -- mirrors
/// `describe_run_integration.rs::recv`'s Activity-skipping, extended since
/// this file's tests never assert on `Welcome` directly.
async fn recv(ws: &mut WsStream) -> Envelope {
    loop {
        let msg = ws
            .next()
            .await
            .expect("expected a frame before the stream ended")
            .expect("expected a valid WS message");
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
async fn route_utterance_frame_correlates_to_route_result_by_the_same_id() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let port = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 42,
            frame: ProtocolFrame::RouteUtterance {
                utterance: "what's on my calendar today".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            id,
            frame: ProtocolFrame::RouteResult { .. },
        } => {
            assert_eq!(id, 42, "expected the RouteResult to correlate by the same id as the request");
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn a_matching_utterance_returns_the_expected_workflow_and_a_similarity_score() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let port = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::RouteUtterance {
                utterance: "what's on my calendar today".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::RouteResult {
                    matched_workflow_id,
                    similarity_score,
                    confirm_tier,
                    ..
                },
            ..
        } => {
            assert_eq!(matched_workflow_id, Some("calendar_today".to_string()));
            assert!(similarity_score.is_some(), "expected Some(similarity_score) for a real match");
            assert_eq!(confirm_tier.as_deref(), Some("route_freely"));
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn an_utterance_that_matches_nothing_returns_a_normal_reply_with_a_detail() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let port = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::RouteUtterance {
                utterance: "completely unrelated gibberish".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::RouteResult {
                    matched_workflow_id,
                    similarity_score,
                    detail,
                    ..
                },
            ..
        } => {
            assert_eq!(matched_workflow_id, None);
            assert_eq!(similarity_score, None);
            assert!(
                detail.is_some_and(|d| !d.is_empty()),
                "expected a non-empty detail explaining the refusal"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn a_daemon_without_a_router_configured_answers_with_a_detail_naming_that() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let port = spawn_server(dir.path(), None).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::RouteUtterance {
                utterance: "anything at all".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::RouteResult {
                    matched_workflow_id,
                    detail,
                    ..
                },
            ..
        } => {
            assert_eq!(matched_workflow_id, None);
            let detail = detail.expect("expected a detail explaining the degraded router");
            assert!(
                detail.to_lowercase().contains("router"),
                "expected the detail to name the router as the cause, got: {detail:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn a_daemon_with_unreachable_ollama_replies_with_a_detail_and_keeps_the_connection_alive() {
    // Reserve then drop a listener to get a very-likely-free, definitely
    // closed port -- mirrors `ws_client_integration.rs`'s dead-port pattern.
    let reserved = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port to reserve a closed one");
    let closed_port = reserved.local_addr().expect("failed to read local_addr").port();
    drop(reserved);

    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let base_url = format!("http://127.0.0.1:{closed_port}");
    let client: Arc<dyn OllamaApi> = Arc::new(HttpOllamaClient::new(base_url));
    let router = Arc::new(Router::new(client, "nomic-embed-text"));
    let port = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::RouteUtterance {
                utterance: "what's on my calendar today".to_string(),
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Protocol {
            frame:
                ProtocolFrame::RouteResult {
                    matched_workflow_id,
                    detail,
                    ..
                },
            ..
        } => {
            assert_eq!(matched_workflow_id, None);
            let detail = detail.expect("expected a detail naming the unreachable endpoint");
            assert!(
                detail.contains(&closed_port.to_string()) || detail.to_lowercase().contains("connect"),
                "expected the detail to name the unreachable endpoint, got: {detail:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }

    // Prove the connection survived: a second, unrelated frame still gets a
    // reply on the SAME connection.
    send(
        &mut ws,
        &Envelope::Req {
            id: 2,
            payload: RequestPayload::ListWorkflows(ListWorkflowsRequest {}),
        },
    )
    .await;
    match recv(&mut ws).await {
        Envelope::Res {
            id,
            payload: ResponsePayload::ListWorkflows(_),
        } => {
            assert_eq!(id, 2, "expected the connection to still answer a subsequent request");
        }
        other => panic!(
            "expected the connection to survive an unreachable-Ollama RouteUtterance, got: {other:?}"
        ),
    }
}
