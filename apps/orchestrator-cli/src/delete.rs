//! `workflow delete` subcommand entry point (quick task 260812-qp4).
//!
//! Mirrors `create.rs`'s conventions: generic over `Write` for stdout so
//! tests capture output deterministically, takes `&dyn shared::WorkflowDeleter`
//! (never a concrete client type and never a `crate::ws_client` reference --
//! that is what keeps the `#[path]` test-inclusion trick compiling,
//! D-DISC-02), renders both success and failure as JSON to `out`, and never
//! panics on caller input.
//!
//! `render_success`/`render_error`/`pretty_json` are duplicated from
//! `create.rs` rather than extracted into a shared module:
//! `create_integration.rs` and `delete_integration.rs` both pull these
//! files in via `#[path]` with no `[lib]` target, so a new cross-module
//! `crate::` reference would break compilation of both test binaries
//! (D-DISC-02).

use std::io::Write;

use serde_json::json;

use shared::{DeleteWorkflowRequest, WorkflowDeleter};

/// Runs the `workflow delete` subcommand: sends `id` over `WorkflowDeleter`
/// and renders success/error JSON to `out`. Returns the process exit code
/// (0 = success, nonzero = any failure, including an unknown id -- D-2,
/// delete is not idempotent).
pub async fn run<W: Write>(client: &dyn WorkflowDeleter, id: &str, out: &mut W) -> std::io::Result<i32> {
    let req = DeleteWorkflowRequest { id: id.to_string() };

    let resp = client.delete_workflow(req).await;

    if resp.deleted {
        render_success(
            out,
            json!({
                "deleted": true,
                "workflow_path": resp.workflow_path,
                "script_path": resp.script_path,
            }),
        )
    } else {
        render_error(out, resp.error.unwrap_or_else(|| "unknown error".to_string()))
    }
}

fn render_success<W: Write>(out: &mut W, value: serde_json::Value) -> std::io::Result<i32> {
    writeln!(out, "{}", pretty_json(&value))?;
    Ok(0)
}

fn render_error<W: Write>(out: &mut W, message: String) -> std::io::Result<i32> {
    writeln!(out, "{}", pretty_json(&json!({ "error": message })))?;
    Ok(1)
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{value:?}"))
}
