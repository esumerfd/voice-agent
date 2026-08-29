//! `agent.claude` — the AI-agent `Service` implementation (06-01, SVC-03).
//! Reads the `__agent` envelope `dispatch()` builds (`dispatch/mod.rs`),
//! composes the D-06 prompt (workflow body + a delimited JSON param block),
//! and runs it through the injected `AgentRuntime` (D-02 pluggable seam) as
//! a background task. `invoke()` returns the `RunHandle` immediately --
//! never awaiting the child inline -- so `status()` reports a genuine
//! `RunStatus::Running` window while the child is in flight (Criterion 5,
//! AI-SPEC E-07). This is the single most important discipline in this
//! module: copy-pasting `ProcessService`'s inline-`.await` shape here would
//! silently erase that window.
//!
//! 06-03 (SVC-03, G-07, restart recovery): every state transition is
//! appended-and-flushed to the durable `agent_log` JSONL store BEFORE the
//! in-memory `states` map is ever touched for that transition, so the map
//! is always a strict subset of what is durably recorded on disk. A crash
//! between the two can therefore never leave a caller having observed a
//! transition that no longer exists on disk. `AiAgentService::from_store`
//! reconstructs the map from that durable history after a restart,
//! reporting any run still `Started` as honestly `Interrupted` rather than
//! resurrecting or hanging it (`agent_log::replay_runs`'s locked recovery
//! bar).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent_log::{self, AgentRunPhase};
use crate::error::RuntimeError;
use crate::runtime::{AgentRuntime, RuntimeOutcome, RuntimeRequest};
use crate::service::{RunHandle, RunStatus, Service, ServiceError};

/// The Service Contract handler key a workflow's `service.handler:` names
/// to bind to this handler (06-01, SVC-03). No file under `src/dispatch/`,
/// `src/server/`, or `src/client/` may ever name this string literal
/// (AI-SPEC E-10, Criterion 1) -- `dispatch()` resolves handlers purely by
/// the `handlers` map key, never by branching on the handler string itself.
pub const AGENT_HANDLER_KEY: &str = "agent.claude";

/// Opens the delimited JSON parameter block inside the composed prompt
/// (D-06). Paired with `PARAM_BLOCK_CLOSE` to mark the block as runtime
/// parameter DATA, never instructions -- `serde_json::to_string` is what
/// makes the delimiters unbreakable: quotes, braces, and newlines inside a
/// parameter value arrive escaped inside a JSON string.
const PARAM_BLOCK_OPEN: &str = "<PARAM_BLOCK_OPEN>";
/// Closes the delimited JSON parameter block (D-06). See `PARAM_BLOCK_OPEN`.
const PARAM_BLOCK_CLOSE: &str = "<PARAM_BLOCK_CLOSE>";

/// Fixed `terminal_reason` marker (06-04 Task 2, G-01) recorded on the
/// durable record when a run is failed by THIS handler's own wall-clock
/// ceiling, rather than by anything the CLI envelope itself reported --
/// distinguishes "the handler gave up on it" from "the CLI reported
/// completion/failure". Defined once, here, and used by both the writer
/// (`invoke`'s spawned task, below) and every test that asserts on it, so
/// the two can never spell it differently.
pub const HANDLER_TIMEOUT_REASON: &str = "handler_timeout";

/// The CLI envelope's own `terminal_reason` value for a genuinely completed
/// run (guardrail G-05) -- anything else downgrades the run to `Failed`.
/// See `classify_outcome`.
const EXPECTED_TERMINAL_REASON: &str = "completed";
/// The CLI envelope's own `subtype` value for a genuinely successful run
/// (guardrail G-05). See `EXPECTED_TERMINAL_REASON`/`classify_outcome`.
const EXPECTED_SUBTYPE: &str = "success";

/// Caller-visible ceiling (06-04 Task 3, G-09) for the envelope's `result`
/// text: `Service::result()` never hands back more than this many bytes of
/// narrative to the invoker, truncated on a character boundary with an
/// explicit marker naming how many bytes were dropped. The durable record's
/// own `detail` field always keeps the FULL text regardless of this
/// ceiling -- this clamp protects only the WS envelope / terminal client
/// from an agent that emits a very large amount of prose.
pub const RESULT_TEXT_MAX_BYTES: usize = 64 * 1024;

/// In-memory lifecycle state for one run, keyed by `RunHandle`. Never
/// exposed outside this module.
enum RunState {
    Running,
    Completed(Value),
    Failed(String),
}

/// Real `Service` impl backing the `agent.claude` handler (06-01, SVC-03).
/// Depends on `Arc<dyn AgentRuntime>`, never on any single concrete
/// implementation of it (D-02 pluggable seam) -- this struct must never
/// name a specific runtime type.
pub struct AiAgentService {
    runtime: Arc<dyn AgentRuntime>,
    states: Arc<Mutex<HashMap<RunHandle, RunState>>>,
    next_id: Mutex<u64>,
    /// Directory `agent_log::store::append` writes durable transitions
    /// into (G-07). Never CWD-relative -- always an explicit path threaded
    /// in by the constructor.
    log_dir: PathBuf,
    retention_days: u64,
    /// Owns an ephemeral temp directory's lifetime when this service was
    /// built via `new()` (test-only convenience) -- `None` for a
    /// `from_store`-constructed service, whose caller owns `log_dir`'s
    /// lifetime.
    _log_tempdir: Option<tempfile::TempDir>,
}

impl AiAgentService {
    /// Test-only convenience (06-01, extended 06-03): builds a service
    /// backed by a fresh, empty, ephemeral log directory
    /// (`tempfile::tempdir()`, kept alive for this service's lifetime via
    /// `_log_tempdir`) with no replayed history -- durable logging still
    /// happens (G-07 is unconditional), it just has nothing to replay.
    /// Production code should use `from_store` with the daemon's real
    /// `--agent-runs-dir` once 06-05 wires that flag through.
    pub fn new(runtime: Arc<dyn AgentRuntime>) -> Self {
        let tempdir = tempfile::tempdir()
            .expect("failed to allocate a temp dir for AiAgentService::new's ephemeral agent log");
        let log_dir = tempdir.path().to_path_buf();
        Self {
            runtime,
            states: Arc::new(Mutex::new(HashMap::new())),
            next_id: Mutex::new(0),
            log_dir,
            retention_days: agent_log::AGENT_LOG_RETENTION_DAYS,
            _log_tempdir: Some(tempdir),
        }
    }

