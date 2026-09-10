//! `Focus::Wizard`'s pure, TTY-free step machine (Phase 10, plan 10-03):
//! `WizardState` walks the same nine steps (plus the three agent-only
//! sub-steps, D-05) in the same order, with the same wording and
//! validation, as the CLI flow proven in 10-01/10-02 (D-02) -- ported, not
//! redesigned. Mirrors `app.rs`'s `Activity`/`App` split: `WizardState` is
//! the domain model with pure, independently-testable mutators; `App`
//! stays the focus owner (a single `Option<WizardState>` field, plus
//! `Focus::Wizard`, in `app.rs`).
//!
//! Every prompt, label, validation rule, and error string comes from
//! `shared::wizard` -- never a literal declared in this file. Task 1
//! single-sourced that wording precisely so it cannot drift between
//! surfaces; a local string here would silently reintroduce the drift
//! Task 1 exists to prevent.
//!
//! Rendering and the write path (the actual daemon call, and turning
//! `collision_check_in_flight` into a real async check) are 10-04's scope.
//! This module stops at the pure step machine: every mutator here is
//! TTY-free and never panics on any buffer state, so the whole nine-step
//! flow -- all three handler shapes (D-05) and the fixed trigger checklist
//! (D-06) -- is unit-tested end to end with no terminal involved.
//!
//! `#![allow(dead_code)]`: this plan's own scope boundary stops at the pure
//! step machine and `App::open_wizard`/`wizard_mut` -- `main.rs`'s
//! `run_loop` does not yet dispatch any `Action::Wizard*` into it (that
//! wiring, plus rendering, is 10-04's scope). Until 10-04 lands, `cargo
//! clippy`'s default (non-test) target sees this entire module as
//! unreachable from `fn main`, even though every mutator is exercised end
//! to end by this file's own `#[cfg(test)]` module below. Same "read only
//! by X, not yet by production code" shape as `event.rs`'s `KeyHint::code`/
//! `ctrl` fields.
#![allow(dead_code)]

use shared::wizard::{self, FieldError, HandlerChoice, Step};
use shared::{
    AgentConfig, CreateWorkflowRequest, IntentCollisionReport, ParameterDescriptor, ParameterType,
    WorkflowWriteMode,
};

/// The full, in-progress answer set (mirrors
/// `orchestrator-cli::create_wizard::WizardAnswers`) -- only converted into
/// a `CreateWorkflowRequest` via `WizardState::to_request`.
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

/// Step 7's internal three-field sub-form (name/type/required), ported
/// from the CLI's `prompt_parameters` loop shape (D-02): "Add a parameter?"
/// gate, then Name, then Type, then Required, then back to the gate. Not a
/// `shared::wizard::Step` variant -- this is a TUI-only representation
/// detail of how ONE step's buffer is reused across a repeatable sub-form,
/// exactly as the CLI achieves the same UX with a loop instead of a state
/// enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamSubStep {
    AskAdd,
    Name,
    Type,
    Required,
}

/// The `Focus::Wizard` step machine: current step, the active field's
/// input buffer, the answers accumulated so far, and every piece of
/// per-step scratch state needed to reconstruct the CLI's proven sequence
/// (D-02) without a terminal. Every mutator is pure and never panics on
/// any buffer state.
#[derive(Debug, Clone)]
pub struct WizardState {
    step: Step,
    buffer: String,
    field_error: Option<FieldError>,
    answers: WizardAnswers,
    selected_triggers: Vec<String>,
    param_substep: ParamSubStep,
    param_draft_name: String,
    param_draft_type: String,
    /// Set by 10-04's async collision-check call.
    collision_check_in_flight: bool,
    /// The most recent collision-check reply (Phase 10, plan 10-04,
    /// D-03/D-04). `Some` with `colliding_workflow_id: Some(..)` gates
    /// `advance()` behind the save-anyway buffer answer; `Some` with only
    /// `detail` (a degraded check) or `None` (a clean check) has already
    /// been resolved by `apply_collision_report` -- see that method's own
    /// doc comment for the full state machine.
    collision_report: Option<IntentCollisionReport>,
    /// Set once the daemon confirms the write succeeded (Phase 10, plan
    /// 10-04): the absolute path of the written `.md`. `Some` short-circuits
    /// every other rendering -- the wizard is done, showing only the
    /// success screen until the caller closes it.
    success_path: Option<String>,
    /// Set when the daemon rejects the write itself (e.g. a duplicate id),
    /// distinct from `field_error` (client-side pre-validation): rendered
    /// verbatim as `[ERROR] {message}` per UI-SPEC's server-side-rejection
    /// row, which does NOT use the generic `[ERROR] {field} {reason}.`
    /// shape `FieldError::line()` produces. Setting this also resets `step`
    /// back to `Step::Id` so only the offending field re-prompts (never a
    /// full restart) -- see `set_server_error`.
    server_error: Option<String>,
}

impl Default for WizardState {
    fn default() -> Self {
        Self::new()
    }
}

