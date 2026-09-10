//! Integration tests for the guided `workflow create` Q&A flow (Phase 10,
//! plan 10-01, CREATEUI-01/CREATEUI-02). Mirrors
//! `create_integration.rs`/`route_integration.rs`'s `#[path]`
//! module-inclusion trick (no `[lib]` target for `orchestrator-cli`) and
//! `route_frame_integration.rs`'s `spawn_server(workflows_dir, Some(router))`
//! shape so daemon-backed tests route against a stub `OllamaApi` with no
//! live Ollama. Most Task 2 behavior tests below use a `MockWorkflowCreator`
//! instead of a real daemon -- they assert on the *shape* of the request
//! the wizard builds, not on the write path itself (already covered by
//! `create_workflow_integration.rs` and this file's own daemon-backed
//! tests).
//!
//! Transcript note: every happy-path transcript in this file answers Steps
//! 1-9 in UI-SPEC's documented order: id, display name, description,
//! triggers, intent, handler type, [agent sub-prompts], the
//! add-a-parameter loop (terminated by a non-`y` answer), then the final
//! `[Y/n]` confirmation.

#[path = "../src/create_wizard.rs"]
mod create_wizard;
#[path = "../src/create.rs"]
mod create;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::io::Cursor;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use shared::{
    AgentConfig, CreateWorkflowRequest, CreateWorkflowResponse, IntentCollisionChecker, IntentCollisionReport,
    ParameterType, ProtocolFrame, WorkflowCreator,
};
use tempfile::TempDir;
use tokio::net::TcpListener;

use orchestrator::router::ollama_client::OllamaApi;
use orchestrator::router::Router;
use orchestrator::{InProcessOrchestrator, RouterError, Service};

use ws_client::WsOrchestratorClient;

// ---------------------------------------------------------------------
// Shared daemon-backed test harness (mirrors route_frame_integration.rs)
// ---------------------------------------------------------------------

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

/// A minimal, complete happy-path transcript: id, blank display name,
/// blank description, trigger selection `1` (cli), the given intent,
/// handler `2` (markdown-body action, so no agent sub-prompts), no
/// parameters, and a `y` final confirmation.
fn happy_path_transcript(id: &str, intent: &str) -> String {
    format!("{id}\n\n\n1\n{intent}\n2\nN\ny\n")
}

// ---------------------------------------------------------------------
// Task 1 tracer tests (transcripts updated for Task 2's full nine steps)
// ---------------------------------------------------------------------

#[tokio::test]
async fn guided_create_produces_exit_zero_and_writes_the_md_file() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), matching_vectors()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let transcript = happy_path_transcript("brew_coffee", INTENT);
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &client, &mut input, &mut out)
        .await
        .expect("create_wizard::run should not error on a valid transcript");
    let output = String::from_utf8(out).expect("captured output was not valid UTF-8");

    assert_eq!(code, 0, "expected exit 0 for a valid transcript, got output: {output:?}");
    assert!(
        dir.path().join("brew_coffee.md").exists(),
        "expected brew_coffee.md to exist in the workflows dir"
    );
}

#[tokio::test]
async fn written_frontmatter_carries_intent_and_triggers() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), matching_vectors()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let transcript = happy_path_transcript("brew_coffee", INTENT);
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &client, &mut input, &mut out)
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

    let transcript = happy_path_transcript("brew_coffee", INTENT);
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &client, &mut input, &mut out)
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

#[tokio::test]
async fn eof_before_the_final_answer_exits_nonzero_and_writes_nothing() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), matching_vectors()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    // id, blank display name, blank description answered; EOF before the
    // trigger selection.
    let transcript = "brew_coffee\n\n\n".to_string();
    let mut input = Cursor::new(transcript.into_bytes());
    let mut out = Vec::new();
    let code = create_wizard::run(&client, &client, &mut input, &mut out)
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

// ---------------------------------------------------------------------
// Task 2: mock-backed behavior tests (request-shape assertions)
// ---------------------------------------------------------------------

/// Records every `create_workflow` call and always reports success --
/// used for behavior tests that assert on the *shape* of the request the
/// wizard builds, not on the real write path (already covered above and by
/// `create_workflow_integration.rs`).
struct MockWorkflowCreator {
    calls: AtomicUsize,
    last_request: Mutex<Option<CreateWorkflowRequest>>,
}

impl MockWorkflowCreator {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            last_request: Mutex::new(None),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(AtomicOrdering::SeqCst)
    }

    fn last_request(&self) -> CreateWorkflowRequest {
        self.last_request
            .lock()
            .expect("mutex poisoned")
            .clone()
            .expect("create_workflow should have been called at least once")
    }
}

