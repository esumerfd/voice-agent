//! `route` subcommand entry point (plan 09-03, D-02/D-03): a DRY-RUN trial
//! surface for the embedding intent router. Reports what the router would
//! select for a given utterance -- it never starts anything; real workflow
//! execution stays exclusively on the `run` path.
//!
//! Generic over `Write` for stdout, mirroring `list.rs`'s convention, so the
//! integration test captures output deterministically without racing real
//! process stdout. Takes `&WsOrchestratorClient` directly (not
//! `&dyn OrchestratorClient`) since `call_protocol` is an inherent method,
//! not part of that trait -- adding it to the trait would force every
//! implementor to carry a protocol path it does not need.

use std::io::Write;

use shared::ProtocolFrame;

use crate::ws_client::WsOrchestratorClient;

/// Runs the `route` subcommand: sends `utterance` as a `RouteUtterance`
/// frame and prints the daemon's `RouteResult` reply. Returns 0 whenever the
/// daemon replied at all -- including a no-match, which is a successful
/// trial, not a CLI error -- and nonzero only on a transport failure or an
/// unexpected reply shape.
pub async fn run<W: Write>(
    client: &WsOrchestratorClient,
    utterance: &str,
    out: &mut W,
) -> std::io::Result<i32> {
    let reply = client
        .call_protocol(ProtocolFrame::RouteUtterance {
            utterance: utterance.to_string(),
        })
        .await;

    let frame = match reply {
        Ok(frame) => frame,
        Err(detail) => {
            writeln!(out, "error: {detail}")?;
            return Ok(1);
        }
    };

    match frame {
        ProtocolFrame::RouteResult {
            utterance,
            matched_workflow_id,
            similarity_score,
            confirm_tier,
            extracted_params,
            detail,
        } => {
            writeln!(out, "DRY RUN -- this is a trial only, nothing was started.")?;
            writeln!(out, "utterance: {utterance}")?;
            match &matched_workflow_id {
                Some(id) => writeln!(out, "matched workflow: {id}")?,
                None => writeln!(out, "matched workflow: (no match)")?,
            }
            match similarity_score {
                Some(score) => writeln!(out, "similarity score: {score:.4}")?,
                None => writeln!(out, "similarity score: -")?,
            }
            match &confirm_tier {
                Some(tier) => writeln!(out, "confirmation tier: {tier}")?,
                None => writeln!(out, "confirmation tier: -")?,
            }
            match &extracted_params {
                Some(params) => writeln!(
                    out,
                    "extracted parameters: {}",
                    serde_json::to_string_pretty(params).unwrap_or_else(|_| params.to_string())
                )?,
                None => writeln!(out, "extracted parameters: -")?,
            }
            if let Some(detail) = &detail {
                writeln!(out, "detail: {detail}")?;
            }
            Ok(0)
        }
        other => {
            writeln!(out, "error: unexpected reply from orchestratord: {other:?}")?;
            Ok(1)
        }
    }
}
