//! Surface-agnostic workflow-authoring contract (Phase 10, plan 10-03): the
//! single-sourced step order, prompt/menu wording, field validators, and
//! cap mirrors both `orchestrator-cli::create_wizard` (the CLI's guided Q&A
//! flow, 10-01/10-02) and `orchestrator-tui::wizard` (the TUI's
//! `Focus::Wizard` step machine, this plan) build their own guided flow
//! from. Promoted out of `orchestrator-cli::create_wizard`, where 10-01/
//! 10-02 first proved this exact step sequence, wording, and validation
//! end-to-end against a real daemon and real Ollama -- this module is a
//! PURE move of that proven contract, not a rewrite: no wording change, no
//! validation change, no step reordering (see 10-03-SUMMARY.md for the
//! acceptance evidence).
//!
//! Charter note: `shared`'s crate doc (`client.rs` lines 1-6) scopes this
//! crate to "only the serializable wire contract." This module is NOT
//! wire-shaped -- nothing here crosses the WS boundary and nothing derives
//! `Serialize`/`Deserialize`. It lives here anyway because UI-SPEC's
//! Copywriting Contract requires byte-identical wording across both client
//! surfaces, and the only alternative -- a per-crate copy in each of
//! `orchestrator-cli` and `orchestrator-tui`, layered on top of the
//! already-duplicated copy of the daemon's own reason strings -- makes that
//! contract unenforceable by construction: a third hand-transcribed copy of
//! `validate_id`'s reason strings would be a drift bomb waiting to happen.
//! If this charter stretch is ever judged too far, relocating this module
//! (and its two consumers' imports) into a dedicated crate is a mechanical
//! move, not a redesign.
//!
//! `shared` must NOT depend on `orchestrator` -- that dependency direction
//! is deliberately one-way (see `shared`'s own crate doc), enforced here by
//! never adding `orchestrator` to this crate's `Cargo.toml`, not even as a
//! dev-dependency. The cap mirrors below therefore stay
//! duplicated-with-a-guard rather than imported: the REAL drift guard
//! against `apps/orchestrator/src/registry/writer.rs`'s live constants
//! remains `orchestrator-cli`'s own `cap_mirrors_match_the_real_writer_
//! constants` test (unchanged by this promotion, still importing the real
//! constants via that crate's existing `orchestrator` dev-dependency); this
//! module's own test below is a documentation-consistency check only.

// ---- Cap mirrors: see apps/orchestrator/src/registry/writer.rs --------

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

/// The literal line shown when the trigger checklist has zero selections
/// (D-06) -- a standalone line, NOT run through the `[ERROR] {field}
/// {reason}.` generic shape the other field validators use: UI-SPEC's Step
/// 4 table documents it exactly this way.
pub const TRIGGERS_EMPTY_LINE: &str = "Select at least one trigger type.";

/// Handler-type selection (D-05, Step 6) -- an exhaustive enum so the
/// branch deriving the request's script/agent/description triple cannot
/// silently miss a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerChoice {
    ScriptAction,
    MarkdownAction,
    Agent,
}

impl HandlerChoice {
    /// UI-SPEC Step 6's numbered menu labels, character for character, in
    /// the same order the `1`/`2`/`3` selection prompt expects.
    pub const MENU_LINES: &'static [&'static str] = &[
        "  1) Script-backed action — runs a script or command you provide",
        "  2) Markdown-body action — description text becomes the action body, no script",
        "  3) Agent (Claude-backed) — runs a Claude Code prompt",
    ];
}

/// The nine documented steps (UI-SPEC's Copywriting Contract) plus the
/// three agent-only sub-steps (6a/6b/6c) -- reachable only when
/// `HandlerChoice::Agent` was chosen at `HandlerType` (D-05).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Id,
    DisplayName,
    Description,
    Triggers,
    Intent,
    HandlerType,
    AgentFiles,
    AgentTimeout,
    AgentBudget,
    Parameters,
    CollisionCheck,
    Review,
}

impl Step {
    /// UI-SPEC's numbered step order (1-9, with 6a/6b/6c inline right after
    /// `HandlerType`, and Step 8's collision check between Parameters and
    /// Review) -- both surfaces walk this exact sequence, in this exact
    /// order.
    pub const ORDERED: &'static [Step] = &[
        Step::Id,
        Step::DisplayName,
        Step::Description,
        Step::Triggers,
        Step::Intent,
        Step::HandlerType,
        Step::AgentFiles,
        Step::AgentTimeout,
        Step::AgentBudget,
        Step::Parameters,
        Step::CollisionCheck,
        Step::Review,
    ];

    /// Whether this step is one of the three agent-only sub-steps
    /// (6a/6b/6c) -- reachable only when `HandlerChoice::Agent` was chosen
    /// at `HandlerType` (D-05).
    pub fn is_agent_only(self) -> bool {
        matches!(self, Step::AgentFiles | Step::AgentTimeout | Step::AgentBudget)
    }
}