impl WizardState {
    /// A fresh wizard: the workflow-id step, an empty buffer, no field
    /// error, and every answer at its zero value.
    pub fn new() -> Self {
        WizardState {
            step: Step::Id,
            buffer: String::new(),
            field_error: None,
            answers: WizardAnswers::default(),
            selected_triggers: Vec::new(),
            param_substep: ParamSubStep::AskAdd,
            param_draft_name: String::new(),
            param_draft_type: String::new(),
            collision_check_in_flight: false,
            collision_report: None,
            success_path: None,
            server_error: None,
        }
    }

    /// The step currently being answered.
    pub fn step(&self) -> Step {
        self.step
    }

    /// The active field's input buffer.
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    /// The active field's rejection, if the last `advance()` failed
    /// validation.
    pub fn field_error(&self) -> Option<&FieldError> {
        self.field_error.as_ref()
    }

    /// The answers accumulated so far.
    pub fn answers(&self) -> &WizardAnswers {
        &self.answers
    }

    /// The trigger checklist's current selection (D-06), in selection
    /// order.
    pub fn selected_triggers(&self) -> &[String] {
        &self.selected_triggers
    }

    /// Whether 10-04's async collision check is currently in flight.
    pub fn collision_check_in_flight(&self) -> bool {
        self.collision_check_in_flight
    }

    /// Sets the collision-check-in-flight flag (10-04 consumes this).
    pub fn set_collision_check_in_flight(&mut self, in_flight: bool) {
        self.collision_check_in_flight = in_flight;
    }

    /// The most recent collision-check reply, if any -- rendered as the
    /// warning banner (a real collision), the degrade notice (`detail`
    /// only), or nothing (a clean check already resolved to `None`).
    pub fn collision_report(&self) -> Option<&IntentCollisionReport> {
        self.collision_report.as_ref()
    }

    /// Records the async collision-check reply (Phase 10, plan 10-04,
    /// D-03/D-04), called once by the caller after the checker's `.await`
    /// resolves. A report naming a real collision STAYS on
    /// `Step::CollisionCheck`, gating further progress behind the
    /// save-anyway buffer answer -- `advance()` resolves it (see
    /// `advance_collision_check`). A report with no collision (a clean
    /// check, or a degraded one carrying only `detail`) advances straight to
    /// `Step::Review` -- UI-SPEC's own non-blocking design default for a
    /// degraded check, and there being nothing left to decide for a clean
    /// one. A degrade's `detail` is retained (not cleared) so the caller can
    /// still render it; a clean report is discarded outright.
    pub fn apply_collision_report(&mut self, report: IntentCollisionReport) {
        let is_real_collision = report.colliding_workflow_id.is_some();
        let has_detail = report.detail.is_some();
        if is_real_collision {
            self.collision_report = Some(report);
        } else if has_detail {
            self.collision_report = Some(report);
            self.commit_and_move_to(Step::Review);
        } else {
            self.collision_report = None;
            self.commit_and_move_to(Step::Review);
        }
    }

    /// The absolute path of the just-written `.md`, once the daemon has
    /// confirmed the create succeeded -- `Some` means the wizard is done and
    /// showing only the terminal success screen.
    pub fn success_path(&self) -> Option<&str> {
        self.success_path.as_deref()
    }

    /// Records a successful create (Phase 10, plan 10-04): the wizard's
    /// terminal success state.
    pub fn set_success(&mut self, workflow_path: String) {
        self.success_path = Some(workflow_path);
    }

    /// The daemon's own rejection text (e.g. a duplicate id), if the most
    /// recent write attempt failed server-side -- distinct from
    /// `field_error`'s client-side pre-validation shape.
    pub fn server_error(&self) -> Option<&str> {
        self.server_error.as_deref()
    }

    /// Records a server-side write rejection (Phase 10, plan 10-04) and
    /// returns to `Step::Id` so only the offending field re-prompts --
    /// mirrors the CLI's own "re-prompt Step 1 only, never a full restart"
    /// discipline.
    pub fn set_server_error(&mut self, message: String) {
        self.server_error = Some(message);
        self.step = Step::Id;
        self.buffer.clear();
        self.field_error = None;
    }

    /// Appends a character to the active field's buffer, in order. Never
    /// panics -- every printable character (including `q`/`?`, which the
    /// wizard's key-mapping gate in `event.rs` routes here rather than to a
    /// global command, T-10-12) is accepted verbatim.
    pub fn push_char(&mut self, c: char) {
        self.buffer.push(c);
    }

    /// Removes the last character from the active field's buffer. A
    /// handled no-op on an empty buffer -- never a panic or underflow.
    pub fn backspace(&mut self) {
        self.buffer.pop();
    }

