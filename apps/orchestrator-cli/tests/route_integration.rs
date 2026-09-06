//! Integration tests for the `orchestrator route "<utterance>"` dry-run
//! trial surface (plan 09-03, D-02/D-03). Mirrors `ws_client_integration.rs`'s
//! `#[path]` module-inclusion trick (there is no `[lib]` target for
//! `orchestrator-cli`) so these tests exercise the exact `route::run`
//! function `main.rs` calls, plus `run_integration.rs`'s real-binary harness
//! for the transport-failure case (which is a `main.rs`-level behavior, not
//! `route.rs`'s own).

#[path = "../src/route.rs"]
mod route;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::router::ollama_client::OllamaApi;
use orchestrator::router::Router;
use orchestrator::{InProcessOrchestrator, RouterError, Service};

use ws_client::WsOrchestratorClient;

fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

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

const SUMMARIZE_WORKFLOW: &str = r#"---
id: ai_summarize
name: AI Summarize
parameters: {}
service:
  type: agent
  handler: agent.claude
intent: "summarize a document using an AI agent"
---
AI summarize fixture (agent-type -- confirm_required).
"#;

fn write_fixture_workflows(dir: &Path) {
    write_workflow(dir, "calendar_today.md", CALENDAR_WORKFLOW);
    write_workflow(dir, "ai_summarize.md", SUMMARIZE_WORKFLOW);
}

/// Deterministic stub `OllamaApi` -- these tests need no live Ollama server.
/// An unregistered input embeds to an empty vector, which `cosine_similarity`
/// refuses to compare against any real candidate (dimension mismatch) --
/// exactly the "matches nothing" shape.
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
    v.insert(
        "summarize a document using an AI agent".to_string(),
        vec![0.0, 1.0, 0.0],
    );
    v.insert("what's on my calendar today".to_string(), vec![0.9, 0.1, 0.0]);
    v.insert(
        "please summarize this document for me".to_string(),
        vec![0.0, 0.95, 0.05],
    );
    v
}

fn stub_router(vectors: HashMap<String, Vec<f32>>) -> Arc<Router> {
    let client: Arc<dyn OllamaApi> = Arc::new(StubOllama::new(vectors));
    Arc::new(Router::new(client, "nomic-embed-text"))
}

/// Binds an ephemeral loopback WS server in-process (never a spawned
/// `orchestratord` subprocess) wrapping an `InProcessOrchestrator::with_router`
/// built from `workflows_dir`, and returns the bound port.
async fn spawn_test_daemon(workflows_dir: &Path, router: Arc<Router>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let orchestrator = Arc::new(InProcessOrchestrator::with_router(workflows_dir, handlers, router));
    let activity_registry = Arc::new(orchestrator::activity::ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

#[tokio::test]
async fn route_run_prints_the_matched_workflow_score_and_tier() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let port = spawn_test_daemon(dir.path(), stub_router(distinguishable_vectors())).await;

    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let mut buf = Vec::new();
    let code = route::run(&client, "what's on my calendar today", &mut buf)
        .await
        .expect("route::run should not error on a valid reply");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0, "expected exit 0 for a successful trial, got output: {output:?}");
    assert!(
        output.contains("calendar_today"),
        "expected the matched workflow id in the output, got: {output:?}"
    );
    assert!(
        output.to_lowercase().contains("similarity score"),
        "expected a similarity score line, got: {output:?}"
    );
    assert!(
        output.to_lowercase().contains("confirmation tier"),
        "expected a confirmation tier line, got: {output:?}"
    );
    assert!(
        output.contains("route_freely"),
        "expected the route_freely tier for a benign workflow, got: {output:?}"
    );
}

#[tokio::test]
async fn a_no_match_reply_prints_the_detail_and_exits_zero() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let port = spawn_test_daemon(dir.path(), stub_router(distinguishable_vectors())).await;

    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let mut buf = Vec::new();
    let code = route::run(&client, "completely unrelated gibberish", &mut buf)
        .await
        .expect("route::run should not error on a no-match reply");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0, "a refusal is a successful trial, not a CLI error -- got output: {output:?}");
    assert!(
        output.contains("(no match)"),
        "expected an explicit no-match marker, got: {output:?}"
    );
    assert!(
        output.to_lowercase().contains("detail:"),
        "expected the detail line to be printed, got: {output:?}"
    );
}

