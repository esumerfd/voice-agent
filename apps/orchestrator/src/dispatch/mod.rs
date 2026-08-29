//! Dispatch: validate a JSON payload against a workflow's `ParameterSpec`
//! (D-07), resolve the declared `service.handler` in a handler registry,
//! invoke it through the `Service` trait (ORCH-01), and assemble the
//! structured `InvokeOutcome` envelope handed back to the caller (ORCH-02) —
//! independent of which concrete `Service` ran.

use std::collections::HashMap;

use serde_json::Value;

use crate::definition::{ParameterSpec, ParameterType, WorkflowDefinition};
use crate::error::{DispatchError, ValidationError};
use crate::service::{RunStatus, Service};

/// Interval `dispatch()`'s poll loop sleeps for between `status()` checks
/// while a handler reports `RunStatus::Running` (D-07). 100ms keeps
/// CPU-only-hardware overhead negligible (T-03-02) while staying far below
/// any test's short injected durations (Pitfall 4).
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Reserved dispatch->handler transport key carrying the trusted, configured
/// command path (quick task 260720-s7i). This namespace is owned entirely by
/// `dispatch()`: it is the SOLE place that both SETS it (from
/// `definition.service.command`, never from the caller payload) and STRIPS
/// it (from any caller-supplied payload, on every dispatch, regardless of
/// whether a command is configured) -- so a caller can never forge a spawn
/// trigger. Re-exported from `lib.rs` so a future handler (e.g. plan 02's
/// `ImmediateService` spawn path) imports this SAME constant and the
/// envelope producer/consumer can never drift on spelling.
pub const ENVELOPE_COMMAND_KEY: &str = "__command";

/// Reserved dispatch->handler transport key carrying the trusted, configured
/// args array (quick task 260720-s7i). Same ownership/strip discipline as
/// `ENVELOPE_COMMAND_KEY`.
pub const ENVELOPE_ARGS_KEY: &str = "__args";

/// Reserved dispatch->handler transport key carrying the trusted AI-agent
/// envelope (06-01, SVC-03, T-06-07). Same ownership/strip discipline as
/// `ENVELOPE_COMMAND_KEY`: `dispatch()` is the SOLE place that both SETS it
/// (from `definition.service.agent`, never from the caller payload) and
/// STRIPS it (from any caller-supplied payload, on every dispatch,
/// unconditionally) -- so a caller can never forge an agent envelope, even
/// on an agent-less workflow.
pub const ENVELOPE_AGENT_KEY: &str = "__agent";

/// The structured result of an invocation (ORCH-02), shaped the same way
/// regardless of which `Service` handled the request.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeOutcome {
    pub status: RunStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
}

