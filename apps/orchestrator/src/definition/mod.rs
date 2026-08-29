//! Workflow definition type surface (WF-01, WF-03).
//!
//! Phase 1 parses/validates only the M1 fields (D-05): id, name, parameters,
//! service. `description` is Markdown body prose, not a frontmatter field.
//! The voice `triggers`/`intent` block is deliberately absent (D-05).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// `ParameterType`/`WorkflowSummary` moved to `shared` (they cross the
/// `OrchestratorClient` seam as part of `ParameterDescriptor`/
/// `ListWorkflowsResponse`) -- re-exported here so every existing
/// `crate::definition::ParameterType`/`crate::definition::WorkflowSummary`
/// reference (dispatch, error, registry/mod.rs, registry/loader.rs) keeps
/// resolving with zero edits at those call sites.
pub use shared::{ParameterType, WorkflowSummary};

/// Full definition returned by `Registry::lookup()`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowDefinition {
    pub id: String,
    pub name: String,
    /// Markdown body prose (D-05) — not a frontmatter field.
    pub description: String,
    pub parameters: HashMap<String, ParameterSpec>,
    pub service: ServiceSpec,
    /// The `.md` file this definition was loaded from (populated by the
    /// registry loader from the scanned path; used for the per-run log
    /// line, quick task 260719-foq).
    pub source_path: PathBuf,
}

/// A single parameter's schema (WF-03): name (map key) + type + required.
#[derive(Debug, Clone, PartialEq)]
pub struct ParameterSpec {
    pub type_: ParameterType,
    pub description: Option<String>,
    pub required: bool,
}

/// Closed enum (D-10) — a per-workflow sync/async declaration consumed by
/// Phase 4's transport to decide whether `dispatch()` is awaited inline or
/// spawned. Absent from frontmatter resolves to `Sync` (Pitfall 4 — must
/// not break the 3 workflow files committed before this field existed).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServiceMode {
    #[default]
    Sync,
    Async,
}

/// The shared default `service.handler` key: an absent or empty/whitespace-
/// only `handler:` defaults here (loader.rs's `validate_definition`) rather
/// than raising a `MissingField` error, mirroring `service.mode`'s absent-
/// defaults-to-Sync pattern (D-10) -- except a free-form handler key has no
/// "bad value", only "unspecified", so there is no reject arm to mirror.
/// `orchestratord`'s handler registration (`bin/orchestratord.rs`) keys its
/// `ImmediateService` entry off this SAME constant, so the defaulted key and
/// the registered key can never drift apart.
pub const DEFAULT_HANDLER: &str = "action.immediate";

/// The Service Contract handler binding (D-08). `handler` is required;
/// `type_` is not validated in Phase 1. `mode` defaults to `Sync` (D-10).
/// An absent/empty `handler:` defaults to `DEFAULT_HANDLER` rather than
/// erroring (this field is never itself absent after loading).
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceSpec {
    pub type_: Option<String>,
    pub handler: String,
    pub mode: ServiceMode,
    /// Optional server-side script binding (quick task 260720-s7i):
    /// trusted-frontmatter-only, set exclusively by `loader.rs` from the
    /// workflow author's local `.md` file, never from a caller's invoke
    /// payload. `None` when the workflow declares no `command:` key. Mirrors
    /// `ProcessService`'s command-as-security-surface discipline -- the
    /// dispatch-side envelope (`dispatch/mod.rs`) reads this field as the
    /// SOLE source of the `__command` transport key. No load-time
    /// existence/executability check is performed here (mirrors
    /// `ProcessService`'s fail-at-spawn-time precedent).
    pub command: Option<PathBuf>,
    /// Optional args accompanying `command`, in declared order. Empty
    /// (never absent) when `command` is `None` or when `args:` is omitted.
    /// Same trust boundary as `command`: sourced only from frontmatter.
    pub args: Vec<String>,
    /// Optional AI-agent binding (06-01, SVC-03, D-01/D-02/D-03).
    /// Trusted-frontmatter-only, set exclusively by `loader.rs` from the
    /// workflow author's local `.md` file, never from a caller's invoke
    /// payload -- same trust tier as `command` above. `None` when the
    /// workflow declares no `service.agent:` block. `loader.rs` rejects a
    /// workflow declaring BOTH `command` and `agent` (a workflow binds to
    /// exactly one handler shape) via `LoadError::ConflictingServiceBinding`.
    /// The dispatch-side envelope (`dispatch/mod.rs`) reads this field as
    /// the SOLE source of the `__agent` transport key.
    pub agent: Option<AgentSpec>,
}

/// Recommended default wall-clock ceiling (seconds) for a `claude -p`
/// invocation when a workflow's `service.agent.timeout_secs` is absent
/// (06-AI-SPEC.md guardrail G-01) -- no CLI-level timeout or `--max-turns`
/// flag exists, and `dispatch()` polls unboundedly, so the handler itself
/// must own the ceiling. Consumed starting 06-04 (this plan only threads
/// the field through, it does not yet enforce it).
pub const DEFAULT_AGENT_TIMEOUT_SECS: u64 = 600;

/// Recommended default hard dollar cap (guardrail G-06) for a single
/// `claude -p` invocation when a workflow's `service.agent.max_budget_usd`
/// is absent -- roughly 2x the high end of 06-AI-SPEC.md Section 4b.5's
/// $0.05-$0.50 planning envelope. Consumed starting 06-04.
pub const DEFAULT_AGENT_MAX_BUDGET_USD: f64 = 1.00;

/// The AI-agent service binding (06-01, SVC-03). A **nested** `service.agent:`
/// block was chosen over 06-RESEARCH.md Pattern 4's flat `service.files:`
/// shape (D-03 is Claude's Discretion): `dispatch()` needs a data-driven
/// `Some`/`None` discriminator to decide whether to build the agent envelope
/// -- exactly mirroring how it already matches on `service.command` -- and a
/// flat list gives no such discriminator for a minimal agent workflow that
/// declares no files. This also keeps the future tmux runtime (D-04,
/// deferred) addable as `service.agent.runtime:` with no shape change.
///
/// Every field here is set exclusively by `loader.rs` from the workflow
/// author's own local `.md` file, and never from a caller's invoke payload
/// (same trust tier as `ServiceSpec::command`/`args`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentSpec {
    /// Declared file dependencies (D-03), anchored to the workflow `.md`'s
    /// own directory by `loader.rs` -- every entry here is an absolute path
    /// (an absolute frontmatter entry is stored verbatim; a relative one is
    /// joined onto the workflow file's parent directory). No filesystem
    /// call is made at load time (fail-at-use-time precedent, mirroring
    /// `command`).
    pub files: Vec<PathBuf>,
    /// Optional model override (e.g. `claude-sonnet-5`), passed through as
    /// `--model <value>` only when present. Never derived from caller
    /// `--param` input.
    pub model: Option<String>,
    /// Optional hard dollar cap for a single invocation (guardrail G-06).
    /// Absent resolves to `DEFAULT_AGENT_MAX_BUDGET_USD` at runtime.
    pub max_budget_usd: Option<f64>,
    /// Optional wall-clock ceiling in seconds (guardrail G-01). Absent
    /// resolves to `DEFAULT_AGENT_TIMEOUT_SECS` at runtime.
    pub timeout_secs: Option<u64>,
}
