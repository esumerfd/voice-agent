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
//! Walks the full nine-step sequence documented in
//! `10-UI-SPEC.md`'s Copywriting Contract, in that exact order: workflow
//! id, display name, description, trigger types, intent phrase, handler
//! type, agent sub-prompts (only when Agent is selected), the parameter
//! loop, a Step 8 collision-check seam (a no-op placeholder in this plan --
//! plan 10-02 fills it in), and the Step 9 review/confirmation. On a
//! server-side rejection (e.g. the id already exists), only Step 1 (the
//! id) is re-prompted -- every other answer is preserved and the flow
//! returns directly to Step 9's review, never restarting from the top.
//!
//! Client-side validation here (`validate_id_client` and friends) is a UX
//! affordance, not an enforcement boundary: `registry::writer` remains the
//! sole authority and re-validates every request regardless of what this
//! module rejects or accepts. Nothing here may weaken or bypass it.
//!
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

use shared::{
    AgentConfig, CreateWorkflowRequest, ParameterDescriptor, ParameterType, WorkflowCreator, WorkflowWriteMode,
};

use crate::create::{agent_config_from_flags, derive_display_name, parse_param, AgentFlags};

// ---- Client-side cap mirrors (a UX affordance only; see module doc). ----
// `orchestrator-cli` cannot import `registry::writer`'s constants directly
// in production code (separate binary crate; `orchestrator` is only a
// dev-dependency of this crate, used by its own test harnesses). Promoting
// these caps into `shared` is the cleaner long-term home -- out of this
// phase's boundary (10-CONTEXT.md: no changes to the write path's
// validation rules). `tests/create_wizard_integration.rs` asserts each of
// these equals the real constant it mirrors, so drift is a test failure.

/// Mirrors `registry::writer::MAX_WORKFLOW_ID_LEN`.
pub const MAX_WORKFLOW_ID_LEN_MIRROR: usize = 64;
/// Mirrors `registry::writer::MAX_WORKFLOW_NAME_LEN`.
pub const MAX_WORKFLOW_NAME_LEN_MIRROR: usize = 200;
/// Mirrors `registry::writer::MAX_INTENT_LEN`.
pub const MAX_INTENT_LEN_MIRROR: usize = 1024;
/// Mirrors `registry::writer::MAX_TRIGGERS`.
pub const MAX_TRIGGERS_MIRROR: usize = 32;
/// Mirrors `registry::writer::MAX_AGENT_FILES`.
pub const MAX_AGENT_FILES_MIRROR: usize = 50;
/// Mirrors `registry::writer::MAX_AGENT_FILE_LEN`.
pub const MAX_AGENT_FILE_LEN_MIRROR: usize = 512;

/// The fixed trigger checklist (D-06) -- never free text.
pub const TRIGGER_CHOICES: &[&str] = &["cli", "voice"];

/// Handler-type selection (D-05, Step 6) -- an exhaustive enum so the
/// branch deriving the request's script/agent/description triple cannot
/// silently miss a shape, mirroring how `create::run` derives the same
/// triple from its own classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerChoice {
    ScriptAction,
    MarkdownAction,
    Agent,
}

/// A rejected answer's field name and the daemon's own reason wording
/// (mirroring `registry::writer`'s reason strings verbatim where
/// applicable), rendered via `line()` as `[ERROR] {field} {reason}.` --
/// UI-SPEC's generic validation-error shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WizardError {
    pub field: &'static str,
    pub reason: String,
}

impl WizardError {
    fn line(&self) -> String {
        format!("[ERROR] {} {}.", self.field, self.reason)
    }
}

/// The collected, validated field set (built up incrementally over the
/// nine steps). Only converted into a `CreateWorkflowRequest` at Step 9's
/// confirmation.
#[derive(Debug, Clone, Default)]
pub struct WizardAnswers {
    pub id: String,
    pub display_name: Option<String>,
    pub description: String,
    pub triggers: Vec<String>,
    pub intent: String,
    pub handler: Option<HandlerChoice>,
    pub agent_files: Vec<String>,
    pub timeout_secs: Option<u64>,
    pub max_budget_usd: Option<f64>,
    pub parameters: Vec<ParameterDescriptor>,
}