#[async_trait]
impl WorkflowCreator for MockWorkflowCreator {
    async fn create_workflow(&self, req: CreateWorkflowRequest) -> CreateWorkflowResponse {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        *self.last_request.lock().expect("mutex poisoned") = Some(req.clone());
        CreateWorkflowResponse {
            created: true,
            workflow_path: Some(format!("/mock/workflows/{}.md", req.id)),
            script_path: req.script.as_ref().map(|_| format!("/mock/workflows/scripts/{}.sh", req.id)),
            error: None,
        }
    }
}

/// A checker stub that always reports "no collision" -- used by every Task 2
/// request-shape test above, which is not exercising the collision gate
/// itself (that is Task 3's own behavior tests below).
struct NoCollisionChecker;

#[async_trait]
impl IntentCollisionChecker for NoCollisionChecker {
    async fn check_intent_collision(&self, _intent: &str) -> IntentCollisionReport {
        IntentCollisionReport {
            colliding_workflow_id: None,
            colliding_intent: None,
            similarity_score: None,
            detail: None,
        }
    }
}

/// A checker stub returning a queue of canned reports (one per call, in
/// call order; the queue's final report repeats once exhausted) -- drives
/// Task 3's multi-report scenarios (e.g. "collision found, then revised
/// intent clears it"). Also records the `WorkflowCreator` mock's own call
/// counter at the moment EACH check is invoked -- the ordering-prohibition
/// assertion (T-10-06): the creator's counter must be 0 every single time
/// the checker runs, proving there is no path from any answer to a write
/// that skips this gate.
struct StubIntentCollisionChecker<'a> {
    reports: Mutex<std::collections::VecDeque<IntentCollisionReport>>,
    creator_calls: &'a AtomicUsize,
    recorded_creator_calls_at_check: Mutex<Vec<usize>>,
}

impl<'a> StubIntentCollisionChecker<'a> {
    fn new(creator_calls: &'a AtomicUsize, reports: Vec<IntentCollisionReport>) -> Self {
        Self {
            reports: Mutex::new(reports.into()),
            creator_calls,
            recorded_creator_calls_at_check: Mutex::new(Vec::new()),
        }
    }

    fn recorded_creator_calls_at_check(&self) -> Vec<usize> {
        self.recorded_creator_calls_at_check.lock().expect("mutex poisoned").clone()
    }
}

fn no_collision_report() -> IntentCollisionReport {
    IntentCollisionReport {
        colliding_workflow_id: None,
        colliding_intent: None,
        similarity_score: None,
        detail: None,
    }
}

#[async_trait]
impl<'a> IntentCollisionChecker for StubIntentCollisionChecker<'a> {
    async fn check_intent_collision(&self, _intent: &str) -> IntentCollisionReport {
        self.recorded_creator_calls_at_check
            .lock()
            .expect("mutex poisoned")
            .push(self.creator_calls.load(AtomicOrdering::SeqCst));
        let mut queue = self.reports.lock().expect("mutex poisoned");
        if queue.len() > 1 {
            queue.pop_front().expect("checked len() > 1 above")
        } else {
            queue.front().cloned().unwrap_or_else(no_collision_report)
        }
    }
}

async fn run_wizard(transcript: &str, client: &dyn WorkflowCreator) -> (i32, String) {
    run_wizard_with_checker(transcript, client, &NoCollisionChecker).await
}

async fn run_wizard_with_checker(
    transcript: &str,
    client: &dyn WorkflowCreator,
    checker: &dyn IntentCollisionChecker,
) -> (i32, String) {
    let mut input = Cursor::new(transcript.as_bytes().to_vec());
    let mut out = Vec::new();
    let code = create_wizard::run(client, checker, &mut input, &mut out)
        .await
        .expect("create_wizard::run should not error");
    (code, String::from_utf8(out).expect("captured output was not valid UTF-8"))
}

/// Steps and order: a full transcript answering all nine steps in
/// UI-SPEC's documented order produces a request whose every field matches
/// the answers, including a collected parameter.
#[tokio::test]
async fn full_transcript_produces_a_request_matching_every_answer() {
    let mock = MockWorkflowCreator::new();
    let transcript = concat!(
        "full_flow_workflow\n", // id
        "My Custom Name\n",     // display name
        "Some description\n",  // description
        "1,2\n",                // triggers: cli + voice
        "brew a fresh pot of coffee\n", // intent
        "2\n",                  // handler: markdown-body action
        "y\n",                  // add a parameter? yes
        "dur\n",                // name
        "int\n",                // type
        "y\n",                  // required? yes
        "N\n",                  // add another parameter? no
        "y\n",                  // confirm
    );
    let (code, _output) = run_wizard(transcript, &mock).await;

    assert_eq!(code, 0);
    assert_eq!(mock.call_count(), 1);
    let req = mock.last_request();
    assert_eq!(req.id, "full_flow_workflow");
    assert_eq!(req.name, "My Custom Name");
    assert_eq!(req.description, "Some description");
    assert_eq!(req.triggers, vec!["cli".to_string(), "voice".to_string()]);
    assert_eq!(req.intent, Some("brew a fresh pot of coffee".to_string()));
    assert_eq!(req.script, None);
    assert_eq!(req.agent, None);
    assert_eq!(req.parameters.len(), 1);
    assert_eq!(req.parameters[0].name, "dur");
    assert_eq!(req.parameters[0].type_, ParameterType::Int);
    assert!(req.parameters[0].required);
}

