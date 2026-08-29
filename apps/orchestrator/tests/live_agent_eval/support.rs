//! Shared plumbing for the live evaluation harness (06-06 Task 2). Lives in
//! its own file, a sibling of `live_agent_eval.rs`, for one specific
//! reason: the plan's own acceptance check greps `live_agent_eval.rs`
//! ALONE for a matching `#[ignore]`-attribute count and `fn`-declaration
//! count ("every test is attribute-gated"). A shared helper `fn` living
//! directly in `live_agent_eval.rs` would inflate that second count without
//! ever being a gated test itself. Putting every non-test helper here
//! keeps that structural guarantee true by construction, never by
//! discipline alone.
//!
//! NOTHING in this file is itself gated, and nothing here spends money on
//! its own -- gating (the `#[ignore]` attribute AND the opt-in environment
//! check) happens exactly once per test, in `live_agent_eval.rs`, strictly
//! BEFORE any function here is ever called.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tempfile::TempDir;

use orchestrator::agent_log;
use orchestrator::handlers::ai_agent::{AiAgentService, AGENT_HANDLER_KEY};
use orchestrator::runtime::local::LocalRuntime;
use orchestrator::{dispatch, InvokeOutcome, Registry, Service};

/// The opt-in environment variable gating this entire file (06-AI-SPEC.md
/// Section 5's CI/CD Integration block). Its literal spelling lives here
/// once so every test in `live_agent_eval.rs` checks the identical name.
pub const LIVE_EVAL_OPT_IN_VAR: &str = "WK_AGENT_LIVE_EVAL";

/// `true` only when the opt-in variable holds exactly `"1"` -- anything
/// else (unset, empty, `"true"`, `"yes"`) is treated as NOT opted in. A
/// single accepted value, spelled once, is what makes the double gate
/// actually double: no test in this file trusts "the variable merely
/// exists" as consent.
pub fn opted_in() -> bool {
    std::env::var(LIVE_EVAL_OPT_IN_VAR).map(|v| v == "1").unwrap_or(false)
}

/// Resolves the real `claude` binary EXACTLY the way the daemon does --
/// `orchestrator::startup::preflight_agent`, reading the same
/// `ORCHESTRATOR_CLAUDE_BIN` environment variable `orchestratord`'s own
/// `--claude-bin` flag reads (falling back to the bare name `claude`,
/// resolved against `PATH` by `preflight_agent` itself) -- and validates
/// `ANTHROPIC_API_KEY` is present and non-empty in the same call. Never a
/// hard-coded path anywhere in this file.
pub fn resolve_real_binary_and_key() -> Result<(PathBuf, String), String> {
    let configured = std::env::var("ORCHESTRATOR_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let resolved = orchestrator::startup::preflight_agent(Path::new(&configured), api_key.as_deref())?;
    let key = api_key.expect("preflight_agent only returns Ok when api_key was Some");
    Ok((resolved, key))
}

/// Absolute path to `tests/fixtures/agent_eval/cases/` -- the SAME
/// workflow fixtures `agent_eval_tier_a.rs` (Task 1) authored; this file
/// reuses them rather than building a second set of case workflows.
pub fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agent_eval/cases")
}

/// Absolute path to `tests/fixtures/fake_claude.sh`.
pub fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_claude.sh")
}

/// One live case's execution result: the `dispatch()`-level outcome plus
/// the scratch directory and `claude_version` recovered from the durable
/// `agent_log` terminal record. `_scratch_root`/`_log_dir` exist purely to
/// keep their `TempDir`s alive for the caller's own post-run inspection.
pub struct LiveCaseRun {
    pub outcome: InvokeOutcome,
    pub scratch_dir: Option<PathBuf>,
    pub claude_version: Option<String>,
    pub _scratch_root: TempDir,
    pub _log_dir: TempDir,
}