    /// Toggles one of the fixed two `shared::wizard::TRIGGER_CHOICES`
    /// entries by index (D-06). An out-of-range index is a handled no-op.
    pub fn toggle_trigger(&mut self, index: usize) {
        let Some(choice) = wizard::TRIGGER_CHOICES.get(index) else {
            return;
        };
        if let Some(pos) = self.selected_triggers.iter().position(|t| t == choice) {
            self.selected_triggers.remove(pos);
        } else {
            self.selected_triggers.push((*choice).to_string());
        }
    }

    /// Step 6 (D-05): choosing a handler type is itself a complete, valid
    /// answer -- there is nothing left to validate, so selecting
    /// immediately records the choice and advances, branching the
    /// remaining sequence: `ScriptAction`/`MarkdownAction` skip straight to
    /// `Parameters`; `Agent` continues into the three agent-only sub-steps
    /// (`AgentFiles` -> `AgentTimeout` -> `AgentBudget` -> `Parameters`).
    pub fn select_handler(&mut self, choice: HandlerChoice) {
        self.answers.handler = Some(choice);
        self.field_error = None;
        self.buffer.clear();
        self.step = if choice == HandlerChoice::Agent {
            Step::AgentFiles
        } else {
            Step::Parameters
        };
    }

    /// Validates the active field's buffer against `shared::wizard` and,
    /// on success, commits it to `answers` and moves to the next step in
    /// the sequence (D-02's proven order). On rejection, the step index is
    /// UNCHANGED and `field_error` carries the exact reason
    /// `shared::wizard` produced -- the caller re-prompts the same field,
    /// exactly like the CLI's re-prompt-on-error discipline.
    ///
    /// `Step::HandlerType` is handled entirely by `select_handler`, not
    /// here -- calling `advance()` while on that step is a no-op.
    pub fn advance(&mut self) {
        match self.step {
            Step::Id => self.advance_id(),
            Step::DisplayName => self.advance_display_name(),
            Step::Description => self.advance_description(),
            Step::Triggers => self.advance_triggers(),
            Step::Intent => self.advance_intent(),
            Step::HandlerType => {}
            Step::AgentFiles => self.advance_agent_files(),
            Step::AgentTimeout => self.advance_agent_timeout(),
            Step::AgentBudget => self.advance_agent_budget(),
            Step::Parameters => self.advance_parameters(),
            Step::CollisionCheck => self.advance_collision_check(),
            Step::Review => {}
        }
    }

    fn advance_id(&mut self) {
        let id = self.buffer.trim().to_string();
        match wizard::validate_id(&id) {
            Ok(()) => {
                self.answers.id = id;
                self.server_error = None;
                self.commit_and_move_to(Step::DisplayName);
            }
            Err(err) => self.field_error = Some(err),
        }
    }

    fn advance_display_name(&mut self) {
        let trimmed = self.buffer.trim();
        if trimmed.is_empty() {
            // Blank accepts the title-cased default derived from the id
            // (`None`, resolved at request-build time, mirroring the CLI).
            self.answers.display_name = None;
            self.commit_and_move_to(Step::Description);
            return;
        }
        match wizard::validate_display_name(trimmed) {
            Ok(()) => {
                self.answers.display_name = Some(trimmed.to_string());
                self.commit_and_move_to(Step::Description);
            }
            Err(err) => self.field_error = Some(err),
        }
    }

    fn advance_description(&mut self) {
        self.answers.description = self.buffer.trim().to_string();
        self.commit_and_move_to(Step::Triggers);
    }

    fn advance_triggers(&mut self) {
        match wizard::validate_triggers(&self.selected_triggers) {
            Ok(()) => {
                self.answers.triggers = self.selected_triggers.clone();
                self.commit_and_move_to(Step::Intent);
            }
            Err(err) => self.field_error = Some(err),
        }
    }

    fn advance_intent(&mut self) {
        let intent = self.buffer.trim().to_string();
        match wizard::validate_intent(&intent) {
            Ok(()) => {
                self.answers.intent = intent;
                self.commit_and_move_to(Step::HandlerType);
            }
            Err(err) => self.field_error = Some(err),
        }
    }

    fn advance_agent_files(&mut self) {
        let trimmed = self.buffer.trim().to_string();
        if trimmed.is_empty() {
            // A blank entry finishes the repeatable list (mirrors the
            // CLI's "blank line to finish").
            self.commit_and_move_to(Step::AgentTimeout);
            return;
        }
        match wizard::validate_agent_file(&trimmed) {
            Ok(()) => {
                self.answers.agent_files.push(trimmed);
                self.field_error = None;
                self.buffer.clear();
                // Stays on `AgentFiles` -- a valid entry is added and the
                // list keeps accepting more, exactly like the CLI's loop.
            }
            Err(err) => self.field_error = Some(err),
        }
    }

    fn advance_agent_timeout(&mut self) {
        let trimmed = self.buffer.trim();
        if trimmed.is_empty() {
            // Never substitutes a number -- the daemon's own default
            // applies at runtime (mirrors the CLI exactly).
            self.answers.timeout_secs = None;
            self.commit_and_move_to(Step::AgentBudget);
            return;
        }
        match trimmed.parse::<u64>() {
            Ok(value) => {
                self.answers.timeout_secs = Some(value);
                self.commit_and_move_to(Step::AgentBudget);
            }
            Err(_) => {
                self.field_error = Some(FieldError {
                    field: "timeout",
                    reason: "must be a whole number of seconds".to_string(),
                });
            }
        }
    }