/// D-06: selecting zero trigger types blocks advancing with the documented
/// message and does not pass a blank line through as a selection.
#[tokio::test]
async fn zero_trigger_selection_reprompts_and_does_not_advance() {
    let mock = MockWorkflowCreator::new();
    // Blank triggers answer (invalid, zero selections), then a valid "2".
    let transcript = concat!(
        "coffee_id\n", "\n", "\n", "\n", "2\n", // triggers: blank (rejected) then item 2 (voice)
        "brew a fresh pot of coffee\n", "2\n", "N\n", "y\n",
    );
    let (code, output) = run_wizard(transcript, &mock).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(
        output.contains("Select at least one trigger type."),
        "expected the zero-selection message, got: {output}"
    );
    let req = mock.last_request();
    assert_eq!(req.triggers, vec!["voice".to_string()]);
}

/// D-06: free-text trigger entry that is not a listed number is rejected
/// without being passed through as a trigger name.
#[tokio::test]
async fn free_text_trigger_entry_is_rejected_not_passed_through() {
    let mock = MockWorkflowCreator::new();
    let transcript = concat!(
        "coffee_id\n", "\n", "\n", "voice\n", "1\n", // triggers: free text "voice" (rejected) then "1" (cli)
        "brew a fresh pot of coffee\n", "2\n", "N\n", "y\n",
    );
    let (code, output) = run_wizard(transcript, &mock).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(
        output.contains("Select at least one trigger type."),
        "expected the free-text entry to be treated as zero valid selections, got: {output}"
    );
    let req = mock.last_request();
    assert_eq!(req.triggers, vec!["cli".to_string()], "free text must never become a trigger name");
}

/// D-05 parity, shape 1: choosing handler type 1 (script-backed action)
/// produces a request with `script: Some(..)` and `agent: None`.
#[tokio::test]
async fn handler_choice_one_produces_a_script_backed_request() {
    let mock = MockWorkflowCreator::new();
    let transcript = happy_path_script("script_wf", "#!/bin/sh -c 'echo hi'");
    let (code, _output) = run_wizard(&transcript, &mock).await;

    assert_eq!(code, 0);
    let req = mock.last_request();
    assert_eq!(req.script, Some("#!/bin/sh -c 'echo hi'".to_string()));
    assert_eq!(req.agent, None);
    assert_eq!(req.description, "");
}

fn happy_path_script(id: &str, script_body: &str) -> String {
    format!("{id}\n\n{script_body}\n1\n{INTENT}\n1\nN\ny\n")
}

/// D-05 parity, shape 2: choosing handler type 2 (markdown-body action)
/// produces `script: None`, `agent: None`, and the description text as the
/// body.
#[tokio::test]
async fn handler_choice_two_produces_a_markdown_body_request() {
    let mock = MockWorkflowCreator::new();
    let transcript = format!("markdown_wf\n\nSome body text\n1\n{INTENT}\n2\nN\ny\n");
    let (code, _output) = run_wizard(&transcript, &mock).await;

    assert_eq!(code, 0);
    let req = mock.last_request();
    assert_eq!(req.script, None);
    assert_eq!(req.agent, None);
    assert_eq!(req.description, "Some body text");
}

/// D-05 parity, shape 3: choosing handler type 3 (Agent) produces
/// `agent: Some(AgentConfig { .. })` and `script: None`, prompting for
/// reference files, timeout, and budget.
#[tokio::test]
async fn handler_choice_three_produces_an_agent_request() {
    let mock = MockWorkflowCreator::new();
    let transcript = concat!(
        "agent_wf\n", "\n", "Summarize this document\n", "1\n", "summarize a document\n", "3\n",
        "reference.md\n", "\n", // agent files: one entry, then blank to finish
        "30\n",                 // timeout
        "1.5\n",                // budget
        "N\n", "y\n",
    );
    let (code, _output) = run_wizard(transcript, &mock).await;

    assert_eq!(code, 0);
    let req = mock.last_request();
    assert_eq!(req.script, None);
    assert_eq!(
        req.agent,
        Some(AgentConfig {
            files: vec!["reference.md".to_string()],
            timeout_secs: Some(30),
            max_budget_usd: Some(1.5),
        })
    );
}

