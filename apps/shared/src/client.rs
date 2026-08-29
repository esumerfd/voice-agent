//! Wire-protocol client types (D-04/D-07): the `OrchestratorClient` trait and
//! its request/response DTOs, plus the two registry-facing wire types
//! (`ParameterType`, `WorkflowSummary`) that cross the seam embedded in
//! those DTOs. `InProcessOrchestrator`, its `impl OrchestratorClient`, and
//! `impl From<RunStatus> for InvokeStatus` stay in the `orchestrator` crate
//! (orphan rule / registry internals) -- only the serializable contract
//! lives here.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListWorkflowsRequest {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListWorkflowsResponse {
    pub workflows: Vec<WorkflowSummary>,
    /// `LoadError`s collected during the scan (D-07 skip-and-warn), carried
    /// through rather than dropped so 02-02 can surface them to stderr
    /// (D-10, Pitfall 5).
    pub warnings: Vec<String>,
}

/// Wire-shaped request to invoke a named workflow with a JSON payload
/// (D-07) — `Serialize`/`Deserialize` even though `InProcessOrchestrator`
/// never crosses a process boundary in M1 (D-04).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeWorkflowRequest {
    pub workflow_id: String,
    pub payload: serde_json::Value,
}

/// Wire-shaped response carrying the `InvokeOutcome` envelope back to the
/// caller (ORCH-02), independent of which `Service` handled the request.
/// `run_id` is `None` for every synchronous (terminal) response and
/// `Some(..)` only on an async-mode `Started` ack (D-08/D-09/D-10) --
/// `server/mod.rs` is the sole producer of a `Some` value here, since
/// `RunHandle` (service-internal) never crosses this wire type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeWorkflowResponse {
    pub status: InvokeStatus,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub run_id: Option<String>,
}

/// Wire-shaped request to resolve a named workflow's declared
/// `ParameterSpec` (Phase 3 CLI-02 D-02/D-04) — the CLI's dynamic-flag
/// coercer and "expected parameters" error rendering are the primary
/// consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeWorkflowRequest {
    pub workflow_id: String,
}

/// A single declared parameter's wire-shaped schema (name/type/required),
/// mirroring `ParameterSpec` but flattened for the seam (the map key
/// becomes an explicit `name` field).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterDescriptor {
    pub name: String,
    pub type_: ParameterType,
    pub required: bool,
}

/// Wire-shaped response to `describe_workflow`. `parameters` is always
/// sorted ascending by `name` (Pitfall 5 / Phase 2 IN-02 precedent) so
/// every consumer renders a deterministic "expected parameters" list,
/// never `HashMap` iteration order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DescribeWorkflowResponse {
    pub found: bool,
    pub parameters: Vec<ParameterDescriptor>,
}

/// Wire-shaped mirror of `RunStatus` — kept as a separate type so the
/// client's wire contract doesn't couple directly to the internal
/// `service` module's enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvokeStatus {
    Completed,
    Failed,
    /// An async-mode (D-10) invocation has been accepted and dispatched as a
    /// background task; the terminal result follows later as an
    /// `Envelope::Event` (D-08/D-09). Never produced by `dispatch()`/
    /// `RunStatus` -- synthesized only by `server/mod.rs`'s immediate ack.
    Started,
}

/// Distinguishes a `CreateWorkflowRequest`'s write mode (quick task
/// 260812-qpn, D-2): `Create` refuses when `id` already exists
/// (D-DISC-06, unchanged); `Edit` refuses when it does NOT — the inverse
/// guard, an in-place overwrite of an existing definition.
///
/// Rides as a field ON `CreateWorkflowRequest` rather than becoming a new
/// `RequestPayload` variant. `RequestPayload` is `#[serde(untagged)]` and
/// serde tries each variant in declaration order — a hypothetical
/// `EditWorkflowRequest` sharing `CreateWorkflowRequest`'s field set would
/// be greedily absorbed by the `CreateWorkflow` variant listed first (the
/// exact hazard `envelope.rs`'s `RequestPayload` doc comment already
/// documents, D-DISC-03). Avoiding that would require `deny_unknown_fields`
/// on the existing request type — a riskier protocol change than adding one
/// defaulted field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowWriteMode {
    #[default]
    Create,
    Edit,
}

