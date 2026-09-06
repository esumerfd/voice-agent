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

/// The one `service.type: agent` fixture (Task 3, D-03): the case that
/// would cost real money (`ai_summarize`'s real Claude spend) if the
/// dry-run guarantee ever failed.
const AGENT_WORKFLOW: &str = r#"---
id: ai_summarize
name: AI Summarize
parameters: {}
service:
  type: agent
  handler: agent.claude
intent: "summarize a document using an AI agent"
---
AI summarize fixture (agent-type -- confirm_required, D-03).
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
    write_workflow(dir, "ai_summarize.md", AGENT_WORKFLOW);
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

    fn embed_call_count(&self) -> usize {
        self.embed_calls.load(Ordering::SeqCst)
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

    // Plan 09-04 (ROUT-03) added `generate_json` to `OllamaApi`. Every
    // fixture workflow this file routes to declares zero parameters, so
    // `Router::extract_params`'s zero-parameter short-circuit means this
    // method is never actually called by any test in this file -- it exists
    // only to satisfy the trait.
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

fn distinguishable_vectors() -> HashMap<String, Vec<f32>> {
    let mut v = HashMap::new();
    v.insert("check today's calendar events".to_string(), vec![1.0, 0.0, 0.0]);
    v.insert("set a countdown timer".to_string(), vec![0.0, 1.0, 0.0]);
    v.insert(
        "summarize a document using an AI agent".to_string(),
        vec![0.0, 0.0, 1.0],
    );
    v.insert("what's on my calendar today".to_string(), vec![0.9, 0.1, 0.0]);
    v.insert(
        "please summarize this document for me".to_string(),
        vec![0.05, 0.0, 0.95],
    );
    v
}

fn stub_router(vectors: HashMap<String, Vec<f32>>) -> Arc<Router> {
    let client: Arc<dyn OllamaApi> = Arc::new(StubOllama::new(vectors));
    Arc::new(Router::new(client, "nomic-embed-text", "llama3.2:3b"))
}

/// Spawns an in-process daemon over `workflows_dir`. `router: None` mirrors
/// a daemon that never resolved the router flags at all -- exercises the
/// "router not configured" degrade path (Task 1's own behaviour bullet).
/// Returns the bound port AND the `Arc<ActivityRegistry>` the daemon was
/// built with (Task 3): the dispatch-prohibition tests snapshot this
/// directly, rather than trusting the wire reply alone, since D-03's
/// guarantee is about real daemon state, not just what the reply claims.
async fn spawn_server(workflows_dir: &Path, router: Option<Arc<Router>>) -> (u16, Arc<ActivityRegistry>) {
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
    tokio::spawn(orchestrator::server::serve(
        listener,
        orchestrator,
        Arc::clone(&activity_registry),
    ));
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
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

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
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

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
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

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
    let (port, _activity_registry) = spawn_server(dir.path(), None).await;

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
    let router = Arc::new(Router::new(client, "nomic-embed-text", "llama3.2:3b"));
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

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

// -----------------------------------------------------------------
// Task 3: prove the trial surface cannot dispatch, and bound it at the wire
// (D-03). The strongest available assertion is behavioural: capture
// `ActivityRegistry::snapshot()` before and after routing an agent-type
// match -- a started run always appears there, so an unchanged snapshot
// proves nothing was started. See `server/mod.rs::handle_route_utterance`
// for the accompanying region-scoped source guard.
// -----------------------------------------------------------------

async fn route_agent_match(ws: &mut WsStream, id: u64) -> ProtocolFrame {
    send(
        ws,
        &Envelope::Protocol {
            id,
            frame: ProtocolFrame::RouteUtterance {
                utterance: "please summarize this document for me".to_string(),
            },
        },
    )
    .await;
    match recv(ws).await {
        Envelope::Protocol { frame, .. } => frame,
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn routing_an_agent_type_match_leaves_the_activity_snapshot_byte_identical() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let (port, activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    let before = activity_registry.snapshot();
    assert!(before.is_empty(), "expected an empty snapshot before any route call, got: {before:?}");

    let frame = route_agent_match(&mut ws, 1).await;
    match &frame {
        ProtocolFrame::RouteResult { matched_workflow_id, .. } => {
            assert_eq!(
                matched_workflow_id.as_deref(),
                Some("ai_summarize"),
                "expected the agent-type workflow to be matched, got: {frame:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }

    let after = activity_registry.snapshot();
    // `ActivityEvent` derives no `PartialEq` -- compare via serialized JSON
    // (byte-identical in length and content) instead of adding a derive to
    // a shared struct outside this plan's file scope.
    let before_json = serde_json::to_value(&before).expect("serialize before snapshot");
    let after_json = serde_json::to_value(&after).expect("serialize after snapshot");
    assert_eq!(
        before_json, after_json,
        "expected the activity snapshot to be byte-identical in length and content after routing \
         an agent-type match -- a started run always appears here, so an unchanged snapshot proves \
         nothing was started (D-03)"
    );
    assert!(after.is_empty(), "expected the snapshot to still be empty, got: {after:?}");
}

#[tokio::test]
async fn an_agent_type_match_returns_confirm_required_and_carries_no_run_id() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    let frame = route_agent_match(&mut ws, 1).await;
    match &frame {
        ProtocolFrame::RouteResult { confirm_tier, .. } => {
            assert_eq!(
                confirm_tier.as_deref(),
                Some("confirm_required"),
                "expected the agent-type match to carry the confirm_required tier, got: {frame:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }

    // `RouteResult` has no `run_id` field at all (unlike `InvokeWorkflowResponse`
    // or the `Started` ack) -- structurally, not just by omission, it can
    // never carry one. Serialize to JSON as a defensive belt-and-braces
    // check against a future accidental field addition.
    let value = serde_json::to_value(&frame).expect("serialize RouteResult");
    assert!(
        value.get("run_id").is_none(),
        "expected no run_id key anywhere in the RouteResult reply, got: {value}"
    );
}

#[tokio::test]
async fn routing_the_same_agent_utterance_twenty_times_never_accumulates_activity_state() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let (port, activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    for i in 0..20 {
        let frame = route_agent_match(&mut ws, i).await;
        match &frame {
            ProtocolFrame::RouteResult { matched_workflow_id, .. } => {
                assert_eq!(matched_workflow_id.as_deref(), Some("ai_summarize"));
            }
            other => panic!("expected Envelope::Protocol(RouteResult) on iteration {i}, got: {other:?}"),
        }
    }

    let after = activity_registry.snapshot();
    assert!(
        after.is_empty(),
        "expected the activity snapshot to remain empty after 20 consecutive routes of an \
         agent-type match -- no path may accumulate state, got: {after:?}"
    );
}

#[tokio::test]
async fn a_client_sent_route_result_is_rejected_and_the_connection_survives_for_a_subsequent_route() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    // A `RouteResult` is server-push-only -- a client sending one is a
    // protocol misuse, matching every other server-push-only arm's
    // treatment (T-09-11).
    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::RouteResult {
                utterance: "forged".to_string(),
                matched_workflow_id: None,
                similarity_score: None,
                confirm_tier: None,
                extracted_params: None,
                detail: None,
            },
        },
    )
    .await;

    match recv(&mut ws).await {
        Envelope::Res {
            payload: ResponsePayload::InvokeWorkflow(resp),
            ..
        } => {
            let error = resp.error.expect("expected an error detail on the rejection reply");
            assert!(
                error.to_lowercase().contains("unexpected") && error.contains("route_result"),
                "expected an UnexpectedFrameType error naming route_result, got: {error:?}"
            );
        }
        other => panic!("expected a graceful error Res, got: {other:?}"),
    }

    // The connection must survive: a subsequent, legitimate RouteUtterance
    // still succeeds on the SAME connection.
    let frame = route_agent_match(&mut ws, 2).await;
    match frame {
        ProtocolFrame::RouteResult { matched_workflow_id, .. } => {
            assert_eq!(
                matched_workflow_id.as_deref(),
                Some("ai_summarize"),
                "expected the connection to still serve a legitimate RouteUtterance after the rejection"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}

#[tokio::test]
async fn an_over_length_utterance_names_the_limit_and_costs_zero_embed_calls() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let stub = Arc::new(StubOllama::new(HashMap::new()));
    let client: Arc<dyn OllamaApi> = stub.clone();
    let router = Arc::new(Router::new(client, "nomic-embed-text", "llama3.2:3b"));
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    let too_long = "a".repeat(orchestrator::router::threshold::MAX_UTTERANCE_CHARS + 1);
    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::RouteUtterance { utterance: too_long },
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
            let detail = detail.expect("expected a detail naming the character limit");
            assert!(
                detail.contains(&orchestrator::router::threshold::MAX_UTTERANCE_CHARS.to_string()),
                "expected the detail to name the {}-code-point limit, got: {detail:?}",
                orchestrator::router::threshold::MAX_UTTERANCE_CHARS
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }

    assert_eq!(
        stub.embed_call_count(),
        0,
        "expected an over-length utterance to be refused before any Ollama embed call"
    );
}

#[tokio::test]
async fn an_empty_utterance_names_that_it_was_empty() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let router = stub_router(distinguishable_vectors());
    let (port, _activity_registry) = spawn_server(dir.path(), Some(router)).await;

    let mut ws = connect(port).await;
    hello(&mut ws, "orchestrator-cli").await;

    send(
        &mut ws,
        &Envelope::Protocol {
            id: 1,
            frame: ProtocolFrame::RouteUtterance {
                utterance: String::new(),
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
            let detail = detail.expect("expected a detail naming the empty utterance");
            assert!(
                detail.to_lowercase().contains("empty"),
                "expected the detail to name that the utterance was empty, got: {detail:?}"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}