/// D-05 conditional fields: choosing handler type 1 or 2 never emits the
/// Step 6a/6b/6c prompt text at all.
#[tokio::test]
async fn non_agent_handlers_never_emit_agent_sub_prompts() {
    let mock = MockWorkflowCreator::new();
    let transcript = format!("markdown_wf\n\nSome body text\n1\n{INTENT}\n2\nN\ny\n");
    let (code, output) = run_wizard(&transcript, &mock).await;

    assert_eq!(code, 0);
    assert!(!output.contains("Reference files"), "got: {output}");
    assert!(!output.contains("Timeout in seconds"), "got: {output}");
    assert!(!output.contains("Max budget in USD"), "got: {output}");
}

/// Agent optional fields: answering the timeout/budget prompts with a
/// blank line leaves them `None` (the daemon default applies) rather than
/// substituting a number.
#[tokio::test]
async fn agent_blank_timeout_and_budget_stay_none() {
    let mock = MockWorkflowCreator::new();
    let transcript = concat!(
        "agent_wf\n", "\n", "Summarize this document\n", "1\n", "summarize a document\n", "3\n",
        "\n", // agent files: blank immediately -- zero files
        "\n", // timeout: blank
        "\n", // budget: blank
        "N\n", "y\n",
    );
    let (code, _output) = run_wizard(transcript, &mock).await;

    assert_eq!(code, 0);
    let req = mock.last_request();
    let agent = req.agent.expect("expected an agent config");
    assert!(agent.files.is_empty());
    assert_eq!(agent.timeout_secs, None);
    assert_eq!(agent.max_budget_usd, None);
}

/// Client-side id validation: an empty id, a 65-character id, an id
/// containing an uppercase letter, and an id starting with `-` each
/// re-prompt Step 1 with an `[ERROR]` line carrying the writer's exact
/// reason string, and none of them reaches the `WorkflowCreator` mock.
#[tokio::test]
async fn invalid_ids_reprompt_with_the_exact_reason_and_never_reach_the_mock() {
    let mock = MockWorkflowCreator::new();
    let too_long_id = "a".repeat(65);
    let transcript = format!("\n{too_long_id}\nUpperCase\n-startswithdash\n"); // then EOF
    let (code, output) = run_wizard(&transcript, &mock).await;

    assert_ne!(code, 0);
    assert_eq!(mock.call_count(), 0, "no invalid id should ever reach the mock");
    assert!(output.contains("[ERROR] id must not be empty."), "got: {output}");
    assert!(
        output.contains("[ERROR] id must be at most 64 characters."),
        "got: {output}"
    );
    assert!(
        output.contains("[ERROR] id must contain only lowercase letters, digits, `_`, and `-`."),
        "got: {output}"
    );
    assert!(
        output.contains("[ERROR] id must start with an alphanumeric character."),
        "got: {output}"
    );
}

/// Intent validation: a whitespace-only intent is rejected; an intent
/// longer than 1024 characters is rejected.
#[tokio::test]
async fn intent_validation_rejects_whitespace_only_and_over_length() {
    let mock = MockWorkflowCreator::new();
    let too_long_intent = "a".repeat(1025);
    let transcript = format!("intent_wf\n\n\n1\n   \n{too_long_intent}\n{INTENT}\n2\nN\ny\n");
    let (code, output) = run_wizard(&transcript, &mock).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(output.contains("[ERROR] intent must not be empty."), "got: {output}");
    assert!(
        output.contains("[ERROR] intent must be at most 1024 characters."),
        "got: {output}"
    );
    let req = mock.last_request();
    assert_eq!(req.intent, Some(INTENT.to_string()));
}

/// Parameter loop: a duplicate parameter name is rejected inline with the
/// documented message and only one copy is kept.
#[tokio::test]
async fn duplicate_parameter_name_is_rejected_inline() {
    let mock = MockWorkflowCreator::new();
    let transcript = concat!(
        "param_wf\n", "\n", "\n", "1\n", "brew a fresh pot of coffee\n", "2\n",
        "y\n", "dur\n", "int\n", "N\n", // first "dur" param, optional
        "y\n", "dur\n", "string\n", "N\n", // duplicate "dur" -- rejected
        "N\n", // stop adding parameters
        "y\n",
    );
    let (code, output) = run_wizard(transcript, &mock).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(output.contains("Parameter 'dur' already added."), "got: {output}");
    let req = mock.last_request();
    assert_eq!(req.parameters.len(), 1);
    assert_eq!(req.parameters[0].type_, ParameterType::Int);
}

