//! Orchestrator library: workflow definition registry.
//!
//! Public library boundary consumed by Phase 2 (D-04). Command-name-agnostic.

pub mod activity;
pub mod agent_log;
pub mod client;
pub mod definition;
pub mod dispatch;
pub mod error;
pub mod handlers;
pub mod registry;
pub mod runtime;
pub mod server;
pub mod service;
pub mod startup;

pub use client::InProcessOrchestrator;
pub use definition::{ParameterSpec, ServiceMode, ServiceSpec, WorkflowDefinition};
pub use dispatch::{
    dispatch, validate_payload, InvokeOutcome, ENVELOPE_AGENT_KEY, ENVELOPE_ARGS_KEY,
    ENVELOPE_COMMAND_KEY,
};
pub use error::{CreateError, DeleteError, DispatchError, LoadError, ServerError, ValidationError};
pub use registry::Registry;
pub use service::{RunHandle, RunStatus, Service, ServiceError};
pub use shared::{
    DescribeWorkflowRequest, DescribeWorkflowResponse, Envelope, InvokeStatus,
    InvokeWorkflowRequest, InvokeWorkflowResponse, ListWorkflowsRequest, ListWorkflowsResponse,
    OrchestratorClient, ParameterDescriptor, ParameterType, RequestPayload, ResponsePayload,
    WorkflowSummary, DEFAULT_PORT,
};
