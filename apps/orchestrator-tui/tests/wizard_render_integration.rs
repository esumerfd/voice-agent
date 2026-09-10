//! Render/integration tests for `Focus::Wizard` (Phase 10, plan 10-04).
//! Included via the same `#[path]` trick `ws_client_integration.rs`/
//! `wizard_integration.rs` use (there is no `[lib]` target for
//! `orchestrator-tui`), pulling in every module `render_wizard` transitively
//! needs so these tests exercise the exact types `main.rs` constructs.
//!
//! Task 2 (this file's first half): pure render assertions against
//! `ratatui::backend::TestBackend`, mirroring `ui.rs`'s own established test
//! harness (`render_to_string`/`row_color_containing`). `WizardState`'s
//! `#[cfg(test)]`-only `test_at_step`/`test_set_buffer` helpers let these
//! tests build an arbitrary mid-flow state directly, without re-driving the
//! real step-by-step `advance()` sequence for every field.
//!
//! Task 3 (this file's second half): drives the same extracted handler
//! functions `run_loop` calls, in the same order -- `run_loop` itself is the
//! one place in this crate no test can reach (an async loop over a real
//! terminal and a live WS client).

#[path = "../src/wizard.rs"]
mod wizard;
#[path = "../src/app.rs"]
mod app;
#[path = "../src/event.rs"]
mod event;
#[path = "../src/ui.rs"]
mod ui;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::timeout;

use shared::wizard::{HandlerChoice, Step};
use shared::{IntentCollisionChecker, IntentCollisionReport, ParameterDescriptor, ParameterType, WorkflowCreator};

use orchestrator::router::ollama_client::OllamaApi;
use orchestrator::router::Router;
use orchestrator::{InProcessOrchestrator, RouterError, Service};

use app::{App, Focus};
use event::Action;
use wizard::{WizardAnswers, WizardState};
use ws_client::TuiWsClient;

// -----------------------------------------------------------------------
// Shared test helpers
// -----------------------------------------------------------------------

/// A fresh `App` with the wizard open at exactly `state` -- bypasses
/// `open_wizard`'s always-fresh `WizardState::new()` so tests can install an
/// arbitrary mid-flow state directly.
fn app_with_wizard(state: WizardState) -> App {
    let mut app = App::new(10);
    app.open_wizard();
    *app.wizard_mut().expect("open_wizard must install a WizardState") = state;
    app
}

fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    terminal
        .draw(|frame| ui::render(frame, app, 47100))
        .expect("render into TestBackend");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for row in buffer.content().chunks(width as usize) {
        for cell in row {
            out.push_str(cell.symbol());
        }
        out.push('\n');
    }
    out
}

/// Finds the foreground color of the first non-space cell on the row
/// containing `needle` -- mirrors `ui.rs`'s own `row_color_containing`.
fn row_color_containing(app: &mut App, width: u16, height: u16, needle: &str) -> Option<Color> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    terminal
        .draw(|frame| ui::render(frame, app, 47100))
        .expect("render into TestBackend");
    let buffer = terminal.backend().buffer();
    for row in buffer.content().chunks(width as usize) {
        let line: String = row.iter().map(|c| c.symbol()).collect();
        if let Some(col) = line.find(needle) {
            return row.get(col).map(|c| c.fg);
        }
    }
    None
}