/// Verbatim mirror of `registry::writer::validate_id` (writer.rs lines
/// 359-392): non-empty, at most `MAX_WORKFLOW_ID_LEN_MIRROR` characters,
/// every character in `[a-z0-9_-]`, first character alphanumeric. Reason
/// strings match the daemon's own character for character (UI-SPEC Step 1)
/// so `[ERROR] id {reason}.` renders identically whether caught here or
/// returned by the daemon.
pub fn validate_id_client(id: &str) -> Result<(), WizardError> {
    if id.is_empty() {
        return Err(WizardError {
            field: "id",
            reason: "must not be empty".to_string(),
        });
    }
    if id.chars().count() > MAX_WORKFLOW_ID_LEN_MIRROR {
        return Err(WizardError {
            field: "id",
            reason: format!("must be at most {MAX_WORKFLOW_ID_LEN_MIRROR} characters"),
        });
    }
    if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
        return Err(WizardError {
            field: "id",
            reason: "must contain only lowercase letters, digits, `_`, and `-`".to_string(),
        });
    }
    let first = id.chars().next().expect("id is non-empty, checked above");
    if !first.is_ascii_alphanumeric() {
        return Err(WizardError {
            field: "id",
            reason: "must start with an alphanumeric character".to_string(),
        });
    }
    Ok(())
}

/// Client-side mirror of the daemon's intent-length enforcement (UI-SPEC
/// Step 5): required, non-empty after trim, at most
/// `MAX_INTENT_LEN_MIRROR` Unicode code points.
fn validate_intent_client(intent: &str) -> Result<(), WizardError> {
    if intent.trim().is_empty() {
        return Err(WizardError {
            field: "intent",
            reason: "must not be empty".to_string(),
        });
    }
    if intent.chars().count() > MAX_INTENT_LEN_MIRROR {
        return Err(WizardError {
            field: "intent",
            reason: format!("must be at most {MAX_INTENT_LEN_MIRROR} characters"),
        });
    }
    Ok(())
}

/// Client-side mirror of the daemon's per-entry agent-file validation
/// (UI-SPEC Step 6a): non-empty, no NUL byte, at most
/// `MAX_AGENT_FILE_LEN_MIRROR` Unicode code points.
fn validate_agent_file_client(entry: &str) -> Result<(), WizardError> {
    if entry.is_empty() {
        return Err(WizardError {
            field: "agent file",
            reason: "must not be empty".to_string(),
        });
    }
    if entry.contains('\0') {
        return Err(WizardError {
            field: "agent file",
            reason: "must not contain a NUL byte".to_string(),
        });
    }
    if entry.chars().count() > MAX_AGENT_FILE_LEN_MIRROR {
        return Err(WizardError {
            field: "agent file",
            reason: format!("must be at most {MAX_AGENT_FILE_LEN_MIRROR} characters"),
        });
    }
    Ok(())
}

