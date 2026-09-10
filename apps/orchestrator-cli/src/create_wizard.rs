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
//! Wired into `main.rs`'s `WorkflowCommands::Create` dispatch arm (Task 3):
//! both `id`/`source` positionals `None` calls `run` below with
//! `std::io::stdin().lock()`/`std::io::stdout()`.

use std::io::{BufRead, Write};

use shared::wizard::{self, HandlerChoice, Step};
use shared::{
    AgentConfig, CreateWorkflowRequest, IntentCollisionChecker, ParameterDescriptor, ParameterType,
    WorkflowCreator, WorkflowWriteMode,
};

use crate::create::{agent_config_from_flags, derive_display_name, parse_param, AgentFlags};

// ---- Client-side cap mirrors (a UX affordance only; see module doc). ----
// Phase 10 plan 10-03 promoted these (and the step order/copy/validators
// below) into `shared::wizard` so `orchestrator-tui`'s wizard can never
// drift from this wording -- re-exported here under their original names
// so `tests/create_wizard_integration.rs`'s existing
// `create_wizard::MAX_*_MIRROR` assertions keep passing unmodified. Some of
// these (id/name/intent/agent-file-length) are no longer read by this
// file's own production code -- validation now calls straight into
// `shared::wizard` -- so they are read only by that `#[path]`-included test
// module, never by the `orchestrator` binary itself (this crate has no
// `[lib]` target, so `pub` does not make an item externally visible the
// way it would in a library crate).
#[allow(unused_imports)]
pub use shared::wizard::{
    MAX_AGENT_FILES_MIRROR, MAX_AGENT_FILE_LEN_MIRROR, MAX_INTENT_LEN_MIRROR, MAX_TRIGGERS_MIRROR,
    MAX_WORKFLOW_ID_LEN_MIRROR, MAX_WORKFLOW_NAME_LEN_MIRROR,
};

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