/// Zero parameters is a valid outcome; the review summary renders the
/// documented `Parameters: none` line.
#[tokio::test]
async fn zero_parameters_renders_parameters_none_in_the_review() {
    let mock = MockWorkflowCreator::new();
    let transcript = happy_path_transcript("no_params_wf", INTENT);
    let (code, output) = run_wizard(&transcript, &mock).await;

    assert_eq!(code, 0);
    assert!(output.contains("Parameters: none"), "got: {output}");
    let req = mock.last_request();
    assert!(req.parameters.is_empty());
}

/// Review summary rendering: with two parameters collected, the summary
/// contains an indented `- ` line per parameter carrying its name, type,
/// and required/optional word, under a `Parameters:` heading.
#[tokio::test]
async fn two_parameters_render_as_indented_bullet_lines() {
    let mock = MockWorkflowCreator::new();
    let transcript = concat!(
        "two_param_wf\n", "\n", "\n", "1\n", "brew a fresh pot of coffee\n", "2\n",
        "y\n", "dur\n", "int\n", "y\n",     // dur:int:required
        "y\n", "note\n", "string\n", "N\n", // note:string:optional
        "N\n", "y\n",
    );
    let (code, output) = run_wizard(transcript, &mock).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(output.contains("Parameters:"), "got: {output}");
    assert!(output.contains("  - dur: int (required)"), "got: {output}");
    assert!(output.contains("  - note: string (optional)"), "got: {output}");
}

/// Cancellation: answering the final confirmation with `n` returns exit
/// code 0, writes nothing, and the `WorkflowCreator` mock's call counter
/// stays at 0.
#[tokio::test]
async fn declining_the_final_confirmation_writes_nothing() {
    let mock = MockWorkflowCreator::new();
    let transcript = format!("cancel_wf\n\n\n1\n{INTENT}\n2\nN\nn\n");
    let (code, _output) = run_wizard(&transcript, &mock).await;

    assert_eq!(code, 0);
    assert_eq!(mock.call_count(), 0, "declining must never call create_workflow");
}

/// Cap-mirror drift guard: each local mirror constant equals the real
/// constant it mirrors in `registry::writer` (available here as a
/// dev-dependency, even though `create_wizard.rs`'s production code cannot
/// import it directly).
#[test]
fn cap_mirrors_match_the_real_writer_constants() {
    assert_eq!(create_wizard::MAX_WORKFLOW_ID_LEN_MIRROR, orchestrator::registry::writer::MAX_WORKFLOW_ID_LEN);
    assert_eq!(
        create_wizard::MAX_WORKFLOW_NAME_LEN_MIRROR,
        orchestrator::registry::writer::MAX_WORKFLOW_NAME_LEN
    );
    assert_eq!(create_wizard::MAX_INTENT_LEN_MIRROR, orchestrator::registry::writer::MAX_INTENT_LEN);
    assert_eq!(create_wizard::MAX_TRIGGERS_MIRROR, orchestrator::registry::writer::MAX_TRIGGERS);
    assert_eq!(create_wizard::MAX_AGENT_FILES_MIRROR, orchestrator::registry::writer::MAX_AGENT_FILES);
    assert_eq!(
        create_wizard::MAX_AGENT_FILE_LEN_MIRROR,
        orchestrator::registry::writer::MAX_AGENT_FILE_LEN
    );
}

// ---------------------------------------------------------------------
// Task 3: the blocking, fail-closed Step 8 collision gate
// ---------------------------------------------------------------------

fn collision_report(id: &str, intent: &str, score: f32) -> IntentCollisionReport {
    IntentCollisionReport {
        colliding_workflow_id: Some(id.to_string()),
        colliding_intent: Some(intent.to_string()),
        similarity_score: Some(score),
        detail: None,
    }
}

fn degrade_report(detail: &str) -> IntentCollisionReport {
    IntentCollisionReport {
        colliding_workflow_id: None,
        colliding_intent: None,
        similarity_score: None,
        detail: Some(detail.to_string()),
    }
}

/// Base transcript through the end of Step 7 (parameters): id, blank
/// display name, blank description, trigger `1` (cli), the given intent,
/// handler `2` (markdown-body action), and `N` to stop adding parameters --
/// everything up to (but not including) Step 8's collision gate.
fn transcript_through_step_7(id: &str, intent: &str) -> String {
    format!("{id}\n\n\n1\n{intent}\n2\nN\n")
}