/// Runs the guided `workflow create` Q&A flow end to end. Returns the
/// process exit code (0 = success, nonzero = any failure). EOF at any
/// prompt returns nonzero and performs no write.
pub async fn run<R: BufRead, W: Write>(client: &dyn WorkflowCreator, input: &mut R, out: &mut W) -> std::io::Result<i32> {
    let Some(id) = prompt_id(input, out)? else {
        return Ok(1);
    };

    let default_name = derive_display_name(&id);
    let Some(display_name_raw) = prompt_line(input, out, &format!("Display name [default: {default_name}]:"))? else {
        return Ok(1);
    };
    let display_name = if display_name_raw.trim().is_empty() {
        None
    } else {
        Some(display_name_raw.trim().to_string())
    };

    let Some(description) = prompt_line(input, out, "Description (workflow body text):")? else {
        return Ok(1);
    };

    let Some(triggers) = prompt_triggers(input, out)? else {
        return Ok(1);
    };

    let Some(intent) = prompt_intent(input, out)? else {
        return Ok(1);
    };

    let Some(handler) = prompt_handler(input, out)? else {
        return Ok(1);
    };

    let (agent_files, timeout_secs, max_budget_usd) = if handler == HandlerChoice::Agent {
        let Some(files) = prompt_agent_files(input, out)? else {
            return Ok(1);
        };
        let Some(timeout_secs) = prompt_optional_u64(input, out, "Timeout in seconds [default: daemon default]:")? else {
            return Ok(1);
        };
        let Some(max_budget_usd) = prompt_optional_f64(input, out, "Max budget in USD [default: daemon default]:")? else {
            return Ok(1);
        };
        (files, timeout_secs, max_budget_usd)
    } else {
        (Vec::new(), None, None)
    };

    let Some(parameters) = prompt_parameters(input, out)? else {
        return Ok(1);
    };

    let mut answers = WizardAnswers {
        id,
        display_name,
        description,
        triggers,
        intent,
        handler: Some(handler),
        agent_files,
        timeout_secs,
        max_budget_usd,
        parameters,
    };

    // Step 8 (D-03/D-04): a no-op placeholder seam in this plan -- plan
    // 10-02 fills it in with the real embedding-based collision check
    // against every existing workflow's intent.
    let _ = passes_collision_check(&answers);

    loop {
        render_review(out, &answers)?;
        let Some(confirmed) = prompt_confirm(input, out, &format!("Create workflow '{}'? [Y/n]:", answers.id))? else {
            return Ok(1);
        };
        if !confirmed {
            return Ok(0);
        }

        let (body_description, script) = derive_write_shape(handler, &answers.description);
        let agent = build_agent_config(handler, &answers.agent_files, answers.timeout_secs, answers.max_budget_usd);
        let name = answers.display_name.clone().unwrap_or_else(|| derive_display_name(&answers.id));

        let req = CreateWorkflowRequest {
            id: answers.id.clone(),
            name,
            description: body_description,
            script,
            parameters: answers.parameters.clone(),
            mode: WorkflowWriteMode::Create,
            agent,
            intent: Some(answers.intent.clone()),
            triggers: answers.triggers.clone(),
        };

        let resp = client.create_workflow(req).await;
        if resp.created {
            writeln!(
                out,
                "[OK] Workflow '{}' created at {}.",
                answers.id,
                resp.workflow_path.unwrap_or_default()
            )?;
            writeln!(out, "Try it now: orchestrator run {}", answers.id)?;
            return Ok(0);
        }

        writeln!(out, "[ERROR] {}", resp.error.unwrap_or_else(|| "unknown error".to_string()))?;
        // Re-prompt Step 1 (id) only -- every other answer is preserved and
        // the flow returns directly to this same review/confirm loop,
        // never restarting from the top.
        let Some(new_id) = prompt_id(input, out)? else {
            return Ok(1);
        };
        answers.id = new_id;
    }
}

/// Step 8 seam (D-03/D-04): plan 10-02 implements the real Ollama-embedding
/// cosine-similarity collision check here. A no-op placeholder in this
/// plan -- always reports "no collision found", so Step 9 (Review) is
/// always reached directly.
fn passes_collision_check(_answers: &WizardAnswers) -> bool {
    true
}

/// Derives the request's `(description, script)` pair from the selected
/// handler type (D-05), mirroring how `create::run` derives the same pair
/// from its own script-vs-markdown classification: a script-backed action
/// sends the collected description text as the script content with an
/// empty body description; markdown and agent both send it as the body.
fn derive_write_shape(handler: HandlerChoice, description: &str) -> (String, Option<String>) {
    match handler {
        HandlerChoice::ScriptAction => (String::new(), Some(description.to_string())),
        HandlerChoice::MarkdownAction | HandlerChoice::Agent => (description.to_string(), None),
    }
}

/// Builds the `Option<AgentConfig>` for the request (D-05), reusing
/// `create::agent_config_from_flags` rather than constructing one by hand
/// so the two entry paths (flag-driven and guided) cannot diverge.
fn build_agent_config(
    handler: HandlerChoice,
    files: &[String],
    timeout_secs: Option<u64>,
    max_budget_usd: Option<f64>,
) -> Option<AgentConfig> {
    if handler != HandlerChoice::Agent {
        return None;
    }
    let flags = AgentFlags {
        agent: true,
        agent_file: files.to_vec(),
        timeout_secs,
        max_budget_usd,
    };
    agent_config_from_flags(&flags).expect("AgentFlags{agent: true, ..} never returns Err")
}

/// Step 1: loops on `validate_id_client` until a valid id is entered.
/// Client-side rejection never reaches `WorkflowCreator` -- only a fully
/// valid id is ever returned.
fn prompt_id<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<String>> {
    loop {
        let Some(raw) = prompt_line(input, out, "Workflow id (used as the .md filename):")? else {
            return Ok(None);
        };
        let id = raw.trim().to_string();
        match validate_id_client(&id) {
            Ok(()) => return Ok(Some(id)),
            Err(err) => writeln!(out, "{}", err.line())?,
        }
    }
}

