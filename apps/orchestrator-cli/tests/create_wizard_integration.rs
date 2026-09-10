//! Integration tests for the guided `workflow create` Q&A flow (Phase 10,
//! plan 10-01, CREATEUI-01/CREATEUI-02). Mirrors
//! `create_integration.rs`/`route_integration.rs`'s `#[path]`
//! module-inclusion trick (no `[lib]` target for `orchestrator-cli`) and
//! `route_frame_integration.rs`'s `spawn_server(workflows_dir, Some(router))`
//! shape so tests route against a stub `OllamaApi` with no live Ollama.

#[path = "../src/create_wizard.rs"]
mod create_wizard;
#[path = "../src/create.rs"]
mod create;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use shared::ProtocolFrame;
use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::router::ollama_client::OllamaApi;
use orchestrator::router::Router;
use orchestrator::{InProcessOrchestrator, RouterError, Service};

use ws_client::WsOrchestratorClient;

/// Deterministic stub `OllamaApi` -- these tests need no live Ollama server.
/// An unregistered input embeds to an empty vector (dimension mismatch
/// against any real candidate), which is exactly the "matches nothing"
/// shape, mirroring `route_frame_integration.rs::StubOllama`.
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

    // No fixture workflow in this file declares parameters, so
    // `Router::extract_params`'s zero-parameter short-circuit means this is
    // never actually called -- exists only to satisfy the trait.
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

/// Spawns an in-process daemon over `workflows_dir` with a stub-`OllamaApi`
/// router, mirroring `route_frame_integration.rs::spawn_server`. `vectors`
/// maps an exact input string to its embedding -- tests make the answered
/// intent and the routing utterance share the SAME vector so cosine
/// similarity is 1.0 and clears `MATCH_THRESHOLD` deterministically.
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

const INTENT: &str = "brew a fresh pot of coffee";

fn matching_vectors() -> HashMap<String, Vec<f32>> {
    let mut v = HashMap::new();
    v.insert(INTENT.to_string(), vec![1.0, 0.0, 0.0]);
    v
}

/// Test 1 (the tracer): a canned transcript answering id, intent, and one
/// trigger produces exit code 0 and a `<id>.md` file in the daemon's
/// workflows directory.
#[tokio::test]
async fn guided_create_produces_exit_zero_and_writes_the_md_file() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), matching_vectors()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let transcript = format!("brew_coffee\n{INTENT}\n1\n");
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &mut input, &mut out)
        .await
        .expect("create_wizard::run should not error on a valid transcript");
    let output = String::from_utf8(out).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0, "expected exit 0 for a valid transcript, got output: {output:?}");
    assert!(
        dir.path().join("brew_coffee.md").exists(),
        "expected brew_coffee.md to exist in the workflows dir"
    );
}

/// Test 2: the written `.md`'s frontmatter carries the answered intent
/// sentence and a `triggers:` list containing `cli`.
#[tokio::test]
async fn written_frontmatter_carries_intent_and_triggers() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), matching_vectors()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let transcript = format!("brew_coffee\n{INTENT}\n1\n");
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &mut input, &mut out)
        .await
        .expect("create_wizard::run should not error on a valid transcript");
    assert_eq!(code, 0);

    let contents =
        std::fs::read_to_string(dir.path().join("brew_coffee.md")).expect("expected the written .md to be readable");
    assert!(
        contents.contains("intent:") && contents.contains(INTENT),
        "expected an intent: key carrying the answered sentence, got: {contents}"
    );
    assert!(
        contents.contains("triggers:") && contents.contains("cli"),
        "expected a triggers: list containing cli, got: {contents}"
    );
}

/// Test 3 (the load-bearing one, CREATEUI-02): WITHOUT restarting or
/// re-spawning the daemon, routing the exact intent sentence just answered
/// resolves to the newly-created workflow id.
#[tokio::test]
async fn created_workflow_is_routable_against_the_same_daemon_with_no_restart() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), matching_vectors()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let transcript = format!("brew_coffee\n{INTENT}\n1\n");
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &mut input, &mut out)
        .await
        .expect("create_wizard::run should not error on a valid transcript");
    assert_eq!(code, 0);

    // No second spawn_server, no process restart, no direct Registry::load
    // call here -- routing goes through the SAME client / daemon instance.
    let route_reply = client
        .call_protocol(ProtocolFrame::RouteUtterance {
            utterance: INTENT.to_string(),
        })
        .await
        .expect("call_protocol should not error against a live in-process WS server");

    match route_reply {
        ProtocolFrame::RouteResult { matched_workflow_id, .. } => {
            assert_eq!(
                matched_workflow_id,
                Some("brew_coffee".to_string()),
                "expected the newly-created workflow to be routable with no daemon restart"
            );
        }
        other => panic!("expected Envelope::Protocol(RouteResult), got: {other:?}"),
    }
}

/// Test 4: the transcript reaching EOF before the final answer returns a
/// nonzero exit code and leaves zero `.md` files in the workflows dir.
#[tokio::test]
async fn eof_before_the_final_answer_exits_nonzero_and_writes_nothing() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), matching_vectors()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    // id and intent answered, EOF before the trigger selection.
    let transcript = format!("brew_coffee\n{INTENT}\n");
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &mut input, &mut out)
        .await
        .expect("create_wizard::run should not error on a truncated transcript");

    assert_ne!(code, 0, "expected a nonzero exit for EOF before the final answer");
    assert!(
        std::fs::read_dir(dir.path())
            .expect("workflows dir should exist")
            .filter_map(|e| e.ok())
            .all(|e| e.path().extension().and_then(|s| s.to_str()) != Some("md")),
        "expected zero .md files to be written on a truncated transcript"
    );
}