    fn advance_agent_budget(&mut self) {
        let trimmed = self.buffer.trim();
        if trimmed.is_empty() {
            self.answers.max_budget_usd = None;
            self.commit_and_move_to(Step::Parameters);
            return;
        }
        match trimmed.parse::<f64>() {
            Ok(value) => {
                self.answers.max_budget_usd = Some(value);
                self.commit_and_move_to(Step::Parameters);
            }
            Err(_) => {
                self.field_error = Some(FieldError {
                    field: "budget",
                    reason: "must be a number".to_string(),
                });
            }
        }
    }

    /// Step 7's repeatable add-a-parameter loop (D-02, ported from the
    /// CLI's `prompt_parameters`): `AskAdd` -> (if yes) `Name` -> `Type` ->
    /// `Required` -> back to `AskAdd`; answering anything other than
    /// `y`/`Y` at `AskAdd` ends the loop and moves to `CollisionCheck`. A
    /// duplicate parameter name is rejected inline with
    /// `shared::wizard::duplicate_parameter_message` and does NOT append;
    /// distinct names accumulate in entry order.
    fn advance_parameters(&mut self) {
        match self.param_substep {
            ParamSubStep::AskAdd => {
                if matches!(self.buffer.trim(), "y" | "Y") {
                    self.param_substep = ParamSubStep::Name;
                    self.field_error = None;
                    self.buffer.clear();
                } else {
                    self.field_error = None;
                    self.buffer.clear();
                    self.step = Step::CollisionCheck;
                }
            }
            ParamSubStep::Name => {
                self.param_draft_name = self.buffer.trim().to_string();
                self.param_substep = ParamSubStep::Type;
                self.field_error = None;
                self.buffer.clear();
            }
            ParamSubStep::Type => {
                self.param_draft_type = self.buffer.trim().to_string();
                self.param_substep = ParamSubStep::Required;
                self.field_error = None;
                self.buffer.clear();
            }
            ParamSubStep::Required => {
                let required = matches!(self.buffer.trim(), "y" | "Y");
                let name = std::mem::take(&mut self.param_draft_name);
                let type_ = match self.param_draft_type.as_str() {
                    "int" => ParameterType::Int,
                    "bool" => ParameterType::Bool,
                    _ => ParameterType::String,
                };
                self.param_draft_type.clear();
                self.buffer.clear();
                self.param_substep = ParamSubStep::AskAdd;

                if self.answers.parameters.iter().any(|p| p.name == name) {
                    self.field_error = Some(FieldError {
                        field: "",
                        reason: wizard::duplicate_parameter_message(&name),
                    });
                } else {
                    self.answers.parameters.push(ParameterDescriptor { name, type_, required });
                    self.field_error = None;
                }
            }
        }
    }

    /// Step 8's save-anyway gate (Phase 10, plan 10-04, D-03/D-04): resolves
    /// a REAL collision (`apply_collision_report` left one pending) against
    /// the buffer's `y`/`Y` answer -- confirming clears the report and moves
    /// to `Step::Review`; declining clears the report and returns to
    /// `Step::Intent` so the wording can be revised (fail-closed, never a
    /// full restart). Called with no real collision pending (a clean/
    /// degraded report already auto-advanced via `apply_collision_report`,
    /// or the check hasn't run yet) is a defensive no-op -- there is nothing
    /// here to resolve.
    fn advance_collision_check(&mut self) {
        let Some(report) = self.collision_report.clone() else {
            return;
        };
        if report.colliding_workflow_id.is_none() {
            return;
        }
        if matches!(self.buffer.trim(), "y" | "Y") {
            self.collision_report = None;
            self.commit_and_move_to(Step::Review);
        } else {
            self.collision_report = None;
            self.commit_and_move_to(Step::Intent);
        }
    }

    /// Clears the field error and buffer, then moves to `next` -- the
    /// shared tail every successful `advance_*` step takes.
    fn commit_and_move_to(&mut self, next: Step) {
        self.field_error = None;
        self.buffer.clear();
        self.step = next;
    }

