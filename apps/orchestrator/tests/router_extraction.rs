//! Every extraction failure mode (plan 09-04, Task 2, ROUT-03): a matched
//! workflow with zero declared parameters, an Ollama-layer failure, a
//! non-object result, a type-check failure, and a missing required
//! parameter -- each degrades to a reported match with `extracted_params:
//! None` and a named `detail`, never an `Err` out of `Router::route` and
//! never a panic (ROUT-04 boundary: a failed extraction never invalidates a
//! successful match).
//!
//! Drives `Router::route` directly over a `TempDir` registry of fixture
//! workflows and a configurable mock `OllamaApi` -- no `#[ignore]`, no live
//! Ollama, no environment variable required for any test in this file.
//!
//! Plan 09-04 Task 3 adds ONE additional, double-gated LIVE test at the
//! bottom of this file (the extraction-model bake-off) -- mirroring
//! `tests/live_agent_eval.rs`'s/`tests/router_calibration.rs`'s identical
//! `#[ignore]` + opt-in-env-var convention. That one test is the only thing
//! in this file that ever touches a real Ollama server.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use tempfile::TempDir;

use orchestrator::definition::{ParameterSpec, ParameterType, ServiceMode, ServiceSpec, WorkflowDefinition};
use orchestrator::router::ollama_client::{HttpOllamaClient, OllamaApi};
use orchestrator::router::Router;
use orchestrator::{Registry, RouterError};