/// Step 5: loops on `validate_intent_client` until a valid intent is
/// entered.
fn prompt_intent<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<String>> {
    loop {
        let Some(raw) = prompt_line(
            input,
            out,
            "Intent phrase (a sentence describing when this should fire, used for voice/utterance routing):",
        )?
        else {
            return Ok(None);
        };
        let intent = raw.trim().to_string();
        match validate_intent_client(&intent) {
            Ok(()) => return Ok(Some(intent)),
            Err(err) => writeln!(out, "{}", err.line())?,
        }
    }
}

/// Step 4: the fixed two-item checklist (D-06). Loops until at least one
/// valid selection is made; free-text tokens that are not a listed number
/// are silently rejected rather than passed through as a trigger name.
fn prompt_triggers<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<Vec<String>>> {
    writeln!(out, "Select trigger types (at least one):")?;
    for (index, choice) in TRIGGER_CHOICES.iter().enumerate() {
        writeln!(out, "  {}) {}", index + 1, choice)?;
    }
    loop {
        let Some(raw) = read_line(input)? else {
            return Ok(None);
        };
        let selected = parse_trigger_selection(&raw);
        if selected.is_empty() {
            writeln!(out, "Select at least one trigger type.")?;
            continue;
        }
        return Ok(Some(selected));
    }
}

/// Resolves a raw, possibly comma-separated trigger-selection answer
/// (`"1"`, `"2"`, `"1,2"`) to its `TRIGGER_CHOICES` entries, de-duplicated
/// and in selection order. A token that is not a listed number (free text,
/// out of range, or unparsable) is silently ignored -- never passed
/// through as a trigger name (D-06).
fn parse_trigger_selection(raw: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let Ok(number) = token.parse::<usize>() else {
            continue;
        };
        let Some(index) = number.checked_sub(1) else {
            continue;
        };
        let Some(choice) = TRIGGER_CHOICES.get(index) else {
            continue;
        };
        if !result.iter().any(|t| t == choice) {
            result.push((*choice).to_string());
        }
    }
    result
}

/// Step 6: the fixed three-item handler menu (D-05). Loops until one of
/// `1`/`2`/`3` is entered.
fn prompt_handler<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<HandlerChoice>> {
    writeln!(out, "Select handler type:")?;
    writeln!(out, "  1) Script-backed action — runs a script or command you provide")?;
    writeln!(out, "  2) Markdown-body action — description text becomes the action body, no script")?;
    writeln!(out, "  3) Agent (Claude-backed) — runs a Claude Code prompt")?;
    loop {
        let Some(raw) = read_line(input)? else {
            return Ok(None);
        };
        match raw.trim() {
            "1" => return Ok(Some(HandlerChoice::ScriptAction)),
            "2" => return Ok(Some(HandlerChoice::MarkdownAction)),
            "3" => return Ok(Some(HandlerChoice::Agent)),
            _ => writeln!(out, "[ERROR] handler must be 1, 2, or 3.")?,
        }
    }
}

/// Step 6a (Agent only): repeatable reference-file entries, a blank line
/// finishes the list. Each entry is validated client-side
/// (`validate_agent_file_client`); a rejected entry is reported and the
/// prompt loops again without being added.
fn prompt_agent_files<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<Vec<String>>> {
    writeln!(out, "Reference files (repeatable, blank line to finish):")?;
    let mut files = Vec::new();
    loop {
        let Some(raw) = read_line(input)? else {
            return Ok(None);
        };
        if raw.trim().is_empty() {
            return Ok(Some(files));
        }
        if files.len() >= MAX_AGENT_FILES_MIRROR {
            writeln!(out, "[ERROR] agent files must be at most {MAX_AGENT_FILES_MIRROR} entries.")?;
            continue;
        }
        match validate_agent_file_client(&raw) {
            Ok(()) => files.push(raw),
            Err(err) => writeln!(out, "{}", err.line())?,
        }
    }
}

/// Steps 6b/6c (Agent only): an optional numeric field. A blank answer
/// omits the field entirely (`Some(None)`), never substituting a number --
/// the daemon's own default applies at runtime.
fn prompt_optional_u64<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    prompt: &str,
) -> std::io::Result<Option<Option<u64>>> {
    loop {
        let Some(raw) = prompt_line(input, out, prompt)? else {
            return Ok(None);
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Some(None));
        }
        match trimmed.parse::<u64>() {
            Ok(value) => return Ok(Some(Some(value))),
            Err(_) => writeln!(out, "[ERROR] timeout must be a whole number of seconds.")?,
        }
    }
}