    /// Returns to the previous step in the SAME conditional sequence the
    /// forward path took: from `Parameters`, an Agent run goes back to
    /// `AgentBudget`; a non-Agent run goes back to `HandlerType` -- the
    /// mirror image of `select_handler`'s branch. Going back from the
    /// first step (`Id`) is a no-op. Always clears the buffer and field
    /// error, and resets the parameter sub-form to its `AskAdd` gate.
    pub fn back(&mut self) {
        self.step = match self.step {
            Step::Id => Step::Id,
            Step::DisplayName => Step::Id,
            Step::Description => Step::DisplayName,
            Step::Triggers => Step::Description,
            Step::Intent => Step::Triggers,
            Step::HandlerType => Step::Intent,
            Step::AgentFiles => Step::HandlerType,
            Step::AgentTimeout => Step::AgentFiles,
            Step::AgentBudget => Step::AgentTimeout,
            Step::Parameters => {
                if self.answers.handler == Some(HandlerChoice::Agent) {
                    Step::AgentBudget
                } else {
                    Step::HandlerType
                }
            }
            Step::CollisionCheck => Step::Parameters,
            Step::Review => Step::CollisionCheck,
        };
        self.field_error = None;
        self.buffer.clear();
        self.param_substep = ParamSubStep::AskAdd;
    }

    /// Builds the `shared::CreateWorkflowRequest` the answers describe
    /// (D-05's three write shapes), mirroring
    /// `create_wizard::derive_write_shape`/`build_agent_config` exactly so
    /// the two surfaces cannot diverge on how a handler choice becomes a
    /// request. Never called before `Review` in the real flow, but pure
    /// and total for any answer state so it stays trivially testable at
    /// every step.
    pub fn to_request(&self) -> CreateWorkflowRequest {
        let name = self
            .answers
            .display_name
            .clone()
            .unwrap_or_else(|| wizard::derive_display_name(&self.answers.id));

        let (description, script) = match self.answers.handler {
            Some(HandlerChoice::ScriptAction) => (String::new(), Some(self.answers.description.clone())),
            _ => (self.answers.description.clone(), None),
        };

        let agent = if self.answers.handler == Some(HandlerChoice::Agent) {
            Some(AgentConfig {
                files: self.answers.agent_files.clone(),
                timeout_secs: self.answers.timeout_secs,
                max_budget_usd: self.answers.max_budget_usd,
            })
        } else {
            None
        };

        CreateWorkflowRequest {
            id: self.answers.id.clone(),
            name,
            description,
            script,
            parameters: self.answers.parameters.clone(),
            mode: WorkflowWriteMode::Create,
            agent,
            intent: Some(self.answers.intent.clone()),
            triggers: self.answers.triggers.clone(),
        }
    }
}

/// Test-only construction helpers (Phase 10, plan 10-04, Task 2): let
/// render tests -- both this file's own `#[cfg(test)] mod tests` below and
/// `tests/wizard_render_integration.rs`'s external `#[path]` inclusion of
/// this file -- build an arbitrary mid-flow `WizardState` directly, without
/// driving the real step-by-step `advance()` sequence for every field.
/// `#[cfg(test)]` on the `impl` block (not just individual methods) keeps
/// every one of these entirely out of the shipped, non-test binary.
#[cfg(test)]
impl WizardState {
    /// Jumps directly to `step` with the given `answers`, bypassing the
    /// real sequence -- e.g. to render the Review screen with a specific
    /// parameter list without re-typing every prior field.
    pub fn test_at_step(step: Step, answers: WizardAnswers) -> Self {
        let mut state = Self::new();
        state.step = step;
        state.answers = answers;
        state
    }

    /// Overwrites the active field's input buffer directly.
    pub fn test_set_buffer(&mut self, buffer: impl Into<String>) {
        self.buffer = buffer.into();
    }