/// The AI-agent write-path discriminator (quick task 260813-rm5, D-1/D-6):
/// the second `CreateWorkflowRequest` field whose `Some`-ness alone marks
/// the request as producing a Claude-backed (`service.type: agent`)
/// workflow -- no separate kind enum, mirroring `script`'s existing
/// discriminator convention. Mutually exclusive with `script` at BOTH ends
/// of the seam: the CLI's `--agent` flag `conflicts_with` `--script`/
/// `--markdown` (D-1), and `registry::writer` refuses a request carrying
/// both `script: Some` and `agent: Some` before any filesystem call (D-6),
/// since `registry::loader` already rejects a workflow declaring both
/// `command` and `agent` (`LoadError::ConflictingServiceBinding`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// Declared file dependencies, crossing the wire as-is (D-3): the CLI
    /// never resolves, reads, or stats these paths -- they are DAEMON-side
    /// path strings that `loader.rs` anchors onto the workflow `.md`'s own
    /// directory, exactly like a hand-authored `.md`'s `files:` list.
    pub files: Vec<String>,
    /// Optional wall-clock ceiling in seconds. `None` omits the frontmatter
    /// key entirely so the daemon's own `DEFAULT_AGENT_TIMEOUT_SECS` applies
    /// at runtime (D-5) -- the CLI never bakes in a number of its own.
    pub timeout_secs: Option<u64>,
    /// Optional hard dollar cap for a single invocation. `None` omits the
    /// frontmatter key entirely so the daemon's own
    /// `DEFAULT_AGENT_MAX_BUDGET_USD` applies at runtime (D-5).
    pub max_budget_usd: Option<f64>,
}

/// Create (or, in edit mode, overwrite) a workflow definition in the
/// DAEMON's workflows directory (quick task 260807-shx, D-DISC-01; edit mode
/// added quick task 260812-qpn D-1/D-2). Field names deliberately avoid
/// `workflow_id` (used by both `InvokeWorkflowRequest` and
/// `DescribeWorkflowRequest`) so this variant can never be greedily matched
/// by, or greedily match, either of them during `#[serde(untagged)]`
/// resolution (see `envelope.rs`'s `RequestPayload` ordering doc comment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkflowRequest {
    /// The workflow id AND the `.md` filename stem. Daemon-validated.
    pub id: String,
    /// Human-facing frontmatter `name:`.
    pub name: String,
    /// Markdown body prose (the loader reads the body as `description`).
    pub description: String,
    /// Companion script CONTENT. `Some` is itself the "this is a
    /// script-backed workflow" discriminator — no separate kind enum.
    pub script: Option<String>,
    pub parameters: Vec<ParameterDescriptor>,
    /// `Create` (the default) refuses when `id` already exists; `Edit`
    /// refuses when it does NOT (quick task 260812-qpn D-2).
    /// `#[serde(default)]` so an omitted key — an older client, or an
    /// existing wire literal predating this field — still resolves to
    /// `Create`, keeping the wire backward compatible.
    #[serde(default)]
    pub mode: WorkflowWriteMode,
    /// AI-agent write-path discriminator (quick task 260813-rm5, D-1). A
    /// bare `Option` with NO `#[serde(default)]` attribute, deliberately
    /// mirroring `script`'s existing field exactly: serde already treats a
    /// missing `Option` field as `None` on deserialize, which is what keeps
    /// every existing wire literal (predating this field) valid with no
    /// attribute needed. Mutually exclusive with `script` (D-6) -- see
    /// `AgentConfig`'s doc comment for the full write-side/load-side
    /// symmetry. Rides on the existing request rather than becoming a new
    /// `RequestPayload` variant for the same reason `WorkflowWriteMode`
    /// does (see that field's doc comment above): `RequestPayload` is
    /// `#[serde(untagged)]`, and a hypothetical new variant sharing this
    /// shape would be greedily absorbed by `CreateWorkflow`'s earlier,
    /// less-specific-looking match.
    pub agent: Option<AgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkflowResponse {
    pub created: bool,
    /// Absolute path of the written `.md`, daemon-side. `None` on failure.
    pub workflow_path: Option<String>,
    /// Absolute path of the written script, daemon-side. `None` when no
    /// script was requested, or on failure.
    pub script_path: Option<String>,
    pub error: Option<String>,
}