/// Validate a JSON payload against a workflow's declared `ParameterSpec`
/// (D-07). Handlers never see a payload that fails this check. See
/// 02-RESEARCH.md Pattern 5 / Pitfall 2 — `Int` must reject floats.
pub fn validate_payload(
    payload: &serde_json::Map<String, Value>,
    spec: &HashMap<String, ParameterSpec>,
) -> Result<(), ValidationError> {
    for (name, param_spec) in spec {
        match payload.get(name) {
            None if param_spec.required => {
                return Err(ValidationError::MissingRequired { param: name.clone() })
            }
            None => continue, // optional and absent — fine
            Some(value) => {
                let ok = match param_spec.type_ {
                    ParameterType::String => value.is_string(),
                    // Reject floats explicitly — `Value::Number` covers both
                    // integers and floats; `is_i64()`/`is_u64()` excludes
                    // floats (Pitfall 2: never a bare `is_number()`).
                    ParameterType::Int => value.is_i64() || value.is_u64(),
                    ParameterType::Bool => value.is_boolean(),
                };
                if !ok {
                    return Err(ValidationError::WrongType {
                        param: name.clone(),
                        expected: param_spec.type_,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Validate the payload, resolve `definition.service.handler` in
/// `handlers`, invoke it, and assemble the `InvokeOutcome` envelope
/// (ORCH-01/ORCH-02). Validation runs before any handler lookup or
/// invocation (D-07) — handlers never see invalid input.
pub async fn dispatch(
    definition: &WorkflowDefinition,
    payload: serde_json::Map<String, Value>,
    handlers: &HashMap<String, Box<dyn Service>>,
) -> Result<InvokeOutcome, DispatchError> {
    validate_payload(&payload, &definition.parameters)?;

    let handler = handlers
        .get(&definition.service.handler)
        .ok_or_else(|| DispatchError::HandlerNotFound {
            handler: definition.service.handler.clone(),
        })?;

    // Strip any caller-supplied reserved transport keys BEFORE branching
    // (quick task 260720-s7i) -- `__command`/`__args` are an internal
    // dispatch->handler namespace, never caller-addressable, in either
    // branch below. This is the defense against a caller forging a spawn
    // trigger on a command-less workflow (T-s7i-01/T-s7i-02).
    let mut stripped_payload = payload;
    stripped_payload.remove(ENVELOPE_COMMAND_KEY);
    stripped_payload.remove(ENVELOPE_ARGS_KEY);
    stripped_payload.remove(ENVELOPE_AGENT_KEY);

    // Build the value handed to the handler: when an agent is configured,
    // wrap the stripped caller payload in a server-constructed `__agent`
    // envelope built ONLY from the trusted `WorkflowDefinition` -- never
    // from the caller (06-01, SVC-03, T-06-07). Matched BEFORE the command
    // branch -- this match reads only definition data, never the handler
    // string (AI-SPEC E-10). When a command is configured (and no agent,
    // enforced mutually exclusive by `loader.rs`), wrap in the `__command`
    // envelope as before. Otherwise pass the stripped payload through.
    let handler_input = if let Some(agent) = &definition.service.agent {
        let mut agent_envelope = serde_json::Map::new();
        agent_envelope.insert("workflow_id".to_string(), Value::String(definition.id.clone()));
        agent_envelope.insert("body".to_string(), Value::String(definition.description.clone()));
        // 06-02 staging boundary (D-03/E-03/G-03): the directory containing
        // the workflow `.md` file, sourced ONLY from the trusted
        // `WorkflowDefinition::source_path` -- never from caller input.
        // `AgentRuntime::prepare` anchors every declared `files` entry's
        // containment check against this directory.
        agent_envelope.insert(
            "workflow_dir".to_string(),
            Value::String(
                definition
                    .source_path
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        );
        agent_envelope.insert(
            "files".to_string(),
            Value::Array(
                agent
                    .files
                    .iter()
                    .map(|f| Value::String(f.to_string_lossy().into_owned()))
                    .collect(),
            ),
        );
        agent_envelope.insert(
            "model".to_string(),
            agent.model.clone().map(Value::String).unwrap_or(Value::Null),
        );
        agent_envelope.insert(
            "max_budget_usd".to_string(),
            agent.max_budget_usd.map(Value::from).unwrap_or(Value::Null),
        );
        agent_envelope.insert(
            "timeout_secs".to_string(),
            agent.timeout_secs.map(Value::from).unwrap_or(Value::Null),
        );

        let mut envelope = serde_json::Map::new();
        envelope.insert(ENVELOPE_AGENT_KEY.to_string(), Value::Object(agent_envelope));
        envelope.insert("params".to_string(), Value::Object(stripped_payload));
        Value::Object(envelope)
    } else {
        match &definition.service.command {
            Some(cmd) => {
                let mut envelope = serde_json::Map::new();
                envelope.insert(
                    ENVELOPE_COMMAND_KEY.to_string(),
                    Value::String(cmd.to_string_lossy().into_owned()),
                );
                envelope.insert(
                    ENVELOPE_ARGS_KEY.to_string(),
                    Value::Array(
                        definition
                            .service
                            .args
                            .iter()
                            .cloned()
                            .map(Value::String)
                            .collect(),
                    ),
                );
                envelope.insert("params".to_string(), Value::Object(stripped_payload));
                Value::Object(envelope)
            }
            None => Value::Object(stripped_payload),
        }
    };

    let handle = match handler.invoke(handler_input).await {
        Ok(handle) => handle,
        Err(service_error) => {
            return Ok(InvokeOutcome {
                status: RunStatus::Failed,
                output: None,
                error: Some(service_error.detail),
            })
        }
    };

    // Poll until a terminal state (D-06/D-07). `Running` sleeps and loops;
    // `Completed`/`Failed` break out to the existing envelope construction.
    // Accepted risk (T-03-01): a handler that never reports a terminal state
    // polls unboundedly — client-side timeout/cancellation is deferred (M1
    // scope), Ctrl-C is the only recourse. Never `std::thread::sleep` here
    // (Pitfall 2) — it would freeze the single `current_thread` runtime.
    loop {
        match handler.status(&handle).await {
            Ok(RunStatus::Running) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
            Ok(RunStatus::Completed) => {
                return match handler.result(&handle).await {
                    Ok(output) => Ok(InvokeOutcome {
                        status: RunStatus::Completed,
                        output,
                        error: None,
                    }),
                    Err(service_error) => Ok(InvokeOutcome {
                        status: RunStatus::Failed,
                        output: None,
                        error: Some(service_error.detail),
                    }),
                }
            }
            Ok(RunStatus::Failed) => {
                return Ok(InvokeOutcome {
                    status: RunStatus::Failed,
                    output: None,
                    error: Some(format!(
                        "handler `{}` reported a failed run",
                        definition.service.handler
                    )),
                })
            }
            Err(service_error) => {
                return Ok(InvokeOutcome {
                    status: RunStatus::Failed,
                    output: None,
                    error: Some(service_error.detail),
                })
            }
        }
    }
}