    /// Overwrites the collision report directly, WITHOUT running
    /// `apply_collision_report`'s transition logic (no auto-advance) -- lets
    /// render tests pin an arbitrary `(step, collision_report)` pairing that
    /// the real state machine would only ever pass through transiently.
    pub fn test_set_collision_report(&mut self, report: Option<IntentCollisionReport>) {
        self.collision_report = report;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_str(state: &mut WizardState, s: &str) {
        for c in s.chars() {
            state.push_char(c);
        }
    }

    #[test]
    fn a_fresh_wizard_starts_on_the_id_step_with_an_empty_buffer_and_no_error() {
        let state = WizardState::new();
        assert_eq!(state.step(), Step::Id);
        assert_eq!(state.buffer(), "");
        assert!(state.field_error().is_none());
    }

    #[test]
    fn typing_appends_in_order_and_backspace_removes_the_last_character() {
        let mut state = WizardState::new();
        type_str(&mut state, "abc");
        assert_eq!(state.buffer(), "abc");
        state.backspace();
        assert_eq!(state.buffer(), "ab");
    }

    #[test]
    fn backspace_on_an_empty_buffer_is_a_handled_no_op() {
        let mut state = WizardState::new();
        state.backspace();
        assert_eq!(state.buffer(), "");
    }

    #[test]
    fn typing_q_and_question_mark_enters_the_buffer_like_any_other_character() {
        // T-10-12: the wizard's text-entry path must never special-case
        // these -- the global-command gate lives in event.rs's map_key,
        // not here.
        let mut state = WizardState::new();
        type_str(&mut state, "q?");
        assert_eq!(state.buffer(), "q?");
    }

    #[test]
    fn advancing_with_a_valid_id_moves_to_display_name_and_clears_the_error() {
        let mut state = WizardState::new();
        type_str(&mut state, "my-workflow");
        state.advance();
        assert_eq!(state.step(), Step::DisplayName);
        assert!(state.field_error().is_none());
        assert_eq!(state.answers().id, "my-workflow");
        assert_eq!(state.buffer(), "");
    }

    #[test]
    fn advancing_with_an_invalid_id_does_not_advance_and_carries_the_exact_reason() {
        let mut state = WizardState::new();
        type_str(&mut state, "MyWorkflow");
        state.advance();
        assert_eq!(state.step(), Step::Id, "step must not change on a rejected id");
        let err = state.field_error().expect("an invalid id must set a field error");
        let expected = wizard::validate_id("MyWorkflow").unwrap_err();
        assert_eq!(err.reason, expected.reason);
    }

    #[test]
    fn advancing_past_display_name_with_an_empty_buffer_accepts_the_title_cased_default() {
        let mut state = WizardState::new();
        type_str(&mut state, "my_workflow");
        state.advance();
        assert_eq!(state.step(), Step::DisplayName);
        // Blank -- accepts the default, resolved lazily at request-build
        // time via `to_request`, exactly like the CLI.
        state.advance();
        assert_eq!(state.step(), Step::Description);
        assert_eq!(state.answers().display_name, None);
        let req = state.to_request();
        assert_eq!(req.name, "My Workflow");
    }

    fn advance_through_id_and_display_name(state: &mut WizardState, id: &str) {
        type_str(state, id);
        state.advance();
        state.advance(); // blank display name -> default
    }

    #[test]
    fn trigger_toggling_and_the_zero_selection_gate() {
        let mut state = WizardState::new();
        advance_through_id_and_display_name(&mut state, "my-workflow");
        state.advance(); // blank description
        assert_eq!(state.step(), Step::Triggers);

        state.advance();
        assert_eq!(state.step(), Step::Triggers, "zero selections must not advance");
        assert_eq!(
            state.field_error().unwrap().reason,
            "Select at least one trigger type.",
            "must carry the documented line"
        );

        state.toggle_trigger(0);
        assert_eq!(state.selected_triggers(), &["cli".to_string()]);
        state.toggle_trigger(0);
        assert!(state.selected_triggers().is_empty(), "toggling twice must deselect");

        state.toggle_trigger(1);
        assert_eq!(state.selected_triggers(), &["voice".to_string()]);
        state.advance();
        assert_eq!(state.step(), Step::Intent, "one selection must advance");
        assert_eq!(state.answers().triggers, vec!["voice".to_string()]);
    }

    fn advance_to_handler_type(state: &mut WizardState, id: &str) {
        advance_through_id_and_display_name(state, id);
        state.advance(); // blank description
        state.toggle_trigger(0);
        state.advance(); // triggers
        type_str(state, "brew a fresh pot of coffee");
        state.advance(); // intent
        assert_eq!(state.step(), Step::HandlerType);
    }

    #[test]
    fn non_agent_handlers_skip_straight_to_parameters() {
        for choice in [HandlerChoice::ScriptAction, HandlerChoice::MarkdownAction] {
            let mut state = WizardState::new();
            advance_to_handler_type(&mut state, "brew-coffee");
            state.select_handler(choice);
            assert_eq!(state.step(), Step::Parameters, "{choice:?} must skip the agent sub-steps");
        }
    }

    #[test]
    fn agent_handler_walks_all_three_sub_steps_in_order() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::Agent);
        assert_eq!(state.step(), Step::AgentFiles);

        // Blank finishes the repeatable file list.
        state.advance();
        assert_eq!(state.step(), Step::AgentTimeout);

        state.advance(); // blank timeout -> None
        assert_eq!(state.step(), Step::AgentBudget);
        assert_eq!(state.answers().timeout_secs, None);

