//! Async orchestrator client seam (D-04/D-07): the only path a consumer
//! (CLI today, `orchestratord` transport later) uses to reach the
//! `Registry`. Request/response types are already `Serialize`/
//! `Deserialize` even though `InProcessOrchestrator` never crosses a
//! process boundary in M1 — extracting a real daemon later is a
//! transport-only change, not a type change.
//!
//! Companion: `registry/mod.rs` (`Registry::load`/`enumerate`, the sole
//! data source behind this seam).

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::dispatch::dispatch;
use crate::registry::Registry;
use crate::definition::ServiceMode;
use crate::service::{RunStatus, Service};
use shared::{
    CreateWorkflowRequest, CreateWorkflowResponse, DeleteWorkflowRequest, DeleteWorkflowResponse,
    DescribeWorkflowRequest, DescribeWorkflowResponse, InvokeStatus, InvokeWorkflowRequest,
    InvokeWorkflowResponse, ListWorkflowsRequest, ListWorkflowsResponse, OrchestratorClient,
    ParameterDescriptor, WorkflowCreator, WorkflowDeleter,
};

impl From<RunStatus> for InvokeStatus {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Completed => InvokeStatus::Completed,
            RunStatus::Failed => InvokeStatus::Failed,
            // Invariant: dispatch()'s poll loop only ever returns a terminal
            // RunStatus (Completed/Failed) inside InvokeOutcome — Running is
            // fully resolved internally before this conversion ever runs. A
            // silent `_ => InvokeStatus::Failed` catch-all would hide a real
            // bug if that invariant were ever violated; panic loudly instead.
            RunStatus::Running => unreachable!("dispatch() only returns terminal states"),
        }
    }
}

/// The M1 implementation: calls straight into the in-process `Registry`.
/// `handlers` is empty by default — per D-08, no real `Service` ships in
/// M1, so every `invoke_workflow` call resolves to `HandlerNotFound` until
/// Phase 3 registers real handlers via `with_handlers`.
pub struct InProcessOrchestrator {
    workflows_dir: PathBuf,
    handlers: HashMap<String, Box<dyn Service>>,
}

impl InProcessOrchestrator {
    pub fn new(workflows_dir: impl Into<PathBuf>) -> Self {
        Self {
            workflows_dir: workflows_dir.into(),
            handlers: HashMap::new(),
        }
    }

    /// Construct with a pre-populated handler registry (keyed by
    /// `service.handler`). Used by Phase 3's real handler registration and
    /// by tests exercising the full `invoke_workflow` path.
    pub fn with_handlers(
        workflows_dir: impl Into<PathBuf>,
        handlers: HashMap<String, Box<dyn Service>>,
    ) -> Self {
        Self {
            workflows_dir: workflows_dir.into(),
            handlers,
        }
    }

    /// Resolve a named workflow's declared `service.mode` (D-10), defaulting
    /// to `Sync` for an unknown workflow id -- `invoke_workflow` itself
    /// remains the sole authority for "unknown workflow" errors; this
    /// accessor only answers the sync/async branch question for
    /// `server::handle_request` (RESEARCH Pattern 4). Just a `Registry`
    /// lookup, never a re-implementation of validation/dispatch (Pitfall 3).
    pub fn resolve_service_mode(&self, workflow_id: &str) -> ServiceMode {
        let (registry, _errors) = Registry::load(&self.workflows_dir);
        registry
            .lookup(workflow_id)
            .map(|definition| definition.service.mode)
            .unwrap_or_default()
    }

    /// Resolve a named workflow's display name + source `.md` file (quick
    /// task 260719-foq's per-run log line). Mirrors `resolve_service_mode`'s
    /// pattern exactly (`Registry::load` then `lookup`) so `server/mod.rs`
    /// never reaches into `Registry` directly (Pitfall 3). `None` for an
    /// unknown workflow id -- never a default, unlike `resolve_service_mode`.
    pub fn workflow_log_info(&self, workflow_id: &str) -> Option<WorkflowLogInfo> {
        let (registry, _errors) = Registry::load(&self.workflows_dir);
        registry.lookup(workflow_id).map(|definition| WorkflowLogInfo {
            name: definition.name.clone(),
            source_path: definition.source_path.clone(),
        })
    }
}

/// The display name + source `.md` file used for a workflow's per-run log
/// line (quick task 260719-foq).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowLogInfo {
    pub name: String,
    pub source_path: PathBuf,
}

#[async_trait]
impl OrchestratorClient for InProcessOrchestrator {
    async fn list_workflows(&self, _req: ListWorkflowsRequest) -> ListWorkflowsResponse {
        let (registry, errors) = Registry::load(&self.workflows_dir);

        ListWorkflowsResponse {
            workflows: registry.enumerate(),
            warnings: errors.iter().map(|e| e.to_string()).collect(),
        }
    }