#[tokio::test]
async fn a_confirmation_required_match_prints_the_tier_and_no_invocation_instruction() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let port = spawn_test_daemon(dir.path(), stub_router(distinguishable_vectors())).await;

    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let mut buf = Vec::new();
    let code = route::run(&client, "please summarize this document for me", &mut buf)
        .await
        .expect("route::run should not error on a confirm-required match");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0);
    assert!(
        output.contains("confirm_required"),
        "expected the confirm_required tier printed prominently, got: {output:?}"
    );
    assert!(
        !output.contains("orchestrator run") && !output.to_lowercase().contains("invoke"),
        "expected no invocation instruction of any kind, got: {output:?}"
    );
}

#[tokio::test]
async fn the_output_states_this_is_a_dry_run_that_starts_nothing() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let port = spawn_test_daemon(dir.path(), stub_router(distinguishable_vectors())).await;

    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let mut buf = Vec::new();
    route::run(&client, "what's on my calendar today", &mut buf)
        .await
        .expect("route::run should not error");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    let lower = output.to_lowercase();
    assert!(
        lower.contains("dry run") && lower.contains("nothing was started"),
        "expected the output to state this is a dry run that starts nothing, got: {output:?}"
    );
}

/// T-09-12, the frame-id correlation hazard: the server-pushed `Welcome`
/// frame arrives with id 0 as the very FIRST server-to-client frame on every
/// connection, before any request is made. A route issued as the
/// connection's very first request must still resolve to its OWN
/// `RouteResult`, never accidentally consuming the `Welcome`.
#[tokio::test]
async fn a_route_issued_as_the_connections_first_request_still_receives_its_own_route_result() {
    let dir = TempDir::new().expect("failed to create tempdir");
    write_fixture_workflows(dir.path());
    let port = spawn_test_daemon(dir.path(), stub_router(distinguishable_vectors())).await;

    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    // No Hello, no other request -- this IS the connection's first request,
    // racing directly against the daemon's own just-sent Welcome frame.
    let mut buf = Vec::new();
    let code = route::run(&client, "what's on my calendar today", &mut buf)
        .await
        .expect("route::run should not error");
    let output = String::from_utf8(buf).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0, "expected the route call to resolve to its own RouteResult, got: {output:?}");
    assert!(
        output.contains("calendar_today"),
        "expected the connection's first-ever request to still resolve correctly, got: {output:?}"
    );
}

/// Runs the built `orchestrator` binary with the given CLI args on a
/// blocking-pool thread -- mirrors `run_integration.rs`/`list_integration.rs`
/// exactly. Exercises the real clap parsing and the real process exit code
/// (D-10) end to end.
async fn run_orchestrator(args: &[&str]) -> Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        cmd.args(&args);
        cmd.output().expect("failed to run the orchestrator binary")
    })
    .await
    .expect("run_orchestrator's spawn_blocking task panicked")
}

/// A transport failure (no daemon reachable at all) prints an error naming
/// the daemon address and exits nonzero -- this is `main.rs`'s existing D-04
/// connect-failure path, exercised here for the `route` subcommand
/// specifically (mirrors `orchestrator-tui`'s own
/// `smoke_integration.rs::connect_failure_prints_actionable_message_and_exits_nonzero`).
#[tokio::test]
async fn a_transport_failure_prints_an_error_naming_the_daemon_address_and_exits_nonzero() {
    // Reserve then drop a listener to get a very-likely-free, definitely
    // closed port.
    let reserved = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port to reserve a closed one");
    let closed_port = reserved.local_addr().expect("failed to read local_addr").port();
    drop(reserved);
    let port_str = closed_port.to_string();

    let output = run_orchestrator(&["--port", &port_str, "route", "anything at all"]).await;

    assert!(
        !output.status.success(),
        "expected a nonzero exit for an unreachable daemon"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr was not valid UTF-8");
    assert!(
        stderr.contains(&closed_port.to_string()),
        "expected the error to name the daemon address (port {closed_port}), got stderr: {stderr:?}"
    );
}

#[tokio::test]
async fn route_help_prints_the_dry_run_doc_comment() {
    let output = run_orchestrator(&["route", "--help"]).await;

    assert!(output.status.success(), "expected `route --help` to exit 0");
    let stdout = String::from_utf8(output.stdout).expect("stdout was not valid UTF-8");
    assert!(
        stdout.to_lowercase().contains("dry-run") || stdout.to_lowercase().contains("dry run"),
        "expected the route command's help text to describe it as a dry run, got: {stdout:?}"
    );
}
