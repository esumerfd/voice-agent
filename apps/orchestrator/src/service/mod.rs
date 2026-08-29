//! The Service Contract (ORCH-01): an async `Service` trait every handler
//! implements, dyn-dispatchable via `async-trait` (D-06) so the orchestrator
//! can hold heterogeneous handlers behind `Box<dyn Service>` without knowing
//! their concrete types. `invoke`/`status`/`cancel`/`result` form the full
//! lifecycle (D-05) — trivial for M1's synchronous stub, but the shape a
//! real long-running handler (Phase 3, SVC-01/02) grows into without a
//! contract change. `result()` is the Service Contract's genuine "output
//! schema out" channel (ORCH-01/ORCH-02, design-overview.md lines 165-166) —
//! the orchestrator sources `InvokeOutcome.output` from it, never from the
//! caller's own input payload.
//!
//! No `Service` implementation lives in this module or anywhere under
//! `src/` (D-08) — the only implementation this phase is a test-only stub
//! under `apps/orchestrator/tests/dispatch_integration.rs`.

use async_trait::async_trait;

/// Opaque handle to a single `Service::invoke` call. The inner identity is
/// intentionally private — callers treat it as a token, never construct one
/// directly, mirroring `Registry`'s encapsulation convention. `Hash` is
/// derived so a `Service` implementation can key an internal map (e.g.
/// `TimerService`'s deadline tracking) directly by the opaque handle,
/// without ever exposing the inner string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunHandle(String);

impl RunHandle {
    /// Construct a handle from an opaque identity string. `Service`
    /// implementations call this from `invoke`; nothing else needs to know
    /// the inner representation.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Lifecycle states for a dispatched invocation (D-05). `Completed`/`Failed`
/// are terminal — `dispatch()`'s poll loop (D-06/D-07) only ever RETURNS one
/// of these two inside `InvokeOutcome`. `Running` is the sole non-terminal
/// state: a long-running handler (e.g. `TimerService`) reports it from
/// `status()` while work is still in flight, and `dispatch()` polls again
/// after a short sleep rather than treating it as a completion. Any code
/// converting `RunStatus` into a caller-facing type (e.g. `client::
/// InvokeStatus`) can rely on never observing `Running` there — see
/// `client/mod.rs`'s `From<RunStatus> for InvokeStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
}

/// Error surfaced by a `Service` implementation. `detail` is a
/// human-readable message; the orchestrator carries it into the
/// `InvokeOutcome` envelope on failure (ORCH-02).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceError {
    pub detail: String,
}

/// The async Service Contract (ORCH-01/D-05/D-06). Every handler — action,
/// script, API integration, or (later) AI-agent — implements this trait
/// uniformly; the orchestrator never special-cases a handler by name.
#[async_trait]
pub trait Service: Send + Sync {
    /// Invoke the handler with a validated JSON payload (D-07 — validation
    /// happens before this is ever called) and return a handle to the run.
    async fn invoke(&self, input: serde_json::Value) -> Result<RunHandle, ServiceError>;

    /// Query the lifecycle status of a previously-returned handle.
    async fn status(&self, handle: &RunHandle) -> Result<RunStatus, ServiceError>;

    /// Cancel a previously-returned handle. A no-op for synchronous
    /// handlers (D-05) — real long-running handlers give this teeth later.
    async fn cancel(&self, handle: &RunHandle) -> Result<(), ServiceError>;

    /// Retrieve the handler's own produced output for a completed run
    /// (ORCH-01/ORCH-02 — the "output schema out" half of the Service
    /// Contract, design-overview.md lines 165-166). Called by the
    /// orchestrator after `status()` reports `Completed`, to retrieve the
    /// marshaled result out-of-band. Returns `Ok(None)` when the run
    /// legitimately produces no output (e.g. a void action) — `result()`,
    /// never the input payload, is the sole source of `InvokeOutcome.output`.
    async fn result(&self, handle: &RunHandle) -> Result<Option<serde_json::Value>, ServiceError>;
}