/// Steps 6b/6c (Agent only): the `f64` counterpart of
/// `prompt_optional_u64`, for the max-budget field.
fn prompt_optional_f64<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    prompt: &str,
) -> std::io::Result<Option<Option<f64>>> {
    loop {
        let Some(raw) = prompt_line(input, out, prompt)? else {
            return Ok(None);
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Some(None));
        }
        match trimmed.parse::<f64>() {
            Ok(value) => return Ok(Some(Some(value))),
            Err(_) => writeln!(out, "[ERROR] budget must be a number.")?,
        }
    }
}

/// Step 7: the add-a-parameter loop. Each collected `name:type:required`
/// triple is parsed by `create::parse_param` (reused verbatim, never
/// reimplemented); a duplicate name is rejected inline without being
/// added, and the loop continues asking. Answering anything other than
/// `y`/`Y` ends the loop; zero parameters is a valid, common outcome.
fn prompt_parameters<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<Vec<ParameterDescriptor>>> {
    let mut parameters: Vec<ParameterDescriptor> = Vec::new();
    loop {
        let Some(raw) = prompt_line(input, out, "Add a parameter? [y/N]:")? else {
            return Ok(None);
        };
        if !matches!(raw.trim(), "y" | "Y") {
            return Ok(Some(parameters));
        }

        let Some(name) = prompt_line(input, out, "Name:")? else {
            return Ok(None);
        };
        let Some(type_raw) = prompt_line(input, out, "Type (string/int/bool):")? else {
            return Ok(None);
        };
        let Some(required_raw) = prompt_line(input, out, "Required? [y/N]:")? else {
            return Ok(None);
        };
        let required = matches!(required_raw.trim(), "y" | "Y");

        let spec = format!(
            "{}:{}:{}",
            name.trim(),
            type_raw.trim(),
            if required { "required" } else { "optional" }
        );
        match parse_param(&spec) {
            Ok(descriptor) => {
                if parameters.iter().any(|p| p.name == descriptor.name) {
                    writeln!(out, "Parameter '{}' already added.", descriptor.name)?;
                } else {
                    parameters.push(descriptor);
                }
            }
            Err(message) => writeln!(out, "[ERROR] parameter {message}.")?,
        }
    }
}

/// Step 9's confirmation: default Y on a blank answer; any answer other
/// than blank/`y`/`Y` cancels without writing.
fn prompt_confirm<R: BufRead, W: Write>(input: &mut R, out: &mut W, prompt: &str) -> std::io::Result<Option<bool>> {
    let Some(raw) = prompt_line(input, out, prompt)? else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    Ok(Some(trimmed.is_empty() || matches!(trimmed, "y" | "Y")))
}

/// Step 9: the full review summary. Zero, one, and many parameters all
/// render as a plain bulleted list under a `Parameters:` heading -- zero
/// renders the single line `Parameters: none` instead, with no
/// singular/plural copy variation for the populated case.
fn render_review<W: Write>(out: &mut W, answers: &WizardAnswers) -> std::io::Result<()> {
    writeln!(out, "Review:")?;
    writeln!(
        out,
        "  id: {}",
        answers.id
    )?;
    writeln!(
        out,
        "  name: {}",
        answers.display_name.clone().unwrap_or_else(|| derive_display_name(&answers.id))
    )?;
    writeln!(out, "  triggers: {}", answers.triggers.join(", "))?;
    writeln!(out, "  intent: {}", answers.intent)?;
    if answers.parameters.is_empty() {
        writeln!(out, "Parameters: none")?;
    } else {
        writeln!(out, "Parameters:")?;
        for param in &answers.parameters {
            let type_str = match param.type_ {
                ParameterType::String => "string",
                ParameterType::Int => "int",
                ParameterType::Bool => "bool",
            };
            let required_str = if param.required { "required" } else { "optional" };
            writeln!(out, "  - {}: {} ({})", param.name, type_str, required_str)?;
        }
    }
    Ok(())
}

/// Writes `prompt` followed by a newline, then reads one line of answer.
/// Returns `Ok(None)` on EOF (a closed stdin).
fn prompt_line<R: BufRead, W: Write>(input: &mut R, out: &mut W, prompt: &str) -> std::io::Result<Option<String>> {
    writeln!(out, "{prompt}")?;
    read_line(input)
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