fn write_workflow(dir: &Path, filename: &str, contents: &str) {
    let path = dir.join(filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create fixture directories");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

const REQUIRED_INT_WORKFLOW: &str = r#"---
id: req_int_wf
name: Required Int Workflow
parameters:
  count:
    type: int
    required: true
service:
  type: action
  handler: demo.handler
intent: "increment the counter by a specific amount"
---
Fixture workflow with one required int parameter.
"#;

const REQUIRED_STRING_WORKFLOW: &str = r#"---
id: req_string_wf
name: Required String Workflow
parameters:
  name:
    type: string
    required: true
service:
  type: action
  handler: demo.handler
intent: "greet someone by their name"
---
Fixture workflow with one required string parameter.
"#;

const NO_PARAM_WORKFLOW: &str = r#"---
id: no_param_wf
name: No Param Workflow
parameters: {}
service:
  type: action
  handler: demo.handler
intent: "do a thing that needs no parameters at all"
---
Fixture workflow with zero declared parameters.
"#;

fn registry() -> Registry {
    let dir = TempDir::new().expect("failed to create fixture tempdir");
    write_workflow(dir.path(), "req_int_wf.md", REQUIRED_INT_WORKFLOW);
    write_workflow(dir.path(), "req_string_wf.md", REQUIRED_STRING_WORKFLOW);
    write_workflow(dir.path(), "no_param_wf.md", NO_PARAM_WORKFLOW);
    let (registry, errors) = Registry::load(dir.path());
    assert!(errors.is_empty(), "expected zero load errors from the fixture directory, got: {errors:?}");
    registry
}

/// Deterministic, orthogonal per-intent vectors -- utterances reuse the
/// exact same vector as the workflow they must match, giving cosine
/// similarity 1.0 against that workflow and 0.0 against the others, well
/// clear of `MATCH_THRESHOLD` regardless of its currently-calibrated value.
fn vectors() -> HashMap<String, Vec<f32>> {
    let mut v = HashMap::new();
    v.insert("increment the counter by a specific amount".to_string(), vec![1.0, 0.0, 0.0]);
    v.insert("greet someone by their name".to_string(), vec![0.0, 1.0, 0.0]);
    v.insert("do a thing that needs no parameters at all".to_string(), vec![0.0, 0.0, 1.0]);
    v.insert("please increment the counter".to_string(), vec![1.0, 0.0, 0.0]);
    v.insert("please greet dave".to_string(), vec![0.0, 1.0, 0.0]);
    v.insert("please do the no-param thing".to_string(), vec![0.0, 0.0, 1.0]);
    v
}

/// What a `generate_json` call returns for every test in this file -- either
/// a canned (already-double-parsed) JSON value, exactly what a real
/// `HttpOllamaClient` would hand back on success, or a canned `RouterError`,
/// exactly what a real client would hand back on any Ollama-layer failure
/// (unreachable, timeout, model-not-found, or the mandatory double-parse
/// itself failing).
enum GenerateBehavior {
    Response(Value),
    Error(RouterError),
}

/// Configurable `OllamaApi` test double for this file's failure-mode
/// coverage -- deterministic embeddings plus one configured
/// `generate_json` outcome, with its own call counter so a test can assert
/// the zero-parameter short-circuit never touches the client at all.
struct MockOllama {
    vectors: HashMap<String, Vec<f32>>,
    generate_calls: AtomicUsize,
    generate_behavior: GenerateBehavior,
}

impl MockOllama {
    fn new(vectors: HashMap<String, Vec<f32>>, generate_behavior: GenerateBehavior) -> Self {
        Self { vectors, generate_calls: AtomicUsize::new(0), generate_behavior }
    }

    fn generate_call_count(&self) -> usize {
        self.generate_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl OllamaApi for MockOllama {
    async fn embed(&self, _model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError> {
        Ok(inputs.iter().map(|i| self.vectors.get(i).cloned().unwrap_or_default()).collect())
    }

    async fn generate_json(
        &self,
        _model: &str,
        _system: &str,
        _prompt: &str,
        _schema: &Value,
    ) -> Result<Value, RouterError> {
        self.generate_calls.fetch_add(1, Ordering::SeqCst);
        match &self.generate_behavior {
            GenerateBehavior::Response(v) => Ok(v.clone()),
            GenerateBehavior::Error(e) => Err(e.clone()),
        }
    }
}

#[tokio::test]
async fn a_zero_parameter_workflow_reports_an_empty_object_and_costs_zero_generate_calls() {
    let mock = Arc::new(MockOllama::new(vectors(), GenerateBehavior::Response(json!({}))));
    let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
    let registry = registry();

    let outcome =
        router.route("please do the no-param thing", &registry).await.expect("route should succeed");

    assert_eq!(outcome.matched_workflow_id, Some("no_param_wf".to_string()));
    assert_eq!(outcome.extracted_params, Some(json!({})));
    assert_eq!(mock.generate_call_count(), 0, "a zero-parameter workflow must never call generate_json");
}

#[tokio::test]
async fn an_unparseable_response_yields_no_parameters_and_a_detail_naming_the_workflow_and_the_parse_failure(
) {
    let mock = Arc::new(MockOllama::new(
        vectors(),
        GenerateBehavior::Error(RouterError::MalformedResponse {
            endpoint: "mock://ollama/api/generate".to_string(),
            detail: "`response` field was not valid JSON: expected value at line 1 column 1".to_string(),
        }),
    ));
    let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
    let registry = registry();

    let outcome =
        router.route("please increment the counter", &registry).await.expect("route should succeed");

    assert_eq!(
        outcome.matched_workflow_id,
        Some("req_int_wf".to_string()),
        "a parse failure must not un-match a real match"
    );
    assert_eq!(outcome.extracted_params, None);
    let detail = outcome.detail.expect("expected a detail string on a degraded extraction");
    assert!(detail.contains("req_int_wf"), "expected the detail to name the workflow, got: {detail}");
    assert!(
        detail.to_lowercase().contains("json"),
        "expected the detail to reference the parse failure, got: {detail}"
    );
}

#[tokio::test]
async fn an_array_response_value_yields_no_parameters_and_a_named_detail() {
    let mock = Arc::new(MockOllama::new(vectors(), GenerateBehavior::Response(json!([1, 2, 3]))));
    let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
    let registry = registry();

    let outcome =
        router.route("please increment the counter", &registry).await.expect("route should succeed");

    assert_eq!(outcome.matched_workflow_id, Some("req_int_wf".to_string()));
    assert_eq!(outcome.extracted_params, None);
    assert!(
        outcome.detail.as_deref().is_some_and(|d| d.contains("req_int_wf")),
        "expected the detail to name the workflow, got: {:?}",
        outcome.detail
    );
}

#[tokio::test]
async fn a_scalar_response_value_yields_no_parameters_and_a_named_detail() {
    let mock = Arc::new(MockOllama::new(vectors(), GenerateBehavior::Response(json!(5))));
    let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
    let registry = registry();

    let outcome =
        router.route("please increment the counter", &registry).await.expect("route should succeed");

    assert_eq!(outcome.matched_workflow_id, Some("req_int_wf".to_string()));
    assert_eq!(outcome.extracted_params, None);
    assert!(outcome.detail.is_some());
}

#[tokio::test]
async fn a_wrong_type_extracted_value_yields_no_parameters_and_names_the_offending_parameter() {
    let mock =
        Arc::new(MockOllama::new(vectors(), GenerateBehavior::Response(json!({"count": "not a number"}))));
    let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
    let registry = registry();

    let outcome =
        router.route("please increment the counter", &registry).await.expect("route should succeed");

    assert_eq!(outcome.matched_workflow_id, Some("req_int_wf".to_string()));
    assert_eq!(
        outcome.extracted_params, None,
        "a type-check failure must drop the WHOLE map, never report the subset that happened to type-check"
    );
    assert!(
        outcome.detail.as_deref().is_some_and(|d| d.contains("count")),
        "expected the detail to name the offending parameter, got: {:?}",
        outcome.detail
    );
}

#[tokio::test]
async fn a_missing_required_parameter_yields_no_parameters_and_names_it() {
    let mock = Arc::new(MockOllama::new(vectors(), GenerateBehavior::Response(json!({}))));
    let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
    let registry = registry();

    let outcome = router.route("please greet dave", &registry).await.expect("route should succeed");

    assert_eq!(outcome.matched_workflow_id, Some("req_string_wf".to_string()));
    assert_eq!(outcome.extracted_params, None);
    assert!(
        outcome.detail.as_deref().is_some_and(|d| d.contains("name")),
        "expected the detail to name the missing required parameter, got: {:?}",
        outcome.detail
    );
}

#[tokio::test]
async fn an_ollama_failure_during_extraction_degrades_to_a_reported_match_never_an_err() {
    let mock = Arc::new(MockOllama::new(
        vectors(),
        GenerateBehavior::Error(RouterError::OllamaUnreachable {
            base_url: "mock://ollama".to_string(),
            detail: "connection refused".to_string(),
        }),
    ));
    let router = Router::new(mock.clone(), "nomic-embed-text", "llama3.2:3b");
    let registry = registry();

    let result = router.route("please increment the counter", &registry).await;

    let outcome =
        result.expect("an extraction-layer Ollama failure must never surface as Err out of route()");
    assert_eq!(outcome.matched_workflow_id, Some("req_int_wf".to_string()));
    assert_eq!(outcome.extracted_params, None);
    assert!(outcome.detail.is_some());
}

#[tokio::test]
async fn an_extraction_failure_never_changes_matched_workflow_id_score_or_confirm_tier() {
    let mock_success = Arc::new(MockOllama::new(vectors(), GenerateBehavior::Response(json!({"count": 5}))));
    let router_success = Router::new(mock_success, "nomic-embed-text", "llama3.2:3b");
    let success_outcome = router_success
        .route("please increment the counter", &registry())
        .await
        .expect("route should succeed");

    let mock_failure = Arc::new(MockOllama::new(
        vectors(),
        GenerateBehavior::Error(RouterError::Timeout {
            endpoint: "mock://ollama/api/generate".to_string(),
            seconds: 10,
        }),
    ));
    let router_failure = Router::new(mock_failure, "nomic-embed-text", "llama3.2:3b");
    let failure_outcome = router_failure
        .route("please increment the counter", &registry())
        .await
        .expect("route should succeed");

    assert_eq!(success_outcome.matched_workflow_id, failure_outcome.matched_workflow_id);
    assert_eq!(success_outcome.similarity_score, failure_outcome.similarity_score);
    assert_eq!(success_outcome.confirm_tier, failure_outcome.confirm_tier);
    assert_ne!(
        success_outcome.extracted_params, failure_outcome.extracted_params,
        "the SUCCESS run must carry parameters and the FAILURE run must not, or the equality \
         assertions above would be vacuous"
    );
}

// -----------------------------------------------------------------
// Plan 09-04, Task 3: opt-in extraction-model bake-off (live Ollama)
// -----------------------------------------------------------------
//
// Double-gated exactly like `live_agent_eval.rs`/`router_calibration.rs`:
// the `#[ignore]` attribute AND this opt-in variable are BOTH required --
// the attribute alone would still run under `--include-ignored`; the
// variable alone would still run if someone deleted the attribute. Never
// wired into automated continuous integration.

/// The opt-in environment variable gating the one live test below.
const EXTRACTION_LIVE_OPT_IN_VAR: &str = "WK_ROUTER_EXTRACTION_LIVE";

/// Matches `orchestratord`'s own `--ollama-url` default -- never a value
/// invented independently here.
const LIVE_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

/// Matches `orchestratord`'s own `--embed-model` default. Unused by
/// extraction itself, but `Router::new` requires one.
const LIVE_EMBED_MODEL: &str = "nomic-embed-text";

/// Matches `orchestratord`'s own `--extract-model` default (plan 09-04) --
/// the ONE model this bake-off's real assertion is scoped to. Every other
/// candidate below is a printed measurement only.
const DEFAULT_EXTRACT_MODEL: &str = "llama3.2:3b";

/// The full candidate list from 09-RESEARCH.md's Open Question 1. This
/// bake-off measures whichever of these happen to already be installed
/// (`ollama list`) and NEVER pulls one -- an absent candidate is a printed
/// skip row, never a failure.
const CANDIDATE_MODELS: [&str; 3] = ["llama3.2:3b", "phi4-mini", "qwen2.5:3b-instruct"];

/// A `set_timer`-shaped fixture definition with one required int parameter
/// -- built directly, with no registry or embedding call involved. The
/// bake-off calls `Router::extract_params` directly (not `route()`),
/// isolating the measurement to the ONE thing that varies across candidate
/// models: structured generation.
fn bakeoff_definition() -> WorkflowDefinition {
    let mut parameters = HashMap::new();
    parameters.insert(
        "duration_minutes".to_string(),
        ParameterSpec {
            type_: ParameterType::Int,
            description: Some("how many minutes the timer should run for".to_string()),
            required: true,
        },
    );
    WorkflowDefinition {
        id: "set_timer".to_string(),
        name: "Set Timer".to_string(),
        description: String::new(),
        parameters,
        service: ServiceSpec {
            type_: Some("action".to_string()),
            handler: "timers.start".to_string(),
            mode: ServiceMode::Sync,
            command: None,
            args: Vec::new(),
            agent: None,
        },
        source_path: std::path::PathBuf::from("bakeoff.md"),
        intent: "set a timer for a given number of minutes and notify me when the time is up"
            .to_string(),
        triggers: Vec::new(),
    }
}

#[ignore]
#[tokio::test]
async fn extraction_model_bake_off_measures_every_installed_candidate_against_the_default() {
    if std::env::var(EXTRACTION_LIVE_OPT_IN_VAR).as_deref() != Ok("1") {
        println!(
            "SKIP: set {EXTRACTION_LIVE_OPT_IN_VAR}=1 to run the extraction-model bake-off \
             against a live local Ollama server. See 09-04-PLAN.md."
        );
        return;
    }

    let ollama_list_output = std::process::Command::new("ollama")
        .arg("list")
        .output()
        .expect("failed to run `ollama list` -- is Ollama installed and on PATH?");
    let installed = String::from_utf8_lossy(&ollama_list_output.stdout);

    let def = bakeoff_definition();
    let utterance = "set a timer for twenty five minutes";
    let expected = json!({"duration_minutes": 25});

    println!(
        "\n{:<24} {:>14} {:>8}  {}",
        "model", "duration_ms", "matched", "extracted (detail on failure)"
    );
    let mut default_result: Option<Value> = None;
    let mut default_was_installed = false;

    for candidate in CANDIDATE_MODELS {
        if !installed.contains(candidate) {
            println!("{candidate:<24} {:>14} {:>8}  (not installed -- skipped, never pulled)", "-", "-");
            continue;
        }

        let client: Arc<dyn OllamaApi> = Arc::new(HttpOllamaClient::new(LIVE_OLLAMA_BASE_URL));
        let router = Router::new(client, LIVE_EMBED_MODEL, candidate);

        let start = std::time::Instant::now();
        let (extracted, detail) = router.extract_params(&def, utterance).await;
        let elapsed_ms = start.elapsed().as_millis();

        let matched = extracted.as_ref() == Some(&expected);
        println!(
            "{candidate:<24} {elapsed_ms:>14} {:>8}  {:?}{}",
            matched,
            extracted,
            detail.map(|d| format!(" (detail: {d})")).unwrap_or_default()
        );

        if candidate == DEFAULT_EXTRACT_MODEL {
            default_result = extracted;
            default_was_installed = true;
        }
    }

    assert!(
        default_was_installed,
        "the configured default extraction model ({DEFAULT_EXTRACT_MODEL}) is not in `ollama \
         list` -- this bake-off cannot measure its own default"
    );
    assert_eq!(
        default_result,
        Some(expected),
        "expected the CONFIGURED DEFAULT model ({DEFAULT_EXTRACT_MODEL}) to extract the exact \
         expected value -- this is the bake-off's one real gate; every other candidate above is \
         a printed measurement only, never an assertion"
    );
}