    async fn invoke_workflow(&self, req: InvokeWorkflowRequest) -> InvokeWorkflowResponse {
        let (registry, _errors) = Registry::load(&self.workflows_dir);

        let Some(definition) = registry.lookup(&req.workflow_id) else {
            return InvokeWorkflowResponse {
                status: InvokeStatus::Failed,
                output: None,
                error: Some(format!("unknown workflow `{}`", req.workflow_id)),
                run_id: None,
            };
        };

        // A non-object, non-null top-level payload is rejected outright
        // (CR-01): silently coercing it to `{}` would discard caller data
        // with no error. `Null` stays lenient (Postel) and maps to an empty
        // parameter map — the caller's "no arguments" intent still dispatches.
        let payload = match req.payload {
            serde_json::Value::Object(map) => map,
            serde_json::Value::Null => serde_json::Map::new(),
            other => {
                return InvokeWorkflowResponse {
                    status: InvokeStatus::Failed,
                    output: None,
                    error: Some(format!(
                        "payload for workflow `{}` must be a JSON object, got: {other}",
                        req.workflow_id
                    )),
                    run_id: None,
                };
            }
        };

        match dispatch(definition, payload, &self.handlers).await {
            Ok(outcome) => InvokeWorkflowResponse {
                status: outcome.status.into(),
                output: outcome.output,
                error: outcome.error,
                run_id: None,
            },
            Err(err) => InvokeWorkflowResponse {
                status: InvokeStatus::Failed,
                output: None,
                error: Some(err.to_string()),
                run_id: None,
            },
        }
    }

    async fn describe_workflow(&self, req: DescribeWorkflowRequest) -> DescribeWorkflowResponse {
        let (registry, _errors) = Registry::load(&self.workflows_dir);

        let Some(definition) = registry.lookup(&req.workflow_id) else {
            return DescribeWorkflowResponse {
                found: false,
                parameters: Vec::new(),
            };
        };

        let mut parameters: Vec<ParameterDescriptor> = definition
            .parameters
            .iter()
            .map(|(name, spec)| ParameterDescriptor {
                name: name.clone(),
                type_: spec.type_,
                required: spec.required,
            })
            .collect();
        parameters.sort_by(|a, b| a.name.cmp(&b.name));

        DescribeWorkflowResponse {
            found: true,
            parameters,
        }
    }
}

/// `InProcessOrchestrator`'s `WorkflowCreator` implementation (quick task
/// 260807-shx, D-06): `workflows_dir` here is always the DAEMON's own
/// directory, constructed at startup -- it is never taken from the request,
/// which is the entire point of routing the write through this seam rather
/// than letting the CLI touch the filesystem directly.
#[async_trait]
impl WorkflowCreator for InProcessOrchestrator {
    async fn create_workflow(&self, req: CreateWorkflowRequest) -> CreateWorkflowResponse {
        match crate::registry::writer::create_workflow(&self.workflows_dir, &req) {
            Ok(outcome) => CreateWorkflowResponse {
                created: true,
                workflow_path: Some(outcome.workflow_path.display().to_string()),
                script_path: outcome.script_path.map(|p| p.display().to_string()),
                error: None,
            },
            Err(err) => CreateWorkflowResponse {
                created: false,
                workflow_path: None,
                script_path: None,
                error: Some(err.to_string()),
            },
        }
    }
}

/// `InProcessOrchestrator`'s `WorkflowDeleter` implementation (quick task
/// 260812-qp4, D-06): `workflows_dir` here is always the DAEMON's own
/// directory, constructed at startup -- it is never taken from the request,
/// mirroring `WorkflowCreator`'s same discipline above. Per D-3, no
/// registry-invalidation or reload call is needed here: every read path
/// (`list_workflows`/`invoke_workflow`/`describe_workflow`/
/// `resolve_service_mode`) already re-runs `Registry::load` on every
/// request, so a removed `.md` is unlistable/unrunnable on the very next
/// request with no daemon restart.
#[async_trait]
impl WorkflowDeleter for InProcessOrchestrator {
    async fn delete_workflow(&self, req: DeleteWorkflowRequest) -> DeleteWorkflowResponse {
        match crate::registry::writer::delete_workflow(&self.workflows_dir, &req.id) {
            Ok(outcome) => DeleteWorkflowResponse {
                deleted: true,
                workflow_path: Some(outcome.workflow_path.display().to_string()),
                script_path: outcome.script_path.map(|p| p.display().to_string()),
                error: None,
            },
            Err(err) => DeleteWorkflowResponse {
                deleted: false,
                workflow_path: None,
                script_path: None,
                error: Some(err.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn write_workflow(dir: &std::path::Path, filename: &str, contents: &str) {
        std::fs::write(dir.join(filename), contents).expect("failed to write fixture");
    }

    #[test]
    fn workflow_log_info_returns_name_and_source_path_for_a_known_workflow_id() {
        let dir = tempdir().expect("tempdir");
        write_workflow(
            dir.path(),
            "loginfo.md",
            r#"---
id: loginfo
name: Log Info Workflow
service:
  type: action
  handler: demo.loginfo
---
Used to assert workflow_log_info's name/source_path lookup.
"#,
        );

        let orchestrator = InProcessOrchestrator::new(dir.path());

        let info = orchestrator
            .workflow_log_info("loginfo")
            .expect("expected a known workflow id to resolve to Some(WorkflowLogInfo)");
        assert_eq!(info.name, "Log Info Workflow");
        assert_eq!(
            info.source_path.file_name().and_then(|n| n.to_str()),
            Some("loginfo.md"),
            "expected source_path's file name to match the fixture file"
        );
    }

    #[test]
    fn workflow_log_info_returns_none_for_an_unknown_workflow_id() {
        let dir = tempdir().expect("tempdir");
        let orchestrator = InProcessOrchestrator::new(dir.path());

        assert!(
            orchestrator.workflow_log_info("does_not_exist").is_none(),
            "expected an unknown workflow id to resolve to None"
        );
    }
}