/// A rejected answer's field name and the daemon's own reason wording
/// (mirroring `registry::writer`'s reason strings verbatim where
/// applicable). `line()` renders UI-SPEC's generic `[ERROR] {field}
/// {reason}.` shape; `TRIGGERS_EMPTY_LINE` above is the one documented
/// exception that is never wrapped this way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: &'static str,
    pub reason: String,
}

impl FieldError {
    pub fn line(&self) -> String {
        format!("[ERROR] {} {}.", self.field, self.reason)
    }
}

/// UI-SPEC's literal prompt/label text for `step` (the Copywriting
/// Contract's Step-order table). `id` is consulted only by
/// `Step::DisplayName`'s default-name interpolation; every other step
/// ignores it -- pass `""` when it is not yet known.
pub fn prompt_for(step: Step, id: &str) -> String {
    match step {
        Step::Id => "Workflow id (used as the .md filename):".to_string(),
        Step::DisplayName => format!("Display name [default: {}]:", derive_display_name(id)),
        Step::Description => "Description (workflow body text):".to_string(),
        Step::Triggers => "Select trigger types (at least one):".to_string(),
        Step::Intent => {
            "Intent phrase (a sentence describing when this should fire, used for voice/utterance routing):"
                .to_string()
        }
        Step::HandlerType => "Select handler type:".to_string(),
        Step::AgentFiles => "Reference files (repeatable, blank line to finish):".to_string(),
        Step::AgentTimeout => "Timeout in seconds [default: daemon default]:".to_string(),
        Step::AgentBudget => "Max budget in USD [default: daemon default]:".to_string(),
        Step::Parameters => "Add a parameter? [y/N]:".to_string(),
        Step::CollisionCheck => "Checking for similar existing workflows...".to_string(),
        Step::Review => "Review:".to_string(),
    }
}