/// A second, narrow trait (D-DISC-02) kept separate from
/// `OrchestratorClient` so `orchestrator-tui`'s `TuiWsClient` (which will
/// never create workflows) is not forced to grow an unused method.
/// Implemented by `InProcessOrchestrator` (daemon side) and
/// `WsOrchestratorClient` (CLI side).
#[async_trait]
pub trait WorkflowCreator: Send + Sync {
    async fn create_workflow(&self, req: CreateWorkflowRequest) -> CreateWorkflowResponse;
}

/// Remove an existing workflow definition from the DAEMON's own workflows
/// directory (quick task 260812-qp4, D-1). Field name deliberately `id`
/// (not `workflow_id`) — `workflow_id` would make this type shape-identical
/// to `DescribeWorkflowRequest` and unresolvable under
/// `#[serde(untagged)]` (see `envelope.rs`'s `RequestPayload` ordering doc
/// comment).
///
/// D-4: removing a definition never cancels an already-dispatched run.
/// `dispatch()` resolves the `WorkflowDefinition` once at invoke time and
/// the spawned handler never re-reads the `.md` mid-run, so an
/// already-running invocation of the deleted workflow completes normally.
/// Delete does not wait for, refuse on, or kill in-flight runs, and does
/// not consult `ActivityRegistry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteWorkflowRequest {
    /// The workflow id to remove. Daemon-validated.
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteWorkflowResponse {
    pub deleted: bool,
    /// Absolute path of the removed `.md`, daemon-side. `None` on failure.
    pub workflow_path: Option<String>,
    /// Absolute path of the removed companion script, daemon-side. `None`
    /// when the workflow declared no `service.command`, or on failure.
    pub script_path: Option<String>,
    pub error: Option<String>,
}

/// A second, narrow trait (D-DISC-02) mirroring `WorkflowCreator`, kept
/// separate from `OrchestratorClient` so `orchestrator-tui`'s `TuiWsClient`
/// (which will never delete workflows) is not forced to grow an unused
/// method. Implemented by `InProcessOrchestrator` (daemon side) and
/// `WsOrchestratorClient` (CLI side).
#[async_trait]
pub trait WorkflowDeleter: Send + Sync {
    async fn delete_workflow(&self, req: DeleteWorkflowRequest) -> DeleteWorkflowResponse;
}

/// The stable seam every consumer calls into the orchestrator through
/// (D-04). Kept `#[async_trait]` for dyn dispatch even with a single M1
/// implementation — explicit future-proofing toward the transport swap,
/// not incidental overhead.
#[async_trait]
pub trait OrchestratorClient: Send + Sync {
    async fn list_workflows(&self, req: ListWorkflowsRequest) -> ListWorkflowsResponse;

    /// Validate `req.payload` against the resolved workflow's
    /// `ParameterSpec`, dispatch to its declared handler, and return the
    /// structured result envelope (ORCH-01/ORCH-02).
    async fn invoke_workflow(&self, req: InvokeWorkflowRequest) -> InvokeWorkflowResponse;

    /// Resolve a named workflow's declared `ParameterSpec` for CLI-side
    /// dynamic-flag coercion (D-02/D-04). `found: false` for an unknown id
    /// rather than an error — callers branch on the flag, mirroring
    /// `invoke_workflow`'s existing unknown-workflow handling.
    async fn describe_workflow(&self, req: DescribeWorkflowRequest) -> DescribeWorkflowResponse;
}

/// Summary view returned by `Registry::enumerate()`. Derives
/// `Serialize`/`Deserialize` so it can cross the `OrchestratorClient`
/// seam (D-04/D-07).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSummary {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Closed enum (D-06) — any other frontmatter value is a load error
/// (`LoadError::UnknownParameterType`). Serde's derive for unit enums
/// automatically names the offending variant in its error message.
/// `Serialize` lets it cross the `OrchestratorClient` seam as part of
/// `ParameterDescriptor` (Phase 3's `describe_workflow`).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ParameterType {
    String,
    Int,
    Bool,
}