/// Runs the guided `workflow create` Q&A flow end to end. Returns the
/// process exit code (0 = success, nonzero = any failure). EOF at any
/// prompt returns nonzero and performs no write.
pub async fn run<R: BufRead, W: Write>(
    client: &dyn WorkflowCreator,
    checker: &dyn IntentCollisionChecker,
    input: &mut R,
    out: &mut W,
) -> std::io::Result<i32> {
    let Some(id) = prompt_id(input, out)? else {
        return Ok(1);
    };

    let Some(display_name) = prompt_display_name(input, out, &id)? else {
        return Ok(1);
    };

    let Some(description) = prompt_line(input, out, &wizard::prompt_for(Step::Description, ""))? else {
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
        let Some(timeout_secs) = prompt_optional_u64(input, out, &wizard::prompt_for(Step::AgentTimeout, ""))? else {
            return Ok(1);
        };
        let Some(max_budget_usd) = prompt_optional_f64(input, out, &wizard::prompt_for(Step::AgentBudget, ""))? else {
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

    // Step 8 (D-03/D-04): the blocking, fail-closed semantic-collision gate.
    // Placed strictly between parameter collection and the review/write
    // loop below -- there is structurally no path from any answer here to a
    // write that skips this gate (the single `create_workflow` call site is
    // downstream of this loop, never inside it).
    let Some(()) = run_collision_check(checker, input, out, &mut answers).await? else {
        return Ok(1);
    };

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

/// Step 8 (D-03/D-04): the blocking, fail-closed semantic-collision gate.
/// Implements the five states from UI-SPEC's Collision-check table:
///
/// - the in-progress line is rendered immediately before the checker call,
///   so a multi-second local-model turn never reads as a silent hang;
/// - a report with no colliding workflow and no `detail` (no prior
///   workflows, or a checked-and-clean intent) proceeds silently to review;
/// - a report naming a colliding workflow renders the warning line (score,
///   colliding id, colliding intent, both debug-quoted -- T-10-09, a
///   deliberate divergence from UI-SPEC's literal single-quote copy, since
///   another workflow's authored intent is untrusted input that must never
///   be able to forge an extra rendered line), then the save-anyway prompt
///   whose default is DECLINE;
/// - declining returns control to Step 5 (the intent-phrase prompt) so the
///   user can revise the wording, then the check re-runs against the
///   REVISED intent -- never step 1, never a full restart;
/// - a report carrying only a `detail` (the check itself failed) renders
///   the could-not-check warning and proceeds to review WITHOUT a
///   save-anyway prompt. This asymmetry is DELIBERATE and is UI-SPEC's own
///   design default, not a weakening of D-04: D-04 blocks on a DETECTED
///   collision, and hard-blocking wizard completion on a local-service
///   outage would defeat CREATEUI-01/02's own goal of a fast, low-friction
///   creation path.
///
/// Returns `Ok(None)` on EOF at any prompt within this gate (mirroring
/// every other step's EOF discipline) -- the caller returns exit code 1
/// with no write.
async fn run_collision_check<R: BufRead, W: Write>(
    checker: &dyn IntentCollisionChecker,
    input: &mut R,
    out: &mut W,
    answers: &mut WizardAnswers,
) -> std::io::Result<Option<()>> {
    loop {
        writeln!(out, "{}", wizard::prompt_for(Step::CollisionCheck, ""))?;
        let report = checker.check_intent_collision(&answers.intent).await;

        match (report.colliding_workflow_id, report.colliding_intent, report.similarity_score) {
            (Some(colliding_id), Some(colliding_intent), Some(score)) => {
                writeln!(
                    out,
                    "[WARN] '{}' is {score:.4} similar to existing workflow {:?} ({:?}). This may \
                     cause the router to confuse the two when routing by voice/utterance.",
                    answers.intent, colliding_id, colliding_intent
                )?;
                let Some(save_anyway) = prompt_save_anyway(input, out, "Save anyway? [y/N]:")? else {
                    return Ok(None);
                };
                if save_anyway {
                    return Ok(Some(()));
                }
                // Declined (fail-closed, D-04): return to Step 5 to revise
                // the wording, then re-run this same check against the
                // revised intent -- never step 1, never a full restart.
                let Some(new_intent) = prompt_intent(input, out)? else {
                    return Ok(None);
                };
                answers.intent = new_intent;
            }
            (None, None, None) => {
                if let Some(detail) = report.detail {
                    writeln!(
                        out,
                        "[WARN] Could not check for similar workflows ({detail}). Proceeding \
                         without a collision check -- you may want to verify '{}' doesn't overlap \
                         with an existing workflow's intent manually.",
                        answers.intent
                    )?;
                }
                // Either a clean "no collision" reply, or (when `detail` is
                // `Some`) the degrade path above -- both proceed straight to
                // review, per UI-SPEC's Collision-check table.
                return Ok(Some(()));
            }
            // Every real reply is either "all three collision fields Some"
            // or "all three None" (see `IntentCollisionReport`'s own doc
            // comment) -- a partially-populated reply cannot come from
            // either implementation of the trait. Treat it exactly like the
            // "no collision" branch (fail toward completing the wizard, not
            // toward blocking it) rather than panicking on a shape that
            // should be structurally unreachable.
            _ => return Ok(Some(())),
        }
    }
}

/// The save-anyway prompt (D-04, Step 8): default DECLINE -- a blank
/// answer, or anything other than an explicit `y`/`Y`, does not save.
/// Deliberately the OPPOSITE default of `prompt_confirm`'s `[Y/n]` review
/// gate, because this prompt guards a risky action (saving a workflow whose
/// intent may confuse the router) rather than confirming an expected one.
fn prompt_save_anyway<R: BufRead, W: Write>(input: &mut R, out: &mut W, prompt: &str) -> std::io::Result<Option<bool>> {
    let Some(raw) = prompt_line(input, out, prompt)? else {
        return Ok(None);
    };
    Ok(Some(matches!(raw.trim(), "y" | "Y")))
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

/// Step 1: loops on `shared::wizard::validate_id` until a valid id is
/// entered. Client-side rejection never reaches `WorkflowCreator` -- only a
/// fully valid id is ever returned.
fn prompt_id<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<String>> {
    loop {
        let Some(raw) = prompt_line(input, out, &wizard::prompt_for(Step::Id, ""))? else {
            return Ok(None);
        };
        let id = raw.trim().to_string();
        match wizard::validate_id(&id) {
            Ok(()) => return Ok(Some(id)),
            Err(err) => writeln!(out, "{}", err.line())?,
        }
    }
}

/// Step 2: optional -- a blank answer accepts the
/// `shared::wizard::derive_display_name(id)` default (`None`, resolved by
/// the caller at request-build time so a server-side id change on
/// re-prompt still derives from the CURRENT id). A given name loops on
/// `shared::wizard::validate_display_name` until valid.
fn prompt_display_name<R: BufRead, W: Write>(input: &mut R, out: &mut W, id: &str) -> std::io::Result<Option<Option<String>>> {
    loop {
        let Some(raw) = prompt_line(input, out, &wizard::prompt_for(Step::DisplayName, id))? else {
            return Ok(None);
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Some(None));
        }
        match wizard::validate_display_name(trimmed) {
            Ok(()) => return Ok(Some(Some(trimmed.to_string()))),
            Err(err) => writeln!(out, "{}", err.line())?,
        }
    }
}

/// Step 5: loops on `shared::wizard::validate_intent` until a valid intent
/// is entered.
fn prompt_intent<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<String>> {
    loop {
        let Some(raw) = prompt_line(input, out, &wizard::prompt_for(Step::Intent, ""))? else {
            return Ok(None);
        };
        let intent = raw.trim().to_string();
        match wizard::validate_intent(&intent) {
            Ok(()) => return Ok(Some(intent)),
            Err(err) => writeln!(out, "{}", err.line())?,
        }
    }
}

/// Step 4: the fixed two-item checklist (D-06). Loops until at least one
/// valid selection is made; free-text tokens that are not a listed number
/// are silently rejected rather than passed through as a trigger name.
fn prompt_triggers<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<Vec<String>>> {
    writeln!(out, "{}", wizard::prompt_for(Step::Triggers, ""))?;
    for line in wizard::menu_lines_for(Step::Triggers) {
        writeln!(out, "{line}")?;
    }
    loop {
        let Some(raw) = read_line(input)? else {
            return Ok(None);
        };
        let selected = parse_trigger_selection(&raw);
        if wizard::validate_triggers(&selected).is_err() {
            writeln!(out, "{}", wizard::TRIGGERS_EMPTY_LINE)?;
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
        let Some(choice) = wizard::TRIGGER_CHOICES.get(index) else {
            continue;
        };
        // Client-side mirror of the daemon's MAX_TRIGGERS cap. Unreachable
        // today (TRIGGER_CHOICES has only 2 entries), but this is the
        // correct defensive bound if the fixed checklist ever grows, and
        // matches the daemon's own enforcement for the same list.
        if result.len() >= MAX_TRIGGERS_MIRROR {
            break;
        }
        if !result.iter().any(|t| t == choice) {
            result.push((*choice).to_string());
        }
    }
    result
}

/// Step 6: the fixed three-item handler menu (D-05). Loops until one of
/// `1`/`2`/`3` is entered.
fn prompt_handler<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<HandlerChoice>> {
    writeln!(out, "{}", wizard::prompt_for(Step::HandlerType, ""))?;
    for line in wizard::menu_lines_for(Step::HandlerType) {
        writeln!(out, "{line}")?;
    }
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
/// (`shared::wizard::validate_agent_file`); a rejected entry is reported
/// and the prompt loops again without being added.
fn prompt_agent_files<R: BufRead, W: Write>(input: &mut R, out: &mut W) -> std::io::Result<Option<Vec<String>>> {
    writeln!(out, "{}", wizard::prompt_for(Step::AgentFiles, ""))?;
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
        match wizard::validate_agent_file(&raw) {
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
        let Some(raw) = prompt_line(input, out, &wizard::prompt_for(Step::Parameters, ""))? else {
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
                    writeln!(out, "{}", wizard::duplicate_parameter_message(&descriptor.name))?;
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
    writeln!(out, "{}", wizard::prompt_for(Step::Review, ""))?;
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
    writeln!(out, "  handler: {}", handler_label(answers.handler))?;
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

/// Human-readable handler-type label for the Step 9 review summary.
fn handler_label(handler: Option<HandlerChoice>) -> &'static str {
    match handler {
        Some(HandlerChoice::ScriptAction) => "script-backed action",
        Some(HandlerChoice::MarkdownAction) => "markdown-body action",
        Some(HandlerChoice::Agent) => "agent (Claude-backed)",
        None => "(none)",
    }
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
