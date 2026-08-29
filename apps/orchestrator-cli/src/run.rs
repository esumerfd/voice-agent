//! `run` subcommand entry point (CLI-02/CLI-03).
//!
//! Deliberately thin, mirroring `list.rs`'s conventions: generic over
//! `Write` so tests capture output deterministically without spawning a
//! process, no per-workflow branching anywhere (D-01/roadmap criterion 5 —
//! dispatch is purely by the resolved `service.handler` string, resolved
//! entirely inside the `orchestrator` lib). Unlike `list.rs`, BOTH success
//! (D-08) and failure (D-09) render as JSON to `out` (stdout) — stdout is
//! not a success/failure routing primitive here; the exit code (D-10,
//! returned to the caller for `main.rs` to feed into `std::process::exit`)
//! is. Never panics on caller-supplied input (T-03-05) — every failure path
//! renders a JSON `error` object instead.
//!
//! Takes `&dyn OrchestratorClient` rather than constructing its own client:
//! `main.rs` owns client construction (now the WS-backed
//! `WsOrchestratorClient`, D-06) and passes it in, so this module never
//! needs to know which transport or concrete `Service`s exist. An async-mode
//! invoke (D-08/D-10) acks immediately with `InvokeStatus::Started` +
//! `run_id` instead of a terminal result -- rendered here as a distinct
//! "started" JSON object, exit 0, never treated as an error.

use std::io::Write;

use serde_json::{json, Map, Value};

use shared::{
    DescribeWorkflowRequest, InvokeStatus, InvokeWorkflowRequest, OrchestratorClient,
    ParameterDescriptor, ParameterType,
};

/// Runs the `run` subcommand: resolves `workflow_id`'s declared parameters
/// via `describe_workflow`, parses/coerces `raw_flags` against them,
/// dispatches through `invoke_workflow`, and renders success/error JSON to
/// `out`. Returns the process exit code (0 = success, nonzero = any
/// failure, D-10) for the caller to act on.
pub async fn run<W: Write>(
    client: &dyn OrchestratorClient,
    workflow_id: &str,
    raw_flags: &[String],
    out: &mut W,
) -> std::io::Result<i32> {
    let describe = client
        .describe_workflow(DescribeWorkflowRequest {
            workflow_id: workflow_id.to_string(),
        })
        .await;

    if !describe.found {
        return render_error(out, format!("unknown workflow `{workflow_id}`"));
    }

    let mut parameters = describe.parameters;
    parameters.sort_by(|a, b| a.name.cmp(&b.name));

    let payload = match coerce_flags(raw_flags, &parameters) {
        Ok(payload) => payload,
        Err(message) => return render_error(out, message),
    };

    let response = client
        .invoke_workflow(InvokeWorkflowRequest {
            workflow_id: workflow_id.to_string(),
            payload: Value::Object(payload),
        })
        .await;

    if response.status == InvokeStatus::Started {
        // Async ack (D-08/D-10): orchestratord accepted and dispatched this
        // run in the background; the terminal result follows later as an
        // Envelope::Event this Phase 4 CLI does not yet consume (see
        // ws_client.rs's reserved events channel). This is not a failure --
        // render the acknowledgement and exit 0.
        let run_id = response.run_id.unwrap_or_default();
        return render_success(out, json!({ "status": "started", "run_id": run_id }));
    }

    if let Some(error) = response.error {
        return render_error(out, error);
    }

    render_success(out, response.output.unwrap_or(Value::Null))
}