/// UI-SPEC's numbered menu lines for the two checklist/menu steps
/// (`Triggers`, `HandlerType`); every other step has no menu and returns an
/// empty list.
pub fn menu_lines_for(step: Step) -> Vec<String> {
    match step {
        Step::Triggers => TRIGGER_CHOICES
            .iter()
            .enumerate()
            .map(|(index, choice)| format!("  {}) {}", index + 1, choice))
            .collect(),
        Step::HandlerType => HandlerChoice::MENU_LINES.iter().map(|line| (*line).to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Message shown when a duplicate parameter name is rejected inline (Step
/// 7, UI-SPEC's Copywriting Contract).
pub fn duplicate_parameter_message(name: &str) -> String {
    format!("Parameter '{name}' already added.")
}

/// Verbatim mirror of `registry::writer::validate_id` (writer.rs lines
/// 359-392): non-empty, at most `MAX_WORKFLOW_ID_LEN_MIRROR` characters,
/// every character in `[a-z0-9_-]`, first character alphanumeric. Reason
/// strings match the daemon's own character for character (UI-SPEC Step 1)
/// so `[ERROR] id {reason}.` renders identically whether caught here or
/// returned by the daemon.
pub fn validate_id(id: &str) -> Result<(), FieldError> {
    if id.is_empty() {
        return Err(FieldError {
            field: "id",
            reason: "must not be empty".to_string(),
        });
    }
    if id.chars().count() > MAX_WORKFLOW_ID_LEN_MIRROR {
        return Err(FieldError {
            field: "id",
            reason: format!("must be at most {MAX_WORKFLOW_ID_LEN_MIRROR} characters"),
        });
    }
    if !id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
        return Err(FieldError {
            field: "id",
            reason: "must contain only lowercase letters, digits, `_`, and `-`".to_string(),
        });
    }
    let first = id.chars().next().expect("id is non-empty, checked above");
    if !first.is_ascii_alphanumeric() {
        return Err(FieldError {
            field: "id",
            reason: "must start with an alphanumeric character".to_string(),
        });
    }
    Ok(())
}

/// Client-side mirror of the daemon's display-name length enforcement
/// (UI-SPEC Step 2): a blank answer is valid and handled by the caller
/// before this is ever invoked -- but a GIVEN name must be at most
/// `MAX_WORKFLOW_NAME_LEN_MIRROR` Unicode code points.
pub fn validate_display_name(name: &str) -> Result<(), FieldError> {
    if name.chars().count() > MAX_WORKFLOW_NAME_LEN_MIRROR {
        return Err(FieldError {
            field: "name",
            reason: format!("must be at most {MAX_WORKFLOW_NAME_LEN_MIRROR} characters"),
        });
    }
    Ok(())
}

/// Client-side mirror of the daemon's intent-length enforcement (UI-SPEC
/// Step 5): required, non-empty after trim, at most `MAX_INTENT_LEN_MIRROR`
/// Unicode code points.
pub fn validate_intent(intent: &str) -> Result<(), FieldError> {
    if intent.trim().is_empty() {
        return Err(FieldError {
            field: "intent",
            reason: "must not be empty".to_string(),
        });
    }
    if intent.chars().count() > MAX_INTENT_LEN_MIRROR {
        return Err(FieldError {
            field: "intent",
            reason: format!("must be at most {MAX_INTENT_LEN_MIRROR} characters"),
        });
    }
    Ok(())
}

/// Client-side mirror of the daemon's per-entry agent-file validation
/// (UI-SPEC Step 6a): non-empty, no NUL byte, at most
/// `MAX_AGENT_FILE_LEN_MIRROR` Unicode code points.
pub fn validate_agent_file(entry: &str) -> Result<(), FieldError> {
    if entry.is_empty() {
        return Err(FieldError {
            field: "agent file",
            reason: "must not be empty".to_string(),
        });
    }
    if entry.contains('\0') {
        return Err(FieldError {
            field: "agent file",
            reason: "must not contain a NUL byte".to_string(),
        });
    }
    if entry.chars().count() > MAX_AGENT_FILE_LEN_MIRROR {
        return Err(FieldError {
            field: "agent file",
            reason: format!("must be at most {MAX_AGENT_FILE_LEN_MIRROR} characters"),
        });
    }
    Ok(())
}

/// Whether the trigger checklist's current selection is valid (D-06): at
/// least one of the fixed two entries must be selected. The fixed checklist
/// itself is never empty by construction (`TRIGGER_CHOICES` always has two
/// entries) -- this validates the SELECTION, not the list. Returns
/// `TRIGGERS_EMPTY_LINE` as the `FieldError`'s `reason` (`field` left empty
/// since this message is a standalone line, never wrapped in the `[ERROR]
/// {field} {reason}.` generic shape).
pub fn validate_triggers(selected: &[String]) -> Result<(), FieldError> {
    if selected.is_empty() {
        return Err(FieldError {
            field: "",
            reason: TRIGGERS_EMPTY_LINE.to_string(),
        });
    }
    Ok(())
}

/// Derives the display-name default from a workflow id: splits on `_`/`-`,
/// title-cases each word, joins with a single space (`my_workflow` ->
/// `"My Workflow"`). Promoted from `orchestrator-cli::create::
/// derive_display_name` (10-01) unchanged -- `create::derive_display_name`
/// now delegates here so both surfaces derive the SAME default and the two
/// copies cannot drift apart.
pub fn derive_display_name(id: &str) -> String {
    id.split(['_', '-'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_id_accepts_a_well_formed_id() {
        assert!(validate_id("my-workflow_1").is_ok());
    }

    #[test]
    fn validate_id_rejects_empty() {
        let err = validate_id("").unwrap_err();
        assert_eq!(err.field, "id");
        assert_eq!(err.reason, "must not be empty");
    }

    #[test]
    fn validate_id_rejects_over_length() {
        let id = "a".repeat(MAX_WORKFLOW_ID_LEN_MIRROR + 1);
        let err = validate_id(&id).unwrap_err();
        assert_eq!(err.field, "id");
        assert_eq!(err.reason, format!("must be at most {MAX_WORKFLOW_ID_LEN_MIRROR} characters"));
    }

    #[test]
    fn validate_id_rejects_uppercase() {
        let err = validate_id("MyWorkflow").unwrap_err();
        assert_eq!(err.field, "id");
        assert_eq!(err.reason, "must contain only lowercase letters, digits, `_`, and `-`");
    }

    #[test]
    fn validate_id_rejects_non_alphanumeric_leading_char() {
        let err = validate_id("-workflow").unwrap_err();
        assert_eq!(err.field, "id");
        assert_eq!(err.reason, "must start with an alphanumeric character");
    }

    #[test]
    fn cap_mirrors_match_the_documented_writer_values() {
        // Documentation-consistency check only -- see the module doc
        // comment on why this cannot import the real
        // `registry::writer` constants (`shared` must not depend on
        // `orchestrator`, not even as a dev-dependency). The REAL drift
        // guard is `orchestrator-cli`'s own
        // `cap_mirrors_match_the_real_writer_constants` test, unchanged by
        // this promotion.
        assert_eq!(MAX_WORKFLOW_ID_LEN_MIRROR, 64);
        assert_eq!(MAX_WORKFLOW_NAME_LEN_MIRROR, 200);
        assert_eq!(MAX_INTENT_LEN_MIRROR, 1024);
        assert_eq!(MAX_TRIGGERS_MIRROR, 32);
        assert_eq!(MAX_AGENT_FILES_MIRROR, 50);
        assert_eq!(MAX_AGENT_FILE_LEN_MIRROR, 512);
    }

    #[test]
    fn trigger_choices_are_cli_then_voice() {
        assert_eq!(TRIGGER_CHOICES, &["cli", "voice"]);
    }

    #[test]
    fn validate_triggers_rejects_empty_selection_with_the_documented_line() {
        let err = validate_triggers(&[]).unwrap_err();
        assert_eq!(err.reason, "Select at least one trigger type.");
    }

    #[test]
    fn validate_triggers_accepts_one_selection() {
        assert!(validate_triggers(&["cli".to_string()]).is_ok());
    }

    #[test]
    fn step_order_matches_ui_spec() {
        assert_eq!(
            Step::ORDERED,
            &[
                Step::Id,
                Step::DisplayName,
                Step::Description,
                Step::Triggers,
                Step::Intent,
                Step::HandlerType,
                Step::AgentFiles,
                Step::AgentTimeout,
                Step::AgentBudget,
                Step::Parameters,
                Step::CollisionCheck,
                Step::Review,
            ]
        );
    }

    #[test]
    fn only_the_three_documented_substeps_are_agent_only() {
        for step in Step::ORDERED {
            let expected = matches!(step, Step::AgentFiles | Step::AgentTimeout | Step::AgentBudget);
            assert_eq!(step.is_agent_only(), expected, "{step:?}");
        }
    }

    #[test]
    fn prompt_for_matches_ui_spec_literal_text() {
        assert_eq!(prompt_for(Step::Id, "x"), "Workflow id (used as the .md filename):");
        assert_eq!(prompt_for(Step::DisplayName, "my_workflow"), "Display name [default: My Workflow]:");
        assert_eq!(prompt_for(Step::Description, "x"), "Description (workflow body text):");
        assert_eq!(prompt_for(Step::Triggers, "x"), "Select trigger types (at least one):");
        assert_eq!(
            prompt_for(Step::Intent, "x"),
            "Intent phrase (a sentence describing when this should fire, used for voice/utterance routing):"
        );
        assert_eq!(prompt_for(Step::HandlerType, "x"), "Select handler type:");
        assert_eq!(prompt_for(Step::AgentFiles, "x"), "Reference files (repeatable, blank line to finish):");
        assert_eq!(prompt_for(Step::AgentTimeout, "x"), "Timeout in seconds [default: daemon default]:");
        assert_eq!(prompt_for(Step::AgentBudget, "x"), "Max budget in USD [default: daemon default]:");
        assert_eq!(prompt_for(Step::Parameters, "x"), "Add a parameter? [y/N]:");
        assert_eq!(prompt_for(Step::CollisionCheck, "x"), "Checking for similar existing workflows...");
        assert_eq!(prompt_for(Step::Review, "x"), "Review:");
    }

    #[test]
    fn handler_choice_has_exactly_three_variants_with_ui_spec_menu_labels() {
        assert_eq!(HandlerChoice::MENU_LINES.len(), 3);
        assert_eq!(HandlerChoice::MENU_LINES[0], "  1) Script-backed action — runs a script or command you provide");
        assert_eq!(
            HandlerChoice::MENU_LINES[1],
            "  2) Markdown-body action — description text becomes the action body, no script"
        );
        assert_eq!(HandlerChoice::MENU_LINES[2], "  3) Agent (Claude-backed) — runs a Claude Code prompt");
        let expected: Vec<String> = HandlerChoice::MENU_LINES.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(menu_lines_for(Step::HandlerType), expected);
    }

    #[test]
    fn menu_lines_for_triggers_matches_ui_spec() {
        assert_eq!(menu_lines_for(Step::Triggers), vec!["  1) cli".to_string(), "  2) voice".to_string()]);
    }

    #[test]
    fn menu_lines_for_non_menu_steps_is_empty() {
        assert!(menu_lines_for(Step::Id).is_empty());
        assert!(menu_lines_for(Step::Review).is_empty());
    }

    #[test]
    fn duplicate_parameter_message_matches_ui_spec() {
        assert_eq!(duplicate_parameter_message("count"), "Parameter 'count' already added.");
    }

    #[test]
    fn derive_display_name_title_cases_and_joins_with_spaces() {
        assert_eq!(derive_display_name("my_workflow"), "My Workflow");
        assert_eq!(derive_display_name("brew-coffee"), "Brew Coffee");
    }
}