    /// Reconstructs a service from `log_dir`'s durable history (06-03,
    /// G-07, ROADMAP Criterion 4): replays every transition
    /// (`agent_log::replay_runs`), seeds the in-memory state map from the
    /// folded result (`Completed` -> `RunState::Completed`,
    /// `Failed`/`Interrupted` -> `RunState::Failed` carrying the recorded
    /// detail), and seeds the handle minter above the highest id already on
    /// disk so a restarted daemon never mints a colliding handle. Never
    /// spawns a child process during reconstruction.
    pub fn from_store(
        runtime: Arc<dyn AgentRuntime>,
        log_dir: impl Into<PathBuf>,
        retention_days: u64,
    ) -> std::io::Result<Self> {
        let log_dir = log_dir.into();
        let (replayed, next_id) = agent_log::replay_runs(&log_dir, retention_days)?;

        let mut states = HashMap::new();
        for (run_id, run) in replayed {
            let state = match run.phase {
                AgentRunPhase::Completed => RunState::Completed(run.envelope.unwrap_or(Value::Null)),
                AgentRunPhase::Failed | AgentRunPhase::Interrupted => RunState::Failed(
                    run.detail.unwrap_or_else(|| "agent run failed".to_string()),
                ),
                // `replay_runs` always rewrites a trailing `Started` to
                // `Interrupted` -- this arm is unreachable in practice, but
                // fails safe (Failed, never silently dropped) rather than
                // panicking on a future change to that invariant.
                AgentRunPhase::Started => RunState::Failed(
                    "agent run left in an unexpected Started state after replay".to_string(),
                ),
            };
            states.insert(RunHandle::new(run_id), state);
        }

        Ok(Self {
            runtime,
            states: Arc::new(Mutex::new(states)),
            next_id: Mutex::new(next_id),
            log_dir,
            retention_days,
            _log_tempdir: None,
        })
    }

    /// Mints a fresh run identity, returning both the opaque `RunHandle`
    /// (never exposes its inner string beyond this crate) and the plain
    /// `String` form `AgentRuntime::prepare` wants as a diagnostic `run_id`
    /// label -- both built from the SAME counter value so they can never
    /// drift apart.
    fn mint_handle(&self) -> (RunHandle, String) {
        let mut next_id = self
            .next_id
            .lock()
            .expect("AiAgentService next_id mutex poisoned");
        let id = *next_id;
        *next_id += 1;
        let run_id = format!("agent-{id}");
        (RunHandle::new(run_id.clone()), run_id)
    }

    /// The retention window (days) this service's durable log was
    /// constructed with -- exposed so a future daemon-startup sweep (06-05)
    /// can reuse the exact window this service already agreed to, rather
    /// than risking the two drifting apart.
    pub fn retention_days(&self) -> u64 {
        self.retention_days
    }
}