/// Runs `case_id` end to end through the REAL handler stack
/// (`Registry::load` -> `dispatch()` -> `AiAgentService` -> `LocalRuntime`),
/// against the REAL `claude_bin`/`api_key` a caller already resolved via
/// `resolve_real_binary_and_key`. Mirrors `agent_eval_tier_a.rs`'s own
/// `run_case` helper exactly, with the fixture double swapped for the real
/// binary -- the SAME code path, a different binary.
pub async fn run_live_case(case_id: &str, params: Value, claude_bin: PathBuf, api_key: &str) -> LiveCaseRun {
    let (registry, errors) = Registry::load(cases_dir());
    assert!(errors.is_empty(), "unexpected load errors in tests/fixtures/agent_eval/cases/: {errors:?}");
    let definition = registry
        .lookup(case_id)
        .unwrap_or_else(|| panic!("case {case_id}: registry.lookup failed -- missing/unparseable workflow fixture"))
        .clone();

    let scratch_root = TempDir::new().expect("failed to create scratch tempdir");
    let log_dir = TempDir::new().expect("failed to create log tempdir");
    let runtime = Arc::new(LocalRuntime::new(claude_bin, api_key, scratch_root.path()).await);
    let service = AiAgentService::from_store(runtime, log_dir.path(), agent_log::AGENT_LOG_RETENTION_DAYS)
        .unwrap_or_else(|e| panic!("case {case_id}: AiAgentService::from_store on a fresh dir failed: {e}"));
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(AGENT_HANDLER_KEY.to_string(), Box::new(service));

    let payload = match params {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };

    let outcome = dispatch(&definition, payload, &handlers)
        .await
        .unwrap_or_else(|e| panic!("case {case_id}: dispatch() itself returned an error: {e:?}"));

    let records = agent_log::store::replay(log_dir.path(), agent_log::AGENT_LOG_RETENTION_DAYS)
        .unwrap_or_else(|e| panic!("case {case_id}: replaying the durable agent log failed: {e}"));
    let terminal = records
        .into_iter()
        .rev()
        .find(|r| r.run_id == "agent-0" && r.phase != agent_log::AgentRunPhase::Started);
    let scratch_dir = terminal.as_ref().and_then(|r| r.scratch_dir.clone()).map(PathBuf::from);
    let claude_version = terminal.as_ref().and_then(|r| r.claude_version.clone());

    LiveCaseRun {
        outcome,
        scratch_dir,
        claude_version,
        _scratch_root: scratch_root,
        _log_dir: log_dir,
    }
}

/// Lists the immediate (non-recursive) entries directly under `dir`,
/// sorted, as a cheap before/after snapshot for the adversarial live
/// cases' blast-radius check ("no file created outside the scratch
/// directory"). Deliberately shallow: a full recursive tree diff would
/// also churn on `target/`'s own build output, which has nothing to do
/// with an agent's tool calls.
pub fn snapshot_dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    entries.sort();
    entries
}

/// Classifies a `serde_json::Value`'s JSON type as a short label, for the
/// fidelity check's real-vs-fixture type-drift comparison.
pub fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Prints the human review checklist a live pass's closing summary owes the
/// author (06-06 Task 2's `<action>`): where to look, and what judgment
/// only a human can make. Never asserts anything itself.
pub fn print_review_checklist(case_id: &str, run: &LiveCaseRun) {
    println!("--- live eval review checklist: {case_id} ---");
    println!("  status: {:?}", run.outcome.status);
    println!("  claude_version: {:?}", run.claude_version);
    match &run.scratch_dir {
        Some(dir) => println!("  scratch dir (open and compare against the result narrative): {}", dir.display()),
        None => println!("  scratch dir: none recorded"),
    }
    match &run.outcome.output {
        Some(output) => {
            let cost = output.get("total_cost_usd").cloned().unwrap_or(Value::Null);
            println!("  result narrative: {:?}", output.get("result"));
            println!("  observed cost (compare against the ~$0.05-$0.50/run planning envelope): {cost:?}");
        }
        None => println!("  error: {:?}", run.outcome.error),
    }
}
