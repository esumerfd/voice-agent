//! Interactive guided workflow-creation Q&A flow (Phase 10, plan 10-01,
//! CREATEUI-01/CREATEUI-02, D-01). Entered when `orchestrator workflow
//! create` is invoked with no positional arguments (see `cli.rs`/
//! `main.rs`'s dispatch branch, plan 10-01 Task 3).
//!
//! Mirrors `create.rs`'s discipline (its module doc, lines 13-18): generic
//! over `Write` for stdout so tests capture output deterministically, takes
//! `&dyn shared::WorkflowCreator` (never a concrete client type and never a
//! `crate::ws_client` reference -- D-DISC-02, load-bearing for the
//! `#[path]` test-inclusion trick this crate uses since it has no `[lib]`
//! target). `input` is generic over `BufRead` for the same reason `out` is
//! generic over `Write`: a test feeds a canned Q&A transcript
//! deterministically. There is no prior interactive-stdin precedent in this
//! codebase -- this is the one genuinely new pattern Phase 10's CLI half
//! introduces.
//!
//! Deliberate divergence from `create.rs`: this is an interactive surface,
//! not a scriptable one, so it renders human-readable `[OK]`/`[ERROR]`
//! lines (UI-SPEC's Terminal States table) rather than a JSON blob. It
//! still returns the same `i32` exit-code convention (0 = success, nonzero
//! = any failure), and never panics on caller input -- a closed stdin (EOF)
//! at any prompt returns nonzero and writes nothing.
//!
//! This module (Task 1) wires the THINNEST path all the way down: workflow
//! id, intent phrase, and one trigger selection. Steps 2, 3, 6, 6a-6c, 7, 8
//! and the full review/confirmation summary are Task 2's scope.

//! `#[allow(dead_code)]` below: this module is not yet reached from
//! `main.rs`'s dispatch -- Task 3 of this plan wires the guided-mode branch
//! onto `workflow create`'s `Option<String>` positionals. Every item here
//! IS exercised, from `tests/create_wizard_integration.rs`'s own separate
//! `#[path]`-included compilation of this file, mirroring `ws_client.rs`'s
//! identical `pending_protocol`/`events` situation (this crate has no
//! `[lib]` target, so the bin target and each test binary compile this file
//! independently).
#![allow(dead_code)]

use std::io::{BufRead, Write};

use shared::{CreateWorkflowRequest, WorkflowCreator, WorkflowWriteMode};

use crate::create::derive_display_name;

/// The fixed trigger checklist (D-06) -- never free text.
pub const TRIGGER_CHOICES: &[&str] = &["cli", "voice"];

/// Runs the guided `workflow create` Q&A flow: prompts for a workflow id,
/// an intent phrase, and one trigger selection, then sends a
/// `CreateWorkflowRequest` through `client` and renders a human-readable
/// `[OK]`/`[ERROR]` line to `out`. Returns the process exit code (0 =
/// success, nonzero = any failure). EOF at any prompt returns nonzero and
/// performs no write -- the single `create_workflow` call below is only
/// ever reached once every prompt has a real answer.
pub async fn run<R: BufRead, W: Write>(client: &dyn WorkflowCreator, input: &mut R, out: &mut W) -> std::io::Result<i32> {
    writeln!(out, "Workflow id (used as the .md filename):")?;
    let Some(id) = read_line(input)? else {
        return Ok(1);
    };
    let id = id.trim().to_string();

    writeln!(
        out,
        "Intent phrase (a sentence describing when this should fire, used for voice/utterance routing):"
    )?;
    let Some(intent) = read_line(input)? else {
        return Ok(1);
    };
    let intent = intent.trim().to_string();

    writeln!(out, "Select trigger types (at least one):")?;
    for (index, choice) in TRIGGER_CHOICES.iter().enumerate() {
        writeln!(out, "  {}) {}", index + 1, choice)?;
    }
    let Some(selection) = read_line(input)? else {
        return Ok(1);
    };
    let Some(trigger) = parse_trigger_selection(&selection) else {
        writeln!(out, "[ERROR] triggers Select at least one trigger type.")?;
        return Ok(1);
    };

    let name = derive_display_name(&id);
    let req = CreateWorkflowRequest {
        id: id.clone(),
        name,
        description: String::new(),
        script: None,
        parameters: Vec::new(),
        mode: WorkflowWriteMode::Create,
        agent: None,
        intent: Some(intent),
        triggers: vec![trigger.to_string()],
    };

    let resp = client.create_workflow(req).await;
    if resp.created {
        writeln!(
            out,
            "[OK] Workflow '{id}' created at {}.",
            resp.workflow_path.unwrap_or_default()
        )?;
        writeln!(out, "Try it now: orchestrator run {id}")?;
        Ok(0)
    } else {
        writeln!(out, "[ERROR] {}", resp.error.unwrap_or_else(|| "unknown error".to_string()))?;
        Ok(1)
    }
}

/// Resolves a raw trigger-selection answer (`"1"`/`"2"`) to its
/// `TRIGGER_CHOICES` entry. Free-text input that is not a listed number is
/// rejected -- `None` -- rather than passed through as a trigger name
/// (D-06).
fn parse_trigger_selection(raw: &str) -> Option<&'static str> {
    let index: usize = raw.trim().parse().ok()?;
    let index = index.checked_sub(1)?;
    TRIGGER_CHOICES.get(index).copied()
}

/// Reads one line from `input`, stripping the trailing newline. Returns
/// `Ok(None)` on EOF (a closed stdin) rather than an empty string, so
/// callers can distinguish "the user pressed Enter on an empty line" from
/// "there is no more input".
fn read_line<R: BufRead>(input: &mut R) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let bytes_read = input.read_line(&mut line)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
}