/// Subset of the CLI's raw JSON result envelope this phase's durable log
/// records verbatim (06-AI-SPEC.md Section 7): `is_error`, `subtype`,
/// `terminal_reason`, `stop_reason`, `session_id`, `num_turns`,
/// `duration_ms`, `duration_api_ms`, `total_cost_usd`, `usage`, and
/// `permission_denials`. Any other field in the raw envelope (there are
/// none known to carry secrets today, but this is a deliberate allowlist,
/// not a denylist) is dropped.
fn envelope_subset(raw: &Value) -> Value {
    const FIELDS: [&str; 11] = [
        "is_error",
        "subtype",
        "terminal_reason",
        "stop_reason",
        "session_id",
        "num_turns",
        "duration_ms",
        "duration_api_ms",
        "total_cost_usd",
        "usage",
        "permission_denials",
    ];
    let mut out = serde_json::Map::new();
    if let Value::Object(map) = raw {
        for key in FIELDS {
            if let Some(v) = map.get(key) {
                out.insert(key.to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

/// Current wall-clock time in milliseconds since the Unix epoch, matching
/// `activity/mod.rs`'s `now_ms` shape.
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Resolves the effective wall-clock ceiling for one run (06-04 Task 2,
/// G-01): the workflow frontmatter's own `timeout_secs` when present, else
/// `DEFAULT_AGENT_TIMEOUT_SECS`. Extracted as a small pure function so a
/// test can assert the resolution logic directly, without waiting on real
/// wall-clock time (a 600s default would make a naive test intolerably
/// slow).
fn resolve_timeout_secs(envelope_timeout_secs: Option<u64>) -> u64 {
    envelope_timeout_secs.unwrap_or(crate::definition::DEFAULT_AGENT_TIMEOUT_SECS)
}

/// One rule-ordered classification of a successfully-PARSED
/// `RuntimeOutcome` into this run's terminal fate (06-04 Task 3, G-05).
/// Deliberately does NOT see an unparseable/missing envelope -- that shape
/// of failure is a `RuntimeError::Envelope` from the runtime seam itself
/// (G-08), handled by the one-retry-then-fail loop in `invoke`'s spawned
/// task, one layer up from this function.
enum Classification {
    Completed,
    Failed(String),
}

/// Stop treating the agent's own self-report as proof (06-AI-SPEC.md
/// Section 1b's documented headless failure pattern: exit 0 + `is_error:
/// false` + a confident narrative, while the action the agent claims to
/// have taken was actually blocked). Every rule lives here, in order, so
/// the ordering is visible in one place and each rule is testable in
/// isolation:
///   1. `is_error:true` -> `Failed`, the envelope's own `result` text as
///      detail (verbatim).
///   2. `is_error:false` but ANY of a non-empty `permission_denials`, a
///      `terminal_reason` other than `EXPECTED_TERMINAL_REASON`, or a
///      `subtype` other than `EXPECTED_SUBTYPE` -> `Failed`, with the
///      denial list carried verbatim into the detail string.
///   3. Otherwise -> `Completed`. The happy path never fires this
///      guardrail.
fn classify_outcome(outcome: &RuntimeOutcome) -> Classification {
    if outcome.is_error {
        return Classification::Failed(outcome.result_text.clone());
    }

    let denials = outcome
        .raw_json
        .get("permission_denials")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let terminal_reason = outcome.raw_json.get("terminal_reason").and_then(|v| v.as_str());
    let subtype = outcome.raw_json.get("subtype").and_then(|v| v.as_str());

    let blocked_or_incomplete = !denials.is_empty()
        || terminal_reason != Some(EXPECTED_TERMINAL_REASON)
        || subtype != Some(EXPECTED_SUBTYPE);

    if blocked_or_incomplete {
        let denials_json = serde_json::to_string(&denials).unwrap_or_else(|_| "[]".to_string());
        return Classification::Failed(format!(
            "agent run did not complete cleanly (terminal_reason={terminal_reason:?}, subtype={subtype:?}, permission_denials={denials_json})"
        ));
    }

    Classification::Completed
}

/// Returns `raw`'s `result` field truncated to at most
/// `RESULT_TEXT_MAX_BYTES`, on a CHARACTER boundary (never a raw byte
/// index, which could split a multi-byte UTF-8 sequence), with an explicit
/// marker naming how many bytes were dropped (06-04 Task 3, G-09). A
/// `result` already within the ceiling -- the overwhelming common case --
/// is left untouched. This clamps only the copy `Service::result()` hands
/// back to the invoker; the FULL text is preserved separately for the
/// durable record (see the spawned task's own `detail` assignment).
fn clamp_result_text(mut raw: Value) -> Value {
    let Some(text) = raw.get("result").and_then(|v| v.as_str()).map(str::to_string) else {
        return raw;
    };
    if text.len() <= RESULT_TEXT_MAX_BYTES {
        return raw;
    }

    let mut boundary = RESULT_TEXT_MAX_BYTES;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let dropped = text.len() - boundary;
    let truncated = format!("{}... [truncated: {dropped} bytes dropped]", &text[..boundary]);

    if let Value::Object(map) = &mut raw {
        map.insert("result".to_string(), Value::String(truncated));
    }
    raw
}

/// Composes the D-06 prompt: the workflow body verbatim, followed by a
/// delimited block containing the caller's validated params, serialized as
/// JSON so a delimiter-breaking value arrives correctly escaped.
fn compose_prompt(body: &str, params: &Value) -> String {
    format!(
        "{body}\n\n{PARAM_BLOCK_OPEN}\n{}\n{PARAM_BLOCK_CLOSE}",
        serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string())
    )
}

#[async_trait]
impl Service for AiAgentService {
    async fn invoke(&self, input: Value) -> Result<RunHandle, ServiceError> {
        let envelope = input.get("__agent").ok_or_else(|| ServiceError {
            detail: "missing `__agent` envelope -- dispatch() must build it from service.agent"
                .to_string(),
        })?;
        let workflow_id = envelope
            .get("workflow_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let body = envelope
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let model = envelope
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let max_budget_usd = envelope.get("max_budget_usd").and_then(|v| v.as_f64());
        // G-01: resolved ONCE, here, via the small pure `resolve_timeout_secs`
        // helper -- frontmatter's own `timeout_secs` when present, else
        // `DEFAULT_AGENT_TIMEOUT_SECS`. `claude` exposes no CLI-level
        // timeout, and `dispatch()`'s poll loop is unbounded, so this
        // handler owns the ceiling end to end.
        let timeout_secs = resolve_timeout_secs(envelope.get("timeout_secs").and_then(|v| v.as_u64()));
        let params = input.get("params").cloned().unwrap_or(Value::Null);

        // 06-02 staging (D-03/E-03/G-03): `workflow_dir` is the SOLE
        // dispatch()-constructed source of the staging boundary's anchor
        // directory (never caller-derived). A lenient "." fallback covers
        // tests that call `invoke()` directly with no `workflow_dir` key
        // and no declared `files` -- production traffic always goes
        // through `dispatch()`, which always sets this field.
        let workflow_dir = envelope
            .get("workflow_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let files: Vec<PathBuf> = envelope
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).map(PathBuf::from).collect())
            .unwrap_or_default();

        let (handle, run_id) = self.mint_handle();

        let prompt = compose_prompt(&body, &params);
        let prompt_digest = agent_log::store::prompt_digest(prompt.as_bytes());
        let prompt_bytes = prompt.len();
        // 06-AI-SPEC.md Section 7's `staged_files[]`: the declared entries'
        // paths relative to the workflow directory -- the same relative
        // shape `LocalRuntime::prepare` stages them under inside the
        // scratch directory. Computed once here since `files`/`workflow_dir`
        // are already in hand, reused for both the Started and terminal
        // records so they can never drift apart.
        let staged_files: Vec<String> = files
            .iter()
            .map(|f| f.strip_prefix(&workflow_dir).unwrap_or(f).to_string_lossy().into_owned())
            .collect();

        // G-07 log-before-memory ordering: append-and-flush the durable
        // Started transition BEFORE the in-memory state map is ever touched
        // for this run. This is what buys the invariant that the map is
        // always a strict subset of durable truth -- a crash between the
        // two can never leave a caller having observed a run that no
        // longer exists on disk. An append failure must not lose the run:
        // log a warning and continue to the in-memory update, so a
        // full-disk condition degrades to "durability lost for this
        // transition" rather than "the run silently hangs".
        let started_record = agent_log::AgentRunRecord {
            run_id: run_id.clone(),
            workflow_id: workflow_id.clone(),
            phase: AgentRunPhase::Started,
            at_ms: now_ms(),
            claude_version: None,
            model: model.clone(),
            max_budget_usd,
            scratch_dir: None,
            staged_files: staged_files.clone(),
            prompt_digest: prompt_digest.clone(),
            prompt_bytes,
            params: Some(params.clone()),
            envelope: None,
            detail: None,
        };
        if let Err(err) = agent_log::store::append(&self.log_dir, &started_record) {
            eprintln!("agent log: failed to persist Started transition for {run_id}: {err}");
        }

        self.states
            .lock()
            .expect("AiAgentService states mutex poisoned")
            .insert(handle.clone(), RunState::Running);

        let runtime = self.runtime.clone();
        let states = self.states.clone();
        let handle_clone = handle.clone();
        let log_dir = self.log_dir.clone();

        // Spawned as a background task, NOT awaited inline -- this is the
        // single most important line in this handler (06-AI-SPEC.md 4b's
        // "one common mistake"). Awaiting the child directly here would
        // silently erase the observable Running window Criterion 5
        // requires; `invoke()` must return the handle before the child
        // exits.
        tokio::spawn(async move {
            let prepared = runtime.prepare(&run_id, &workflow_dir, &files).await;
            let mut scratch_dir: Option<String> = None;
            let mut envelope_value: Option<Value> = None;
            // Every code path below now sets this before the match block
            // ends (06-04 Task 3: the durable record always keeps the full
            // narrative text, so the previously-Completed-stays-None arm
            // no longer exists) -- the initial value only silences
            // definite-assignment analysis across the nested match.
            #[allow(unused_assignments)]
            let mut detail: Option<String> = None;
            let mut claude_version: Option<String> = None;

            let next_state = match prepared {
                Ok(prepared) => {
                    scratch_dir = Some(prepared.working_dir.to_string_lossy().into_owned());
                    let request = RuntimeRequest {
                        prompt,
                        model: model.clone(),
                        max_budget_usd,
                        timeout: Duration::from_secs(timeout_secs),
                    };

                    let mut outcome = runtime.run(&prepared, &request).await;

                    // G-08: envelope missing/unparseable, or missing the
                    // error flag, surfaces from the runtime seam as
                    // `RuntimeError::Envelope` -- retry ONCE with the
                    // IDENTICAL prompt (never a silently rewritten one).
                    //
                    // CR-02 fix (06-REVIEW.md): this diagnostic is
                    // deliberately routed through `eprintln!` ONLY, never
                    // the authoritative durable JSONL. Appending a
                    // `phase: Failed` record here (the pre-fix behavior)
                    // broke `agent_log::replay_runs`'s locked
                    // restart-recovery bar, which only rewrites a run to
                    // `Interrupted` when its LAST replayed phase is still
                    // `Started`: a daemon crash between that diagnostic
                    // write and the retry's real terminal write would leave
                    // exactly `Started` -> `Failed` (diagnostic) on disk,
                    // permanently misreporting a genuinely interrupted run
                    // as a deterministic envelope-parsing failure. Keeping
                    // this non-authoritative means the durable phase
                    // sequence for this run_id stays exactly
                    // `Started` -> `<real terminal>`, so the existing
                    // recovery rule keeps working unmodified. The
                    // in-memory `states` map is deliberately NOT touched
                    // here either -- the run stays `Running` in memory
                    // until a genuinely terminal outcome exists.
                    if let Err(RuntimeError::Envelope { detail: parse_detail }) = &outcome {
                        eprintln!(
                            "agent log: envelope parse failure on first attempt for {run_id}, retrying once: {parse_detail}"
                        );
                        outcome = runtime.run(&prepared, &request).await;
                    }

                    let state = match outcome {
                        Ok(o) => {
                            envelope_value = Some(envelope_subset(&o.raw_json));
                            claude_version = o.claude_version.clone();
                            // The durable record always keeps the FULL
                            // narrative text (G-09) -- captured here BEFORE
                            // any caller-visible clamping happens below.
                            detail = Some(o.result_text.clone());
                            match classify_outcome(&o) {
                                Classification::Completed => {
                                    RunState::Completed(clamp_result_text(o.raw_json))
                                }
                                Classification::Failed(reason) => RunState::Failed(reason),
                            }
                        }
                        Err(RuntimeError::Timeout { seconds }) => {
                            // G-01: the handler's own ceiling fired, not
                            // anything the CLI envelope reported -- no
                            // envelope exists for a killed child, so a
                            // synthetic one carries the fixed
                            // `HANDLER_TIMEOUT_REASON` marker so the
                            // offline timeout-rate metric (F-04) can count
                            // it directly from the durable log.
                            let msg = format!(
                                "agent run exceeded its {seconds}s ceiling and was terminated"
                            );
                            envelope_value = Some(json!({"terminal_reason": HANDLER_TIMEOUT_REASON}));
                            detail = Some(msg.clone());
                            RunState::Failed(msg)
                        }
                        Err(e) => {
                            detail = Some(e.to_string());
                            RunState::Failed(e.to_string())
                        }
                    };
                    // Best-effort cleanup -- never lets a cleanup failure
                    // mask the run's own terminal state. `failed` reflects
                    // the FINAL classification (denials/timeout/parse
                    // failure all count), not just the raw `is_error` flag
                    // -- a denial-downgraded run must retain its scratch
                    // directory exactly like an `is_error:true` one.
                    let failed = matches!(state, RunState::Failed(_));
                    let _ = runtime.cleanup(&prepared, failed).await;
                    state
                }
                Err(e) => {
                    detail = Some(e.to_string());
                    RunState::Failed(e.to_string())
                }
            };

            let phase = match &next_state {
                RunState::Completed(_) => AgentRunPhase::Completed,
                RunState::Failed(_) => AgentRunPhase::Failed,
                RunState::Running => unreachable!("a spawned run's terminal state is never Running"),
            };
            let terminal_record = agent_log::AgentRunRecord {
                run_id: run_id.clone(),
                workflow_id,
                phase,
                at_ms: now_ms(),
                claude_version,
                model,
                max_budget_usd,
                scratch_dir,
                staged_files,
                prompt_digest,
                prompt_bytes,
                params: Some(params),
                envelope: envelope_value,
                detail,
            };
            // G-07 again: append-and-flush the terminal transition BEFORE
            // the in-memory state map observes it.
            if let Err(err) = agent_log::store::append(&log_dir, &terminal_record) {
                eprintln!("agent log: failed to persist terminal transition for {run_id}: {err}");
            }

            states
                .lock()
                .expect("AiAgentService states mutex poisoned")
                .insert(handle_clone, next_state);
        });

        Ok(handle)
    }

    async fn status(&self, handle: &RunHandle) -> Result<RunStatus, ServiceError> {
        let states = self
            .states
            .lock()
            .expect("AiAgentService states mutex poisoned");
        match states.get(handle) {
            Some(RunState::Running) => Ok(RunStatus::Running),
            Some(RunState::Completed(_)) => Ok(RunStatus::Completed),
            Some(RunState::Failed(_)) => Ok(RunStatus::Failed),
            None => Err(ServiceError {
                detail: "unknown agent run handle".to_string(),
            }),
        }
    }

    async fn cancel(&self, _handle: &RunHandle) -> Result<(), ServiceError> {
        // No-op, matching every other handler in this codebase.
        Ok(())
    }

    async fn result(&self, handle: &RunHandle) -> Result<Option<Value>, ServiceError> {
        let states = self
            .states
            .lock()
            .expect("AiAgentService states mutex poisoned");
        match states.get(handle) {
            Some(RunState::Completed(value)) => Ok(Some(value.clone())),
            Some(RunState::Failed(detail)) => Err(ServiceError {
                detail: detail.clone(),
            }),
            Some(RunState::Running) => Err(ServiceError {
                detail: "agent run still in progress".to_string(),
            }),
            None => Err(ServiceError {
                detail: "unknown agent run handle".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use serde_json::json;

    use tempfile::TempDir;

    use crate::error::RuntimeError;
    use crate::runtime::{AgentRuntime, PreparedRun, RuntimeOutcome, RuntimeRequest};
    use crate::service::{RunStatus, Service};

    use super::{
        classify_outcome, resolve_timeout_secs, AiAgentService, Arc, Classification,
        HANDLER_TIMEOUT_REASON, RESULT_TEXT_MAX_BYTES,
    };

    /// In-memory `AgentRuntime` test double (D-02 pluggable seam). Proves
    /// `AiAgentService`'s OWN state-machine logic (the Running window,
    /// terminal-state mapping) independent of the real subprocess-spawning
    /// runtime's concerns -- those are covered separately by
    /// `runtime/local.rs`'s own unit tests. This is `AiAgentService`'s ONLY
    /// concrete `AgentRuntime` dependency anywhere in this module: it never
    /// names any single concrete implementation (D-02 -- the Service impl
    /// depends on `Arc<dyn AgentRuntime>` only), and needs no subprocess,
    /// no env-var side channel, and no spawn-serialization guard -- timing
    /// is controlled directly via an injected `tokio::time::sleep` delay.
    struct MockAgentRuntime {
        delay: Duration,
        outcome: MockOutcome,
        /// Counts every `run()` invocation (06-04 Task 3, G-08) -- lets a
        /// retry test assert the fixture/mock was invoked EXACTLY twice,
        /// never a third time.
        call_count: std::sync::atomic::AtomicUsize,
    }

    enum MockOutcome {
        Success(serde_json::Value),
        AgentError(String),
        SpawnFailure,
        /// 06-04 Task 2 (G-01): every call returns `RuntimeError::Timeout`.
        TimeoutFailure(u64),
        /// 06-04 Task 3 (G-08): the FIRST call returns
        /// `RuntimeError::Envelope`; every call after that returns the
        /// wrapped clean envelope -- proves the one-retry path succeeds
        /// when the retry itself is clean.
        EnvelopeFailureThenSuccess(serde_json::Value),
        /// 06-04 Task 3 (G-08): every call returns `RuntimeError::Envelope`
        /// -- proves the one-retry CAP: exactly two calls, then `Failed`,
        /// never a third.
        AlwaysEnvelopeFailure,
    }

    impl MockAgentRuntime {
        fn new(delay: Duration, outcome: MockOutcome) -> Self {
            Self { delay, outcome, call_count: std::sync::atomic::AtomicUsize::new(0) }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl AgentRuntime for MockAgentRuntime {
        async fn prepare(
            &self,
            run_id: &str,
            _workflow_dir: &std::path::Path,
            _files: &[PathBuf],
        ) -> Result<PreparedRun, RuntimeError> {
            Ok(PreparedRun {
                run_id: run_id.to_string(),
                working_dir: PathBuf::from("/mock-scratch-dir"),
            })
        }

        async fn run(
            &self,
            _prepared: &PreparedRun,
            _request: &RuntimeRequest,
        ) -> Result<RuntimeOutcome, RuntimeError> {
            let attempt = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            tokio::time::sleep(self.delay).await;
            match &self.outcome {
                MockOutcome::Success(raw_json) => Ok(RuntimeOutcome {
                    is_error: false,
                    result_text: raw_json
                        .get("result")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    raw_json: raw_json.clone(),
                    claude_version: Some("mock-2.0.0".to_string()),
                }),
                MockOutcome::AgentError(detail) => Ok(RuntimeOutcome {
                    is_error: true,
                    result_text: detail.clone(),
                    raw_json: json!({"is_error": true, "result": detail}),
                    claude_version: Some("mock-2.0.0".to_string()),
                }),
                MockOutcome::SpawnFailure => Err(RuntimeError::Spawn {
                    binary: PathBuf::from("/mock/claude"),
                    detail: "mock spawn failure".to_string(),
                }),
                MockOutcome::TimeoutFailure(seconds) => Err(RuntimeError::Timeout { seconds: *seconds }),
                MockOutcome::EnvelopeFailureThenSuccess(raw_json) => {
                    if attempt == 1 {
                        Err(RuntimeError::Envelope {
                            detail: "mock: malformed envelope on first attempt".to_string(),
                        })
                    } else {
                        Ok(RuntimeOutcome {
                            is_error: false,
                            result_text: raw_json
                                .get("result")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            raw_json: raw_json.clone(),
                            claude_version: Some("mock-2.0.0".to_string()),
                        })
                    }
                }
                MockOutcome::AlwaysEnvelopeFailure => Err(RuntimeError::Envelope {
                    detail: "mock: always malformed envelope".to_string(),
                }),
            }
        }

        async fn cleanup(&self, _prepared: &PreparedRun, _failed: bool) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    /// A fully-clean canned envelope (06-04 Task 3, G-05): the shape every
    /// `MockOutcome::Success` test that wants a clean `Completed` needs --
    /// `terminal_reason`/`subtype`/`permission_denials` are ALL guardrail
    /// inputs now, not just `is_error`, so a bare `json!({"result": ...})`
    /// no longer reaches `Completed` (`classify_outcome` correctly reads it
    /// as an envelope with no denials but also no confirmed completion).
    fn clean_success_envelope(result: &str) -> serde_json::Value {
        json!({
            "is_error": false,
            "result": result,
            "terminal_reason": "completed",
            "subtype": "success",
            "permission_denials": [],
        })
    }

    /// Polls `status()` until it leaves `Running` or `deadline` passes,
    /// mirroring `dispatch()`'s own poll-until-terminal shape.
    async fn poll_until_terminal(service: &AiAgentService, handle: &crate::service::RunHandle) -> RunStatus {
        let mut status = service.status(handle).await.expect("status should succeed");
        let deadline = Instant::now() + Duration::from_secs(2);
        while status == RunStatus::Running && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
            status = service.status(handle).await.expect("status should succeed");
        }
        status
    }

    #[tokio::test]
    async fn invoke_returns_before_the_runtime_delay_completes_and_status_is_running() {
        let service = AiAgentService::new(Arc::new(MockAgentRuntime::new(
            Duration::from_millis(150),
            MockOutcome::Success(clean_success_envelope("PONG")),
        )));

        let started = Instant::now();
        let handle = service
            .invoke(json!({"__agent": {"body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");
        let invoke_elapsed = started.elapsed();

        assert!(
            invoke_elapsed < Duration::from_millis(100),
            "expected invoke() to return well under the runtime's 150ms injected delay, took {invoke_elapsed:?}"
        );
        assert_eq!(
            service.status(&handle).await,
            Ok(RunStatus::Running),
            "expected status() to be Running immediately after invoke(), before the runtime's run() resolves"
        );

        // Let the background task finish so it doesn't outlive the test.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn after_the_runtime_resolves_status_is_completed_and_result_returns_parsed_text() {
        let service = AiAgentService::new(Arc::new(MockAgentRuntime::new(
            Duration::ZERO,
            MockOutcome::Success(clean_success_envelope("PONG")),
        )));

        let handle = service
            .invoke(json!({"__agent": {"body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");

        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Completed, "expected Completed once the runtime resolves");

        let output = service
            .result(&handle)
            .await
            .expect("result should succeed for a completed run")
            .expect("result should produce Some output");
        assert_eq!(
            output.get("result").and_then(|v| v.as_str()),
            Some("PONG"),
            "expected result() to return the runtime's parsed envelope, got: {output:?}"
        );
    }

    #[tokio::test]
    async fn is_error_true_outcome_maps_to_failed_with_detail() {
        let service = AiAgentService::new(Arc::new(MockAgentRuntime::new(
            Duration::ZERO,
            MockOutcome::AgentError("boom: bad thing happened".to_string()),
        )));

        let handle = service
            .invoke(json!({"__agent": {"body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed even though the run will fail");

        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Failed, "expected Failed for an is_error:true outcome");

        let error = service
            .result(&handle)
            .await
            .expect_err("expected result() to return Err for a failed run");
        assert!(
            error.detail.contains("boom: bad thing happened"),
            "expected the error detail to carry the envelope's result text, got: {error:?}"
        );
    }

    #[tokio::test]
    async fn runtime_spawn_failure_maps_to_failed_never_stuck_running() {
        let service = AiAgentService::new(Arc::new(MockAgentRuntime::new(
            Duration::ZERO,
            MockOutcome::SpawnFailure,
        )));

        let handle = service
            .invoke(json!({"__agent": {"body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed even when the runtime cannot spawn");

        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(
            status,
            RunStatus::Failed,
            "expected a runtime spawn failure to surface Failed, never stuck Running"
        );
    }

    #[tokio::test]
    async fn unknown_handle_is_rejected_by_status_and_result() {
        let service = AiAgentService::new(Arc::new(MockAgentRuntime::new(
            Duration::ZERO,
            MockOutcome::Success(clean_success_envelope("PONG")),
        )));
        let unknown = crate::service::RunHandle::new("agent-does-not-exist");

        assert!(service.status(&unknown).await.is_err());
        assert!(service.result(&unknown).await.is_err());
    }

    // -----------------------------------------------------------------
    // 06-04 Task 2: `resolve_timeout_secs` (pure, no wall-clock)
    // -----------------------------------------------------------------

    #[test]
    fn resolve_timeout_secs_defaults_when_the_envelope_omits_it() {
        assert_eq!(resolve_timeout_secs(None), crate::definition::DEFAULT_AGENT_TIMEOUT_SECS);
    }

    #[test]
    fn resolve_timeout_secs_uses_the_frontmatter_value_when_present() {
        assert_eq!(resolve_timeout_secs(Some(42)), 42);
    }

    // -----------------------------------------------------------------
    // 06-04 Task 2: runtime timeout -> Failed, never Running again
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn runtime_timeout_maps_to_failed_naming_the_ceiling_and_status_never_returns_to_running() {
        let service = AiAgentService::new(Arc::new(MockAgentRuntime::new(
            Duration::ZERO,
            MockOutcome::TimeoutFailure(5),
        )));

        let handle = service
            .invoke(json!({"__agent": {"body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed even when the runtime times out");

        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Failed, "expected a runtime timeout to surface Failed");

        // Never returns to Running on any subsequent poll.
        for _ in 0..3 {
            assert_eq!(service.status(&handle).await, Ok(RunStatus::Failed));
        }

        let error = service.result(&handle).await.expect_err("expected Err for a timed-out run");
        assert!(
            error.detail.contains("5s"),
            "expected the error detail to name the ceiling, got: {error:?}"
        );
    }

    #[tokio::test]
    async fn runtime_timeout_records_the_handler_timeout_reason_on_the_durable_record() {
        let log_dir = TempDir::new().expect("tempdir");
        let service = AiAgentService::from_store(
            Arc::new(MockAgentRuntime::new(Duration::ZERO, MockOutcome::TimeoutFailure(5))),
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("from_store on a fresh directory should succeed");

        let handle = service
            .invoke(json!({"__agent": {"workflow_id": "stall_wf", "body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");
        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Failed);

        let records = crate::agent_log::store::replay(
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("replay should succeed");
        let terminal = records
            .iter()
            .rev()
            .find(|r| r.run_id == "agent-0" && r.phase == crate::agent_log::AgentRunPhase::Failed)
            .expect("expected a Failed terminal record on disk");
        let logged_reason = terminal
            .envelope
            .as_ref()
            .and_then(|e| e.get("terminal_reason"))
            .and_then(|v| v.as_str());
        assert_eq!(
            logged_reason,
            Some(HANDLER_TIMEOUT_REASON),
            "expected the durable record's envelope to carry the handler-timeout marker, got: {terminal:?}"
        );
    }

    // -----------------------------------------------------------------
    // 06-04 Task 3: `classify_outcome` (pure, no runtime/subprocess)
    // -----------------------------------------------------------------

    fn outcome_from(raw_json: serde_json::Value, is_error: bool) -> RuntimeOutcome {
        RuntimeOutcome {
            is_error,
            result_text: raw_json.get("result").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            raw_json,
            claude_version: None,
        }
    }

    #[test]
    fn classify_outcome_downgrades_a_clean_is_error_false_run_with_permission_denials_to_failed() {
        let outcome = outcome_from(
            json!({
                "is_error": false,
                "result": "Done! I created the report",
                "permission_denials": [{"tool": "Bash", "reason": "blocked"}],
                "terminal_reason": "completed",
                "subtype": "success"
            }),
            false,
        );

        match classify_outcome(&outcome) {
            Classification::Failed(detail) => {
                assert!(
                    detail.contains("Bash"),
                    "expected the denial entries to appear verbatim in the failure detail, got: {detail}"
                );
            }
            Classification::Completed => {
                panic!("expected a non-empty permission_denials to downgrade to Failed, got Completed")
            }
        }
    }

    #[test]
    fn classify_outcome_fails_a_non_completed_terminal_reason_even_with_is_error_false() {
        let outcome = outcome_from(
            json!({"is_error": false, "terminal_reason": "max_turns", "subtype": "success", "permission_denials": []}),
            false,
        );
        assert!(matches!(classify_outcome(&outcome), Classification::Failed(_)));
    }

    #[test]
    fn classify_outcome_fails_a_non_success_subtype_even_with_is_error_false() {
        let outcome = outcome_from(
            json!({"is_error": false, "terminal_reason": "completed", "subtype": "error_max_turns", "permission_denials": []}),
            false,
        );
        assert!(matches!(classify_outcome(&outcome), Classification::Failed(_)));
    }

    #[test]
    fn classify_outcome_reports_completed_for_a_fully_clean_envelope() {
        let outcome = outcome_from(
            json!({"is_error": false, "terminal_reason": "completed", "subtype": "success", "permission_denials": [], "result": "PONG"}),
            false,
        );
        assert!(matches!(classify_outcome(&outcome), Classification::Completed));
    }

    #[test]
    fn classify_outcome_fails_an_is_error_true_run_carrying_the_result_text_as_detail() {
        let outcome = outcome_from(json!({"is_error": true, "result": "boom: bad thing happened"}), true);
        match classify_outcome(&outcome) {
            Classification::Failed(detail) => assert_eq!(detail, "boom: bad thing happened"),
            Classification::Completed => panic!("expected is_error:true to fail"),
        }
    }

    // -----------------------------------------------------------------
    // 06-04 Task 3: denial envelope end-to-end (invoke -> result -> log)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn denial_envelope_downgrades_to_failed_and_denials_appear_in_result_and_durable_log() {
        let log_dir = TempDir::new().expect("tempdir");
        let denial_envelope = json!({
            "is_error": false,
            "result": "Done! I created the report",
            "permission_denials": [{"tool": "Bash", "reason": "blocked", "command": "rm -rf /"}],
            "terminal_reason": "completed",
            "subtype": "success"
        });
        let service = AiAgentService::from_store(
            Arc::new(MockAgentRuntime::new(Duration::ZERO, MockOutcome::Success(denial_envelope.clone()))),
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("from_store on a fresh directory should succeed");

        let handle = service
            .invoke(json!({"__agent": {"workflow_id": "denial_wf", "body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");
        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Failed, "a non-empty permission_denials must never surface as a clean Completed");

        let error = service.result(&handle).await.expect_err("expected Err for a denial-flagged run");
        assert!(
            error.detail.contains("Bash"),
            "expected the denial entries verbatim in the caller-visible payload, got: {:?}",
            error.detail
        );

        let records = crate::agent_log::store::replay(
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("replay should succeed");
        let terminal = records
            .iter()
            .rev()
            .find(|r| r.run_id == "agent-0" && r.phase == crate::agent_log::AgentRunPhase::Failed)
            .expect("expected a Failed terminal record on disk");
        let logged_denials = terminal.envelope.as_ref().and_then(|e| e.get("permission_denials")).cloned();
        assert_eq!(
            logged_denials,
            denial_envelope.get("permission_denials").cloned(),
            "expected the durable record's envelope to carry the denial list verbatim"
        );
    }

    // -----------------------------------------------------------------
    // 06-04 Task 3: G-08 retry -- exactly one retry, never a third call
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn envelope_parse_failure_retries_once_and_succeeds_on_the_second_attempt() {
        let mock = Arc::new(MockAgentRuntime::new(
            Duration::ZERO,
            MockOutcome::EnvelopeFailureThenSuccess(clean_success_envelope("PONG")),
        ));
        let service = AiAgentService::new(mock.clone() as Arc<dyn AgentRuntime>);

        let handle = service
            .invoke(json!({"__agent": {"body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");
        let status = poll_until_terminal(&service, &handle).await;

        assert_eq!(
            status,
            RunStatus::Completed,
            "expected a retry that succeeds on the second attempt to reach Completed"
        );
        assert_eq!(mock.call_count(), 2, "expected exactly two runtime.run() invocations (one retry)");
    }

    #[tokio::test]
    async fn envelope_parse_failure_on_both_attempts_fails_after_exactly_one_retry() {
        let mock = Arc::new(MockAgentRuntime::new(Duration::ZERO, MockOutcome::AlwaysEnvelopeFailure));
        let service = AiAgentService::new(mock.clone() as Arc<dyn AgentRuntime>);

        let handle = service
            .invoke(json!({"__agent": {"body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed even though the runtime keeps failing to parse");
        let status = poll_until_terminal(&service, &handle).await;

        assert_eq!(
            status,
            RunStatus::Failed,
            "expected a run that never produces a parseable envelope to fail, never stay Running"
        );
        assert_eq!(mock.call_count(), 2, "expected exactly one retry (two total calls), never a third");
    }

    // -----------------------------------------------------------------
    // CR-02 (06-REVIEW.md): the envelope-retry diagnostic must never break
    // the restart-recovery invariant (`agent_log::replay_runs`'s locked bar:
    // any run whose LAST replayed phase is still `Started` is rewritten to
    // `Interrupted`). Writing an intermediate `phase: Failed` diagnostic
    // record before the retry defeats that bar if the daemon crashes
    // between the diagnostic write and the retry's real terminal write.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn envelope_parse_failure_never_writes_an_intermediate_failed_diagnostic_record_to_the_durable_log() {
        let log_dir = TempDir::new().expect("tempdir");
        let mock = Arc::new(MockAgentRuntime::new(
            Duration::ZERO,
            MockOutcome::EnvelopeFailureThenSuccess(clean_success_envelope("PONG")),
        ));
        let service = AiAgentService::from_store(
            mock.clone() as Arc<dyn AgentRuntime>,
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("from_store on a fresh directory should succeed");

        let handle = service
            .invoke(json!({"__agent": {"workflow_id": "retry_wf", "body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");
        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Completed, "expected the retry that succeeds on attempt 2 to reach Completed");

        let records = crate::agent_log::store::replay(
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("replay should succeed");
        let run_phases: Vec<crate::agent_log::AgentRunPhase> =
            records.iter().filter(|r| r.run_id == "agent-0").map(|r| r.phase).collect();

        // CR-02 fix: the durable phase sequence for this run_id must stay
        // EXACTLY Started -> Completed -- never an intermediate Failed
        // diagnostic record sandwiched in between, which would defeat
        // `agent_log::replay_runs`'s locked restart-recovery bar if the
        // daemon crashed in the window between that diagnostic write and
        // the retry's real terminal write.
        assert_eq!(
            run_phases,
            vec![crate::agent_log::AgentRunPhase::Started, crate::agent_log::AgentRunPhase::Completed],
            "expected exactly Started then Completed on disk, no intermediate Failed diagnostic record, got: {run_phases:?}"
        );
    }

    #[test]
    fn a_crash_between_the_envelope_retry_diagnostic_and_the_retrys_terminal_write_still_replays_as_interrupted_not_failed(
    ) {
        // Simulates a daemon crash occurring after the Started record was
        // written but before ANY terminal record exists on disk -- exactly
        // the crash window CR-02 (06-REVIEW.md) identified. With the
        // diagnostic-record fix (an envelope-parse-failure retry never
        // writes an intermediate phase to the durable log), a crash in
        // this window leaves only the Started record on disk, so the
        // locked restart-recovery bar rewrites it to Interrupted -- never
        // the misleading deterministic Failed the old diagnostic-write
        // behavior would have produced.
        let log_dir = tempfile::tempdir().expect("tempdir");
        let started = crate::agent_log::AgentRunRecord {
            run_id: "agent-0".to_string(),
            workflow_id: "retry_wf".to_string(),
            phase: crate::agent_log::AgentRunPhase::Started,
            at_ms: 1_000,
            claude_version: None,
            model: None,
            max_budget_usd: None,
            scratch_dir: None,
            staged_files: vec![],
            prompt_digest: "digest".to_string(),
            prompt_bytes: 10,
            params: None,
            envelope: None,
            detail: None,
        };
        crate::agent_log::store::append(log_dir.path(), &started).expect("append should succeed");

        let (replayed, _next_id) =
            crate::agent_log::replay_runs(log_dir.path(), crate::agent_log::AGENT_LOG_RETENTION_DAYS)
                .expect("replay_runs should succeed");
        let run = replayed.get("agent-0").expect("expected agent-0 in the replayed map");
        assert_eq!(
            run.phase,
            crate::agent_log::AgentRunPhase::Interrupted,
            "expected a crash leaving only Started on disk to replay as Interrupted, never Failed"
        );
    }

    // -----------------------------------------------------------------
    // 06-04 Task 3: G-09 output-size clamp
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn oversized_result_text_is_truncated_for_the_caller_but_kept_full_in_the_durable_record() {
        let log_dir = TempDir::new().expect("tempdir");
        let long_text = "x".repeat(RESULT_TEXT_MAX_BYTES + 500);
        let envelope = json!({
            "is_error": false,
            "result": long_text,
            "terminal_reason": "completed",
            "subtype": "success",
            "permission_denials": []
        });
        let service = AiAgentService::from_store(
            Arc::new(MockAgentRuntime::new(Duration::ZERO, MockOutcome::Success(envelope))),
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("from_store on a fresh directory should succeed");

        let handle = service
            .invoke(json!({"__agent": {"workflow_id": "big_wf", "body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");
        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Completed);

        let output = service
            .result(&handle)
            .await
            .expect("result should succeed for a completed run")
            .expect("result should produce Some output");
        let caller_visible_result =
            output.get("result").and_then(|v| v.as_str()).expect("expected a result string");
        assert!(
            caller_visible_result.len() < long_text.len(),
            "expected the caller-visible result to be shorter than the full text"
        );
        assert!(
            caller_visible_result.contains("truncated"),
            "expected an explicit truncation marker in the caller-visible text"
        );

        let records = crate::agent_log::store::replay(
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("replay should succeed");
        let terminal = records
            .iter()
            .rev()
            .find(|r| r.run_id == "agent-0" && r.phase == crate::agent_log::AgentRunPhase::Completed)
            .expect("expected a Completed terminal record on disk");
        let stored_detail = terminal.detail.as_ref().expect("expected detail to carry the full result text");
        assert!(
            stored_detail.len() > caller_visible_result.len(),
            "expected the durable record to retain the FULL text, longer than the caller-visible truncated copy"
        );
        assert_eq!(stored_detail, &long_text, "expected the durable record's detail to equal the full untruncated text");
    }

    // -----------------------------------------------------------------
    // 06-04 Task 3: cost/usage fields + claude_version round-trip
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn cost_and_usage_fields_round_trip_into_the_durable_record() {
        let log_dir = TempDir::new().expect("tempdir");
        let envelope = json!({
            "is_error": false, "result": "PONG", "terminal_reason": "completed", "subtype": "success",
            "permission_denials": [], "total_cost_usd": 0.0234, "duration_ms": 4200, "duration_api_ms": 3900,
            "num_turns": 3, "usage": {"input_tokens": 120, "output_tokens": 45}
        });
        let service = AiAgentService::from_store(
            Arc::new(MockAgentRuntime::new(Duration::ZERO, MockOutcome::Success(envelope.clone()))),
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("from_store on a fresh directory should succeed");

        let handle = service
            .invoke(json!({"__agent": {"workflow_id": "cost_wf", "body": "hi"}, "params": {}}))
            .await
            .expect("invoke should succeed");
        let status = poll_until_terminal(&service, &handle).await;
        assert_eq!(status, RunStatus::Completed);

        let records = crate::agent_log::store::replay(
            log_dir.path(),
            crate::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
        .expect("replay should succeed");
        let terminal = records
            .iter()
            .rev()
            .find(|r| r.run_id == "agent-0" && r.phase == crate::agent_log::AgentRunPhase::Completed)
            .expect("expected a Completed terminal record on disk");
        let logged_envelope = terminal.envelope.as_ref().expect("expected an envelope subset on the record");
        assert_eq!(logged_envelope.get("total_cost_usd"), envelope.get("total_cost_usd"));
        assert_eq!(logged_envelope.get("duration_ms"), envelope.get("duration_ms"));
        assert_eq!(logged_envelope.get("duration_api_ms"), envelope.get("duration_api_ms"));
        assert_eq!(logged_envelope.get("num_turns"), envelope.get("num_turns"));
        assert_eq!(logged_envelope.get("usage"), envelope.get("usage"));
        assert!(
            terminal.claude_version.as_deref().is_some_and(|v| !v.is_empty()),
            "expected a non-empty claude_version on the durable record when the runtime reports one, got: {:?}",
            terminal.claude_version
        );
    }
}