/// Progress cue: the in-progress line is rendered before any collision
/// verdict (here, the warning line for a detected collision).
#[tokio::test]
async fn progress_cue_appears_before_the_collision_warning() {
    let mock = MockWorkflowCreator::new();
    let checker =
        StubIntentCollisionChecker::new(&mock.calls, vec![collision_report("make_coffee", "make a pot of coffee", 0.9)]);
    // EOF at the save-anyway prompt -- enough to observe the progress cue
    // and the warning without completing the flow.
    let transcript = transcript_through_step_7("progress_wf", INTENT);
    let (code, output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_ne!(code, 0, "expected EOF at the save-anyway prompt, output: {output}");
    let progress_idx = output
        .find("Checking for similar existing workflows...")
        .expect("expected the progress line to be rendered");
    let warn_idx = output.find("[WARN]").expect("expected a warning line to be rendered");
    assert!(
        progress_idx < warn_idx,
        "expected the progress cue before the collision warning, got: {output}"
    );
    assert_eq!(checker.recorded_creator_calls_at_check(), vec![0], "T-10-06 ordering prohibition");
    assert_eq!(mock.call_count(), 0);
}

/// Collision found, declined by a blank answer (D-04 fail-closed): the
/// creator mock is never called.
#[tokio::test]
async fn collision_found_declined_by_blank_answer_writes_nothing() {
    let mock = MockWorkflowCreator::new();
    let checker =
        StubIntentCollisionChecker::new(&mock.calls, vec![collision_report("make_coffee", "make a pot of coffee", 0.9)]);
    // Blank line at the save-anyway prompt (decline), then EOF at the
    // re-prompted intent step.
    let transcript = format!("{}\n", transcript_through_step_7("blank_decline_wf", INTENT));
    let (code, _output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_ne!(code, 0, "expected EOF at the re-prompted intent step");
    assert_eq!(mock.call_count(), 0, "a declined collision must never reach create_workflow");
    assert_eq!(checker.recorded_creator_calls_at_check(), vec![0], "T-10-06 ordering prohibition");
}

/// Collision found, declined by an explicit `n`: same as blank -- the
/// creator mock is never called.
#[tokio::test]
async fn collision_found_declined_by_explicit_n_writes_nothing() {
    let mock = MockWorkflowCreator::new();
    let checker =
        StubIntentCollisionChecker::new(&mock.calls, vec![collision_report("make_coffee", "make a pot of coffee", 0.9)]);
    let transcript = format!("{}n\n", transcript_through_step_7("explicit_decline_wf", INTENT));
    let (code, _output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_ne!(code, 0, "expected EOF at the re-prompted intent step");
    assert_eq!(mock.call_count(), 0, "a declined collision must never reach create_workflow");
    assert_eq!(checker.recorded_creator_calls_at_check(), vec![0], "T-10-06 ordering prohibition");
}

/// Declining returns control to Step 5 (the intent prompt), never Step 1
/// (the id prompt) and never a full restart of the flow: the id prompt
/// renders exactly once; the intent prompt renders twice (initial +
/// re-prompt).
#[tokio::test]
async fn declining_returns_to_the_intent_step_not_a_full_restart() {
    let mock = MockWorkflowCreator::new();
    let checker =
        StubIntentCollisionChecker::new(&mock.calls, vec![collision_report("make_coffee", "make a pot of coffee", 0.9)]);
    let transcript = format!("{}n\nrevised intent\n", transcript_through_step_7("declines_wf", INTENT));
    let (code, output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_ne!(
        code, 0,
        "expected EOF once the revised intent's own re-check reaches the save-anyway prompt again, output: {output}"
    );
    let id_prompt_count = output.matches("Workflow id (used as the .md filename):").count();
    assert_eq!(id_prompt_count, 1, "expected the id prompt to render exactly once (no full restart), got: {output}");
    let intent_prompt_count =
        output.matches("Intent phrase (a sentence describing when this should fire").count();
    assert_eq!(
        intent_prompt_count, 2,
        "expected the intent prompt to render once initially and once as the Step 5 re-prompt, got: {output}"
    );
    assert_eq!(mock.call_count(), 0);
}

/// Declining, then answering a revised intent whose second check reports no
/// collision: the flow reaches review and a confirmed create calls the
/// mock exactly once, carrying the REVISED intent.
#[tokio::test]
async fn revised_intent_that_clears_the_check_reaches_review_and_creates_once() {
    let mock = MockWorkflowCreator::new();
    let checker = StubIntentCollisionChecker::new(
        &mock.calls,
        vec![collision_report("make_coffee", "make a pot of coffee", 0.9), no_collision_report()],
    );
    let transcript = format!("{}n\nrevised intent\ny\n", transcript_through_step_7("revise_wf", INTENT));
    let (code, output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_eq!(code, 0, "output: {output}");
    assert_eq!(mock.call_count(), 1);
    let req = mock.last_request();
    assert_eq!(req.intent, Some("revised intent".to_string()));
    assert_eq!(checker.recorded_creator_calls_at_check(), vec![0, 0], "T-10-06 ordering prohibition");
}

/// Collision found, overridden with `y`: the flow proceeds to review and a
/// confirmed create calls the mock exactly once, carrying the ORIGINAL
/// intent (not a revised one -- no revision happened).
#[tokio::test]
async fn overriding_the_collision_reaches_review_with_the_original_intent_and_creates_once() {
    let mock = MockWorkflowCreator::new();
    let checker =
        StubIntentCollisionChecker::new(&mock.calls, vec![collision_report("make_coffee", "make a pot of coffee", 0.9)]);
    let transcript = format!("{}y\ny\n", transcript_through_step_7("override_wf", INTENT));
    let (code, output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_eq!(code, 0, "output: {output}");
    assert_eq!(mock.call_count(), 1);
    let req = mock.last_request();
    assert_eq!(
        req.intent,
        Some(INTENT.to_string()),
        "expected the ORIGINAL intent in the request after an override"
    );
    assert_eq!(checker.recorded_creator_calls_at_check(), vec![0], "T-10-06 ordering prohibition");
}

/// Warning content: the rendered warning line contains the similarity
/// score, the colliding workflow's id, and its own intent text.
#[tokio::test]
async fn the_warning_line_contains_the_score_colliding_id_and_colliding_intent() {
    let mock = MockWorkflowCreator::new();
    let checker =
        StubIntentCollisionChecker::new(&mock.calls, vec![collision_report("make_coffee", "make a pot of coffee", 0.9)]);
    let transcript = transcript_through_step_7("warncontent_wf", INTENT);
    let (code, output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_ne!(code, 0, "expected EOF at the save-anyway prompt");
    assert!(output.contains("0.9000"), "expected the similarity score in the warning, got: {output}");
    assert!(output.contains("make_coffee"), "expected the colliding workflow id in the warning, got: {output}");
    assert!(
        output.contains("make a pot of coffee"),
        "expected the colliding intent text in the warning, got: {output}"
    );
}

/// No collision: a report with all `None` fields renders no warning line
/// and no save-anyway prompt, and the flow proceeds straight to review.
#[tokio::test]
async fn no_collision_renders_no_warning_and_proceeds_straight_to_review() {
    let mock = MockWorkflowCreator::new();
    let checker = StubIntentCollisionChecker::new(&mock.calls, vec![no_collision_report()]);
    let transcript = format!("{}y\n", transcript_through_step_7("noclash_wf", INTENT));
    let (code, output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(!output.contains("[WARN]"), "expected no warning line, got: {output}");
    assert!(!output.contains("Save anyway?"), "expected no save-anyway prompt, got: {output}");
    assert_eq!(mock.call_count(), 1);
    assert_eq!(checker.recorded_creator_calls_at_check(), vec![0], "T-10-06 ordering prohibition");
}

/// Degrade path: a report whose `detail` is `Some` and whose collision
/// fields are all `None` renders the could-not-check warning, does NOT
/// render a save-anyway prompt, and proceeds to review -- a confirmed
/// create still calls the mock once.
#[tokio::test]
async fn degrade_path_warns_and_proceeds_without_a_save_anyway_prompt() {
    let mock = MockWorkflowCreator::new();
    let checker = StubIntentCollisionChecker::new(&mock.calls, vec![degrade_report("Ollama unreachable")]);
    let transcript = format!("{}y\n", transcript_through_step_7("degrade_wf", INTENT));
    let (code, output) = run_wizard_with_checker(&transcript, &mock, &checker).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(
        output.contains("[WARN] Could not check for similar workflows (Ollama unreachable)."),
        "got: {output}"
    );
    assert!(!output.contains("Save anyway?"), "expected no save-anyway prompt on the degrade path, got: {output}");
    assert_eq!(mock.call_count(), 1);
    assert_eq!(checker.recorded_creator_calls_at_check(), vec![0], "T-10-06 ordering prohibition");
}

// ---------------------------------------------------------------------
// Task 2: idempotency probe (CREATEUI-02) -- real daemon, real collision
// ---------------------------------------------------------------------

/// Idempotency (CREATEUI-02 probe): a second guided run against a
/// workflows directory that already contains `<id>.md` surfaces the
/// daemon's `already exists` message verbatim on an `[ERROR]` line and
/// re-prompts Step 1 only rather than restarting the flow or overwriting.
#[tokio::test]
async fn second_create_with_the_same_id_reprompts_only_the_id() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let client = WsOrchestratorClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    // First run creates "existing_id" successfully.
    let first_transcript = happy_path_transcript("existing_id", "the first intent");
    let (code, _output) = run_wizard(&first_transcript, &client).await;
    assert_eq!(code, 0);
    assert!(dir.path().join("existing_id.md").exists());

    // Second run: answers "existing_id" again (collision), then supplies a
    // fresh id at the re-prompt, then confirms again WITHOUT re-answering
    // any other step.
    let second_transcript = concat!(
        "existing_id\n", // Step 1, first attempt -- collides
        "\n", "\n", "1\n", "the second intent\n", "2\n", "N\n",
        "y\n",            // confirm -- fails with AlreadyExists
        "existing_id_2\n", // Step 1 re-prompt only
        "y\n",             // confirm again -- succeeds
    );
    let (code, output) = run_wizard(second_transcript, &client).await;

    assert_eq!(code, 0, "output: {output}");
    assert!(
        output.to_lowercase().contains("already exists"),
        "expected the daemon's AlreadyExists message verbatim, got: {output}"
    );
    assert!(dir.path().join("existing_id_2.md").exists());
    // The original workflow is untouched -- this was never an overwrite.
    assert!(dir.path().join("existing_id.md").exists());
}

// ---------------------------------------------------------------------
// Task 3: main.rs/cli.rs dispatch wiring (compiled-binary harness)
// ---------------------------------------------------------------------

/// Runs the built `orchestrator` binary with the given CLI args and an
/// explicitly controlled stdin, mirroring `create_integration.rs::
/// run_orchestrator` but with `stdin` overridable so a closed-stdin
/// scenario (EOF on the very first prompt) is deterministic regardless of
/// what the test harness's own stdin happens to be.
async fn run_orchestrator_with_stdin(args: &[&str], stdin: Stdio) -> Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_orchestrator"));
        cmd.args(&args);
        cmd.stdin(stdin);
        cmd.output().expect("failed to run the orchestrator binary")
    })
    .await
    .expect("run_orchestrator_with_stdin's spawn_blocking task panicked")
}

/// Guided entry: invoking `workflow create` with no positional args and a
/// closed stdin exits nonzero via the wizard's own EOF handling, not
/// clap's "missing required argument" usage error.
#[tokio::test]
async fn no_args_and_closed_stdin_exits_via_wizard_eof_not_a_clap_usage_error() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let port_str = port.to_string();

    let output = run_orchestrator_with_stdin(&["--port", &port_str, "workflow", "create"], Stdio::null()).await;

    assert!(!output.status.success(), "expected a nonzero exit for closed stdin with no args");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("required arguments were not provided") && !stderr.contains("Usage:"),
        "expected the wizard's own EOF handling, not a clap usage error, got stderr: {stderr}"
    );
}

/// Flag-driven entry unchanged: invoking `workflow create demo "hello"`
/// with the existing flags still succeeds through the daemon's real write
/// path -- the existing `create_integration.rs` suite (unmodified) is the
/// authoritative byte-identical-output check; this test proves the
/// `Option<String>` positional change did not disturb the both-`Some` path.
#[tokio::test]
async fn flag_driven_create_with_both_positionals_still_succeeds() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let port_str = port.to_string();

    let output = run_orchestrator_with_stdin(
        &["--port", &port_str, "workflow", "create", "demo", "hello"],
        Stdio::null(),
    )
    .await;

    assert!(
        output.status.success(),
        "expected the flag-driven path to still succeed, got stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("demo.md").exists());
}

/// Half-specified rejection: invoking `workflow create demo` with an id
/// but no source exits nonzero with a message naming that both
/// positionals are required together, and never silently drops into
/// guided mode with a pre-filled id.
#[tokio::test]
async fn half_specified_positionals_reject_naming_both_required_together() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let port_str = port.to_string();

    let output = run_orchestrator_with_stdin(&["--port", &port_str, "workflow", "create", "demo"], Stdio::null()).await;

    assert!(!output.status.success(), "expected a nonzero exit for a half-specified create");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        (stdout.contains("id") && stdout.contains("source")) || (stderr.contains("id") && stderr.contains("source")),
        "expected a message naming both id and source as required together, got stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !dir.path().join("demo.md").exists(),
        "a half-specified create must never write a pre-filled-id guided result"
    );
}

/// Edit unchanged: `workflow edit` still requires both positionals --
/// `Edit` was NOT changed to `Option<String>`, so clap itself rejects a
/// half-specified edit at parse time.
#[tokio::test]
async fn edit_still_requires_both_positionals() {
    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), HashMap::new()).await;
    let port_str = port.to_string();

    let output = run_orchestrator_with_stdin(&["--port", &port_str, "workflow", "edit", "demo"], Stdio::null()).await;

    assert!(!output.status.success(), "expected a nonzero exit for a half-specified edit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("required arguments were not provided") || stderr.contains("Usage:"),
        "expected clap's own usage error for edit (unchanged positionals), got stderr: {stderr}"
    );
}