/// Walks `raw_flags`, pairing each `--name` with the value token(s) its
/// resolved `ParameterDescriptor` expects (D-02): a bool descriptor
/// consumes zero following tokens and coerces to `true` (D-03 presence
/// flag; absent bools are defaulted to `false` below); a string descriptor
/// consumes exactly one following token as a JSON string; an int
/// descriptor consumes exactly one following token parsed as an integer.
/// Never checks `required` here — `validate_payload` downstream (via
/// `invoke_workflow`) remains the sole authority for missing-required
/// errors (T-03-04); this function only owns argv-shape errors (unknown
/// flag, dangling flag, unparseable int token).
fn coerce_flags(
    raw_flags: &[String],
    parameters: &[ParameterDescriptor],
) -> Result<Map<String, Value>, String> {
    let mut payload = Map::new();
    let mut i = 0;

    while i < raw_flags.len() {
        let flag = &raw_flags[i];
        let Some(name) = flag.strip_prefix("--") else {
            return Err(format!(
                "expected a `--flag` style argument, got `{flag}`; expected parameters: {}",
                expected_parameters(parameters)
            ));
        };

        let Some(descriptor) = parameters.iter().find(|p| p.name == name) else {
            return Err(format!(
                "unknown parameter flag `--{name}`; expected parameters: {}",
                expected_parameters(parameters)
            ));
        };

        match descriptor.type_ {
            ParameterType::Bool => {
                payload.insert(descriptor.name.clone(), json!(true));
                i += 1;
            }
            ParameterType::String => {
                let Some(value) = raw_flags.get(i + 1) else {
                    return Err(format!(
                        "flag `--{name}` expects a value; expected parameters: {}",
                        expected_parameters(parameters)
                    ));
                };
                payload.insert(descriptor.name.clone(), json!(value));
                i += 2;
            }
            ParameterType::Int => {
                let Some(raw_value) = raw_flags.get(i + 1) else {
                    return Err(format!(
                        "flag `--{name}` expects a value; expected parameters: {}",
                        expected_parameters(parameters)
                    ));
                };
                let parsed: i64 = raw_value.parse().map_err(|_| {
                    format!(
                        "flag `--{name}` expects an integer value, got `{raw_value}`; expected parameters: {}",
                        expected_parameters(parameters)
                    )
                })?;
                payload.insert(descriptor.name.clone(), json!(parsed));
                i += 2;
            }
        }
    }

    // Bool params default to false when absent (D-03) — a bool is
    // inherently optional; the caller's omission always means false,
    // satisfying validate_payload even if the workflow marks it required.
    for descriptor in parameters {
        if descriptor.type_ == ParameterType::Bool && !payload.contains_key(&descriptor.name) {
            payload.insert(descriptor.name.clone(), json!(false));
        }
    }

    Ok(payload)
}

/// Formats the workflow's declared parameters as `name (type, required|
/// optional)`, sorted ascending by name (Pitfall 5 — deterministic, never
/// `HashMap` order) for D-04's "expected parameters" error text.
fn expected_parameters(parameters: &[ParameterDescriptor]) -> String {
    let mut sorted: Vec<&ParameterDescriptor> = parameters.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted
        .iter()
        .map(|p| {
            format!(
                "{} ({}, {})",
                p.name,
                type_name(p.type_),
                if p.required { "required" } else { "optional" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn type_name(type_: ParameterType) -> &'static str {
    match type_ {
        ParameterType::String => "string",
        ParameterType::Int => "int",
        ParameterType::Bool => "bool",
    }
}

/// Success rendering (D-08): a plain JSON string prints bare, any other
/// JSON value pretty-prints as an object/array/etc. No envelope noise, no
/// status line, no per-workflow prose (D-01).
fn render_success<W: Write>(out: &mut W, value: Value) -> std::io::Result<i32> {
    match value {
        Value::String(text) => writeln!(out, "{text}")?,
        other => writeln!(out, "{}", pretty_json(&other))?,
    }
    Ok(0)
}

/// Failure rendering (D-09): always a single `{"error": ...}` JSON object
/// on `out` (stdout, never stderr) — this is the one convention `run.rs`
/// deliberately does NOT share with `list.rs`'s stderr warnings.
fn render_error<W: Write>(out: &mut W, message: String) -> std::io::Result<i32> {
    writeln!(out, "{}", pretty_json(&json!({ "error": message })))?;
    Ok(1)
}

/// Pretty-prints a `Value` — either a handler's own `result()` output or
/// this module's own error object. `to_string_pretty` on a `serde_json::
/// Value` is effectively total for these shapes, but never `.expect()`/
/// `.unwrap()` on data that ultimately traces back to caller input or a
/// third-party handler (T-03-05): fall back to `{value:?}` rather than
/// panic if serialization ever did fail.
fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{value:?}"))
}