        state.advance(); // blank budget -> None
        assert_eq!(state.step(), Step::Parameters);
        assert_eq!(state.answers().max_budget_usd, None);
    }

    #[test]
    fn agent_files_accumulate_and_reject_invalid_entries_without_advancing() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::Agent);

        type_str(&mut state, "recipes/coffee.md");
        state.advance();
        assert_eq!(state.step(), Step::AgentFiles, "a valid entry must stay on this step");
        assert_eq!(state.answers().agent_files, vec!["recipes/coffee.md".to_string()]);

        // A NUL byte is rejected by shared::wizard::validate_agent_file.
        state.push_char('\0');
        state.advance();
        assert_eq!(state.step(), Step::AgentFiles);
        assert!(state.field_error().is_some());
    }

    #[test]
    fn agent_optional_fields_parse_a_given_value() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::Agent);
        state.advance(); // blank files -> AgentTimeout

        type_str(&mut state, "30");
        state.advance();
        assert_eq!(state.answers().timeout_secs, Some(30));
        assert_eq!(state.step(), Step::AgentBudget);

        type_str(&mut state, "2.5");
        state.advance();
        assert_eq!(state.answers().max_budget_usd, Some(2.5));
        assert_eq!(state.step(), Step::Parameters);
    }

    #[test]
    fn agent_optional_fields_reject_unparsable_values_without_advancing() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::Agent);
        state.advance(); // blank files -> AgentTimeout

        type_str(&mut state, "not-a-number");
        state.advance();
        assert_eq!(state.step(), Step::AgentTimeout, "an unparsable timeout must not advance");
        assert!(state.field_error().is_some());
    }

    fn advance_to_parameters(state: &mut WizardState, id: &str) {
        advance_to_handler_type(state, id);
        state.select_handler(HandlerChoice::MarkdownAction);
        assert_eq!(state.step(), Step::Parameters);
    }

    #[test]
    fn duplicate_parameter_names_are_rejected_and_distinct_names_accumulate_in_order() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");

        // First parameter: "count".
        type_str(&mut state, "y");
        state.advance();
        type_str(&mut state, "count");
        state.advance();
        type_str(&mut state, "int");
        state.advance();
        type_str(&mut state, "y");
        state.advance();
        assert_eq!(state.answers().parameters.len(), 1);
        assert_eq!(state.answers().parameters[0].name, "count");
        assert_eq!(state.answers().parameters[0].type_, ParameterType::Int);
        assert!(state.answers().parameters[0].required);

        // Second parameter: duplicate "count" -- rejected, not appended.
        type_str(&mut state, "y");
        state.advance();
        type_str(&mut state, "count");
        state.advance();
        type_str(&mut state, "string");
        state.advance();
        type_str(&mut state, "n");
        state.advance();
        assert_eq!(state.answers().parameters.len(), 1, "a duplicate name must not append");
        assert_eq!(
            state.field_error().unwrap().reason,
            "Parameter 'count' already added.",
            "must carry the documented duplicate message"
        );

        // Third parameter: distinct "label" -- accumulates.
        type_str(&mut state, "y");
        state.advance();
        type_str(&mut state, "label");
        state.advance();
        type_str(&mut state, "string");
        state.advance();
        type_str(&mut state, "n");
        state.advance();
        assert_eq!(state.answers().parameters.len(), 2);
        assert_eq!(state.answers().parameters[1].name, "label");

        // Ending the loop moves on to the collision check.
        type_str(&mut state, "n");
        state.advance();
        assert_eq!(state.step(), Step::CollisionCheck);
    }

    fn no_collision_report() -> IntentCollisionReport {
        IntentCollisionReport {
            colliding_workflow_id: None,
            colliding_intent: None,
            similarity_score: None,
            detail: None,
        }
    }

    fn degraded_report(detail: &str) -> IntentCollisionReport {
        IntentCollisionReport {
            colliding_workflow_id: None,
            colliding_intent: None,
            similarity_score: None,
            detail: Some(detail.to_string()),
        }
    }

    fn colliding_report(id: &str, intent: &str, score: f32) -> IntentCollisionReport {
        IntentCollisionReport {
            colliding_workflow_id: Some(id.to_string()),
            colliding_intent: Some(intent.to_string()),
            similarity_score: Some(score),
            detail: None,
        }
    }

    #[test]
    fn a_clean_collision_report_auto_advances_to_review() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");
        type_str(&mut state, "n");
        state.advance();
        assert_eq!(state.step(), Step::CollisionCheck);

        state.apply_collision_report(no_collision_report());

        assert_eq!(state.step(), Step::Review);
        assert!(state.collision_report().is_none(), "a clean report must be discarded");
    }

    #[test]
    fn a_degraded_collision_report_auto_advances_to_review_but_is_retained() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");
        type_str(&mut state, "n");
        state.advance();

        state.apply_collision_report(degraded_report("ollama unreachable"));

        assert_eq!(state.step(), Step::Review, "a degraded check must proceed to review without a keypress");
        assert_eq!(
            state.collision_report().and_then(|r| r.detail.as_deref()),
            Some("ollama unreachable"),
            "the degrade detail must be retained so the caller can still render it"
        );
    }

    #[test]
    fn a_real_collision_stays_on_the_step_until_the_save_anyway_buffer_is_answered() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");
        type_str(&mut state, "n");
        state.advance();

        state.apply_collision_report(colliding_report("countdown", "count down from a number", 0.81));

        assert_eq!(state.step(), Step::CollisionCheck, "a real collision must not auto-advance");
        assert!(state.collision_report().is_some());
    }

    #[test]
    fn confirming_the_save_anyway_prompt_clears_the_report_and_reaches_review() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");
        type_str(&mut state, "n");
        state.advance();
        state.apply_collision_report(colliding_report("countdown", "count down from a number", 0.81));

        type_str(&mut state, "y");
        state.advance();

        assert_eq!(state.step(), Step::Review);
        assert!(state.collision_report().is_none(), "an overridden collision must be cleared, not re-shown at review");
    }

    #[test]
    fn declining_the_save_anyway_prompt_returns_to_intent_and_clears_the_report() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");
        type_str(&mut state, "n");
        state.advance();
        state.apply_collision_report(colliding_report("countdown", "count down from a number", 0.81));

        type_str(&mut state, "n");
        state.advance();

        assert_eq!(state.step(), Step::Intent, "declining must return to the intent step to revise the wording");
        assert!(state.collision_report().is_none());
    }

    #[test]
    fn a_blank_save_anyway_answer_declines_by_default() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");
        type_str(&mut state, "n");
        state.advance();
        state.apply_collision_report(colliding_report("countdown", "count down from a number", 0.81));

        state.advance(); // blank buffer -- default is decline (D-04)

        assert_eq!(state.step(), Step::Intent, "a blank answer must decline (fail-closed default)");
    }

    #[test]
    fn success_path_and_server_error_are_independent_terminal_fields() {
        let mut state = WizardState::new();
        assert!(state.success_path().is_none());
        assert!(state.server_error().is_none());

        state.set_success("/workflows/brew-coffee.md".to_string());
        assert_eq!(state.success_path(), Some("/workflows/brew-coffee.md"));
    }

    #[test]
    fn server_error_resets_to_the_id_step_and_clears_the_buffer_and_field_error() {
        let mut state = WizardState::new();
        advance_through_id_and_display_name(&mut state, "brew-coffee");
        type_str(&mut state, "some-typed-text");

        state.set_server_error("workflow 'brew-coffee' already exists".to_string());

        assert_eq!(state.step(), Step::Id);
        assert_eq!(state.buffer(), "");
        assert!(state.field_error().is_none());
        assert_eq!(state.server_error(), Some("workflow 'brew-coffee' already exists"));
    }

    #[test]
    fn advancing_past_the_id_step_again_clears_a_prior_server_error() {
        let mut state = WizardState::new();
        state.set_server_error("workflow 'brew-coffee' already exists".to_string());
        assert_eq!(state.step(), Step::Id);

        type_str(&mut state, "brew-coffee-2");
        state.advance();

        assert!(state.server_error().is_none(), "a fresh, valid id must clear the stale server error");
    }

    #[test]
    fn going_back_from_parameters_returns_to_budget_for_an_agent_run() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::Agent);
        state.advance(); // files -> timeout
        state.advance(); // timeout -> budget
        state.advance(); // budget -> parameters
        assert_eq!(state.step(), Step::Parameters);

        state.back();
        assert_eq!(state.step(), Step::AgentBudget, "an Agent run must go back to budget");
    }

    #[test]
    fn going_back_from_parameters_returns_to_handler_type_for_a_non_agent_run() {
        let mut state = WizardState::new();
        advance_to_parameters(&mut state, "brew-coffee");

        state.back();
        assert_eq!(state.step(), Step::HandlerType, "a non-Agent run must go back to handler type");
    }

    #[test]
    fn going_back_walks_the_same_conditional_sequence_in_reverse() {
        let mut state = WizardState::new();
        assert_eq!(state.step(), Step::Id);
        state.back();
        assert_eq!(state.step(), Step::Id, "going back from the first step is a no-op");

        advance_through_id_and_display_name(&mut state, "brew-coffee");
        assert_eq!(state.step(), Step::Description);
        state.back();
        assert_eq!(state.step(), Step::DisplayName);
        state.back();
        assert_eq!(state.step(), Step::Id);
    }

    #[test]
    fn to_request_matches_the_answers_for_a_script_backed_run() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::ScriptAction);
        // (skip parameters/collision-check -- to_request is total at any
        // answer state)

        let req = state.to_request();
        assert_eq!(req.id, "brew-coffee");
        assert_eq!(req.intent, Some("brew a fresh pot of coffee".to_string()));
        assert_eq!(req.triggers, vec!["cli".to_string()]);
        assert_eq!(req.script, Some(String::new()));
        assert!(req.agent.is_none());
    }

    #[test]
    fn to_request_matches_the_answers_for_a_markdown_body_run() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::MarkdownAction);

        let req = state.to_request();
        assert!(req.script.is_none());
        assert!(req.agent.is_none());
    }

    #[test]
    fn to_request_matches_the_answers_for_an_agent_run() {
        let mut state = WizardState::new();
        advance_to_handler_type(&mut state, "brew-coffee");
        state.select_handler(HandlerChoice::Agent);
        type_str(&mut state, "recipes/coffee.md");
        state.advance();
        state.advance(); // blank -> finish files, move to timeout
        type_str(&mut state, "30");
        state.advance();
        type_str(&mut state, "2.5");
        state.advance();

        let req = state.to_request();
        assert!(req.script.is_none());
        let agent = req.agent.expect("an Agent run must populate agent");
        assert_eq!(agent.files, vec!["recipes/coffee.md".to_string()]);
        assert_eq!(agent.timeout_secs, Some(30));
        assert_eq!(agent.max_budget_usd, Some(2.5));
    }
}
