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
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use shared::{
    AgentConfig, CreateWorkflowRequest, CreateWorkflowResponse, ParameterType, ProtocolFrame, WorkflowCreator,
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

    let transcript = happy_path_transcript("brew_coffee", INTENT);
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

async fn run_wizard(transcript: &str, client: &dyn WorkflowCreator) -> (i32, String) {
    let mut input = Cursor::new(transcript.as_bytes().to_vec());
    let mut out = Vec::new();
    let code = create_wizard::run(client, &mut input, &mut out)
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
