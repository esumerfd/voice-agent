//! `AgentRuntime` — the D-02 pluggable seam behind the AI-agent handler
//! (06-01, SVC-03). `AiAgentService` (`handlers/ai_agent.rs`) depends on
//! `Arc<dyn AgentRuntime>`, never on a concrete runtime type: container and
//! remote-VM runtimes are named future implementations of this same trait
//! (D-04's deferred tmux warm-session runtime is one), and nothing in
//! `AiAgentService` may assume local-filesystem semantics beyond what this
//! contract exposes. `LocalRuntime` (`runtime::local`) is the only
//! implementation this phase ships (D-05).

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::RuntimeError;

pub mod local;

/// The per-run scratch directory a runtime prepared, returned by
/// `AgentRuntime::prepare` and threaded into `AgentRuntime::run`/`cleanup`.
/// `run_id` is an opaque diagnostic label (not the `RunHandle` itself --
/// the runtime never depends on `service::RunHandle`, keeping the seam
/// independent of the Service Contract's own identity type).
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedRun {
    pub run_id: String,
    pub working_dir: PathBuf,
}

/// A single agent invocation request: the fully-composed prompt (workflow
/// body + D-06 delimited JSON param block) and the frontmatter-sourced,
/// trusted-only knobs (`model`, `max_budget_usd`) plus the wall-clock
/// ceiling (`timeout`, enforced starting 06-04).
///
/// `model` and `max_budget_usd` share `service.command`'s trust tier
/// (06-04, E-04/G-04): both are frontmatter-sourced, workflow-author-
/// trusted, and can NEVER be populated from a caller-supplied parameter.
/// A caller's own `params` reach the child through exactly ONE place --
/// the composed `prompt` string above, `serde_json`-serialized inside
/// D-06's delimited block -- and no other argv slot. `LocalRuntime::run`
/// enforces this by construction: it builds argv in one visible place,
/// and every element besides `prompt` is either a fixed safety/cost flag
/// or one of these two frontmatter-only fields.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub timeout: Duration,
}

/// The parsed outcome of a completed `AgentRuntime::run` call: whether the
/// envelope reported an error, the free-text `result` field, and the full
/// raw JSON envelope for callers that need more than `result_text` (e.g. a
/// future `--json-schema` typed-result consumer, or the durable log).
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeOutcome {
    pub is_error: bool,
    pub result_text: String,
    pub raw_json: Value,
    /// The configured binary's `--version` output, captured ONCE at
    /// `LocalRuntime::new` construction time (never per invocation) and
    /// cached (06-04 Task 3) -- the drift tripwire (06-AI-SPEC.md
    /// F-05/Section 7) that lets a future behaviour change in the real CLI
    /// be traced to a specific release rather than blamed on this handler.
    /// `None` when the version lookup itself failed (binary missing,
    /// non-executable, non-UTF8 output) -- a version-lookup failure is
    /// never fatal to the run itself.
    pub claude_version: Option<String>,
}

/// The pluggable agent-execution seam (D-02). `prepare`/`run`/`cleanup`
/// mirror the lifecycle every implementation must honor: stage an isolated
/// working directory, execute one agentic invocation inside it, then apply
/// a retention/cleanup policy. `Send + Sync` so a runtime can be shared as
/// `Arc<dyn AgentRuntime>` across concurrent invocations.
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Prepare an isolated working directory for one run, staging the
    /// declared file dependencies (D-03/06-02). `run_id` is a diagnostic
    /// label only. `workflow_dir` is the directory containing the workflow
    /// `.md` file that declared `files` (`WorkflowDefinition::source_path`'s
    /// parent) -- every declared entry's SOURCE side must resolve (after
    /// canonicalize) inside this directory, and every staged file's
    /// DESTINATION side must resolve inside the prepared per-run scratch
    /// directory (E-03/G-03: both ends of the boundary are checked).
    async fn prepare(
        &self,
        run_id: &str,
        workflow_dir: &Path,
        files: &[PathBuf],
    ) -> Result<PreparedRun, RuntimeError>;

    /// Execute one agent invocation inside `prepared.working_dir`.
    async fn run(
        &self,
        prepared: &PreparedRun,
        request: &RuntimeRequest,
    ) -> Result<RuntimeOutcome, RuntimeError>;

    /// Apply the runtime's retention/cleanup policy for `prepared`.
    /// `failed` lets 06-02 implement a "retain on failure" policy without a
    /// signature change -- this plan's `LocalRuntime` leaves it a
    /// documented no-op.
    async fn cleanup(&self, prepared: &PreparedRun, failed: bool) -> Result<(), RuntimeError>;
}