fn sample_parameters(n: usize) -> Vec<ParameterDescriptor> {
    (0..n)
        .map(|i| ParameterDescriptor {
            name: format!("param_{i:02}"),
            type_: ParameterType::String,
            required: i % 2 == 0,
        })
        .collect()
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

// -----------------------------------------------------------------------
// Task 2: chrome / split / step-indicator
// -----------------------------------------------------------------------

#[test]
fn wizard_focus_reuses_the_same_four_row_chrome_shape() {
    let mut app = app_with_wizard(WizardState::new());
    let rendered = render_to_string(&mut app, 100, 24);
    let lines: Vec<&str> = rendered.lines().collect();

    assert!(lines[0].contains("47100"), "row 0 must still be the shared header: {rendered}");
    assert!(lines[1].contains("Step 1/12"), "row 1 must be the wizard step indicator: {rendered}");
    assert!(
        lines.last().unwrap().contains("advance") && lines.last().unwrap().contains("cancel"),
        "the last row must still be the shared footer, now showing wizard hints: {rendered}"
    );
}

#[test]
fn wizard_step_indicator_reports_first_middle_and_last_positions() {
    assert_eq!(ui::wizard_step_indicator(Step::Id), "Step 1/12");
    assert_eq!(ui::wizard_step_indicator(Step::Intent), "Step 5/12");
    assert_eq!(ui::wizard_step_indicator(Step::Review), "Step 12/12");
}

#[test]
fn wizard_body_splits_fifty_fifty_with_summary_left_and_field_right() {
    let mut answers = WizardAnswers::default();
    answers.id = "brew-coffee".to_string();
    let mut app = app_with_wizard(WizardState::test_at_step(Step::DisplayName, answers));

    let rendered = render_to_string(&mut app, 100, 24);
    let lines: Vec<&str> = rendered.lines().collect();
    let half = 50usize;

    let summary_row = lines.iter().find(|l| l.contains("Id: brew-coffee")).expect("left pane must show the answered id");
    assert!(
        summary_row.chars().take(half).collect::<String>().contains("Id: brew-coffee"),
        "the running summary must render in the LEFT half: {summary_row:?}"
    );

    let field_row = lines
        .iter()
        .find(|l| l.contains("Display name"))
        .expect("right pane must show the active field's prompt");
    let right_half: String = field_row.chars().skip(half).collect();
    assert!(
        right_half.contains("Display name"),
        "the active field must render in the RIGHT half: {field_row:?}"
    );
}

// -----------------------------------------------------------------------
// Task 2: parameters (empty / zero-one-many / populated)
// -----------------------------------------------------------------------

#[test]
fn zero_parameters_renders_the_exact_parameters_none_line_at_review() {
    let answers = WizardAnswers {
        id: "brew-coffee".to_string(),
        intent: "brew coffee".to_string(),
        triggers: vec!["cli".to_string()],
        handler: Some(HandlerChoice::MarkdownAction),
        ..Default::default()
    };
    let mut app = app_with_wizard(WizardState::test_at_step(Step::Review, answers));

    let rendered = render_to_string(&mut app, 100, 24);

    assert!(rendered.contains("Parameters: none"), "expected the exact documented line: {rendered}");
}

#[test]
fn one_and_many_parameters_render_identical_bullet_copy_under_a_parameters_heading() {
    for n in [1usize, 5] {
        let answers = WizardAnswers {
            id: "brew-coffee".to_string(),
            intent: "brew coffee".to_string(),
            triggers: vec!["cli".to_string()],
            handler: Some(HandlerChoice::MarkdownAction),
            parameters: sample_parameters(n),
            ..Default::default()
        };
        let mut app = app_with_wizard(WizardState::test_at_step(Step::Review, answers));

        let rendered = render_to_string(&mut app, 100, 30);

        assert!(rendered.contains("Parameters:"), "expected the heading for n={n}: {rendered}");
        assert!(
            rendered.contains("  - param_00: string (required)"),
            "expected the exact indented bullet copy for n={n}: {rendered}"
        );
        assert!(
            !rendered.contains("Parameters: none"),
            "a non-empty list must never also render the empty-state line for n={n}"
        );
    }
}

// -----------------------------------------------------------------------
// Task 2: colors (asserted against style data, not text)
// -----------------------------------------------------------------------

#[test]
fn the_success_line_carries_green() {
    let mut state = WizardState::new();
    state.set_success("/workflows/brew-coffee.md".to_string());
    let mut app = app_with_wizard(state);

    let color = row_color_containing(&mut app, 100, 24, "[OK]");
    assert_eq!(color, Some(Color::Green), "the success line must be green");
}

#[test]
fn a_field_error_carries_red() {
    let mut state = WizardState::new();
    for c in "MyWorkflow".chars() {
        state.push_char(c);
    }
    state.advance(); // rejected: uppercase not allowed
    let mut app = app_with_wizard(state);

    let color = row_color_containing(&mut app, 100, 24, "[ERROR]");
    assert_eq!(color, Some(Color::Red), "a validation error line must be red");
}

#[test]
fn the_collision_warning_banner_carries_yellow() {
    let mut state = WizardState::new();
    state.test_set_buffer("");
    // Drive to CollisionCheck cheaply via test_at_step, then apply a report.
    let mut answers = WizardAnswers::default();
    answers.id = "brew-coffee".to_string();
    answers.intent = "brew coffee".to_string();
    let mut state = WizardState::test_at_step(Step::CollisionCheck, answers);
    state.apply_collision_report(colliding_report("countdown", "count down from a number", 0.81));
    let mut app = app_with_wizard(state);

    let color = row_color_containing(&mut app, 100, 24, "[WARN]");
    assert_eq!(color, Some(Color::Yellow), "the collision warning banner must be yellow");
}

// -----------------------------------------------------------------------
// Task 2: no glyphs -- ASCII only
// -----------------------------------------------------------------------

#[test]
fn a_full_wizard_screen_renders_only_ascii_cells() {
    let answers = WizardAnswers {
        id: "brew-coffee".to_string(),
        intent: "brew coffee".to_string(),
        triggers: vec!["cli".to_string(), "voice".to_string()],
        handler: Some(HandlerChoice::MarkdownAction),
        parameters: sample_parameters(2),
        ..Default::default()
    };
    let mut app = app_with_wizard(WizardState::test_at_step(Step::Review, answers));

    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
    terminal
        .draw(|frame| ui::render(frame, &mut app, 47100))
        .expect("render into TestBackend");
    let buffer = terminal.backend().buffer();

    for cell in buffer.content() {
        assert!(
            cell.symbol().is_ascii(),
            "every rendered cell must be ASCII, found: {:?}",
            cell.symbol()
        );
    }
}

// -----------------------------------------------------------------------
// Task 2, backstop 1: long-text wrap in the active field
// -----------------------------------------------------------------------

#[test]
fn a_long_intent_wraps_in_the_active_field_rather_than_clipping() {
    let long_text = format!("{}TAIL_MARKER_END", "x".repeat(200));

    let mut answers = WizardAnswers::default();
    answers.id = "brew-coffee".to_string();
    let mut state = WizardState::test_at_step(Step::Intent, answers);
    state.test_set_buffer(long_text.clone());
    let mut app = app_with_wizard(state);

    // Narrow terminal so the right pane's inner width is well under the
    // buffer's length -- without wrap, the tail never reaches the buffer.
    // The marker itself can land split across a hard-wrapped word boundary
    // (there is no whitespace in this fixture for ratatui's wrapper to
    // break on), so this checks for its tail fragment rather than the
    // whole literal string intact on one row.
    let rendered = render_to_string(&mut app, 60, 24);
    assert!(
        rendered.contains("_END"),
        "the long buffer must wrap onto a later row rather than clip -- its tail must reach the buffer: {rendered}"
    );

    // Negative control (load-bearing, not decorative): the SAME text
    // rendered WITHOUT Wrap must yield a line_count of 1 -- proving this
    // test would genuinely fail if wrap were ever dropped from the real
    // paragraph.
    let unwrapped = Paragraph::new(format!("> {long_text}"));
    assert_eq!(
        unwrapped.line_count(30),
        1,
        "the unwrapped negative control must report exactly one line"
    );

    // The real active-field paragraph, at the same narrow width, must wrap
    // to more than one line.
    let wrapped_line_count_gt_one = {
        let lines: Vec<&str> = rendered.lines().collect();
        let occurrences = lines.iter().filter(|l| l.contains('x')).count();
        occurrences > 1
    };
    assert!(
        wrapped_line_count_gt_one,
        "the buffer echo must occupy more than one rendered row at this width: {rendered}"
    );
}

// -----------------------------------------------------------------------
// Task 2, backstop 2: left-pane overflow with many parameters
// -----------------------------------------------------------------------

#[test]
fn many_parameters_scroll_the_left_pane_keeping_the_most_recent_visible() {
    let answers = WizardAnswers {
        id: "brew-coffee".to_string(),
        intent: "brew coffee".to_string(),
        triggers: vec!["cli".to_string()],
        handler: Some(HandlerChoice::MarkdownAction),
        parameters: sample_parameters(40),
        ..Default::default()
    };
    let mut app = app_with_wizard(WizardState::test_at_step(Step::Parameters, answers));

    let height = 16;
    let rendered = render_to_string(&mut app, 100, height);
    let lines: Vec<&str> = rendered.lines().collect();

    assert!(
        rendered.contains("param_39"),
        "the most recently answered parameter must stay visible: {rendered}"
    );
    assert!(
        !rendered.contains("param_00"),
        "the pane must not show every one of 40 parameters at a {height}-row terminal -- it must bound to a tail: {rendered}"
    );
    assert!(lines.len() <= height as usize, "rendered rows must never exceed the terminal height");
}

// -----------------------------------------------------------------------
// Task 2: collision states + in-progress
// -----------------------------------------------------------------------

#[test]
fn a_collision_renders_the_banner_score_id_intent_and_the_save_anyway_prompt() {
    let mut answers = WizardAnswers::default();
    answers.id = "brew-coffee".to_string();
    answers.intent = "brew a fresh pot of coffee".to_string();
    let mut state = WizardState::test_at_step(Step::CollisionCheck, answers);
    state.apply_collision_report(colliding_report("countdown", "count down from a number", 0.8137));
    let mut app = app_with_wizard(state);

    let rendered = render_to_string(&mut app, 120, 24);

    assert!(rendered.contains("[WARN]"), "expected the warning tag: {rendered}");
    assert!(rendered.contains("0.8137"), "expected the similarity score: {rendered}");
    assert!(rendered.contains("countdown"), "expected the colliding workflow id: {rendered}");
    assert!(rendered.contains("count down from a number"), "expected the colliding intent: {rendered}");
    assert!(rendered.contains("Save anyway?"), "expected the save-anyway prompt: {rendered}");
}

#[test]
fn a_degraded_check_renders_the_could_not_check_warning_with_no_save_anyway_prompt() {
    let mut answers = WizardAnswers::default();
    answers.id = "brew-coffee".to_string();
    let mut state = WizardState::test_at_step(Step::CollisionCheck, answers);
    // `test_set_collision_report` pins the collision report directly,
    // WITHOUT `apply_collision_report`'s auto-advance-to-Review transition
    // -- this is a pure render assertion for the (step, report) pairing
    // itself, independent of the real async flow's timing.
    state.test_set_collision_report(Some(degraded_report("ollama unreachable")));
    let mut app = app_with_wizard(state);

    let rendered = render_to_string(&mut app, 120, 24);

    assert!(rendered.contains("Could not check for similar workflows"), "expected the degrade line: {rendered}");
    assert!(!rendered.contains("Save anyway?"), "a degraded check must never show the save-anyway prompt: {rendered}");
}

#[test]
fn the_in_progress_line_renders_while_the_collision_check_is_in_flight() {
    let mut answers = WizardAnswers::default();
    answers.id = "brew-coffee".to_string();
    let mut state = WizardState::test_at_step(Step::CollisionCheck, answers);
    state.set_collision_check_in_flight(true);
    let mut app = app_with_wizard(state);

    let rendered = render_to_string(&mut app, 120, 24);

    assert!(
        rendered.contains("Checking for similar existing workflows..."),
        "expected the documented in-progress line: {rendered}"
    );
    assert!(!rendered.contains("Save anyway?"), "the in-progress line must not also show a stale prompt");
}
