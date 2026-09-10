//! Shared wire-protocol crate: the `OrchestratorClient` seam, its DTOs, and
//! the `Envelope` framing that crosses the process boundary between
//! `orchestratord` and `orchestrator-cli`. Extracted so `orchestrator-cli`'s
//! production code depends only on the serializable wire contract, never the
//! full `orchestrator` registry/dispatch/handler/server machinery.

pub mod activity;
pub mod capability;
pub mod client;
pub mod envelope;
pub mod wizard;

pub use activity::{ActivityEvent, ActivityLogEvent, ActivityPhase, ActivityStatus};
pub use capability::{filter_activity_event_for, filter_output_for, CAPABILITY_SPEECH, GATED_CAPABILITY_FIELDS};
pub use client::{
    AgentConfig, CreateWorkflowRequest, CreateWorkflowResponse, DeleteWorkflowRequest,
    DeleteWorkflowResponse, DescribeWorkflowRequest, DescribeWorkflowResponse, IntentCollisionChecker,
    IntentCollisionReport, InvokeStatus, InvokeWorkflowRequest, InvokeWorkflowResponse,
    ListWorkflowsRequest, ListWorkflowsResponse, OrchestratorClient, ParameterDescriptor,
    ParameterType, WorkflowCreator, WorkflowDeleter, WorkflowSummary, WorkflowWriteMode,
};
pub use envelope::{Envelope, ProtocolFrame, RequestPayload, ResponsePayload, DEFAULT_PORT};
pub use wizard::{
    derive_display_name, duplicate_parameter_message, menu_lines_for, prompt_for, validate_agent_file,
    validate_display_name, validate_id, validate_intent, validate_triggers, FieldError, HandlerChoice, Step,
    MAX_AGENT_FILES_MIRROR, MAX_AGENT_FILE_LEN_MIRROR, MAX_INTENT_LEN_MIRROR, MAX_TRIGGERS_MIRROR,
    MAX_WORKFLOW_ID_LEN_MIRROR, MAX_WORKFLOW_NAME_LEN_MIRROR, TRIGGERS_EMPTY_LINE, TRIGGER_CHOICES,
};
