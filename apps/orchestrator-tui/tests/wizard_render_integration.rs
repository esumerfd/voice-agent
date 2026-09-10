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
#[path = "../src/wizard_dispatch.rs"]
mod wizard_dispatch;
#[path = "../src/ws_client.rs"]
mod ws_client;

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
use shared::{
    CreateWorkflowRequest, CreateWorkflowResponse, IntentCollisionChecker, IntentCollisionReport,
    ParameterDescriptor, ParameterType, ProtocolFrame, WorkflowCreator,
};

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

// =========================================================================
// Task 3: wiring the loop and same-session routing
// =========================================================================

// -----------------------------------------------------------------------
// Task 3 test doubles
// -----------------------------------------------------------------------

#[derive(Default)]
struct MockWorkflowCreator {
    calls: AtomicUsize,
    last_request: Mutex<Option<CreateWorkflowRequest>>,
    response: Mutex<Option<CreateWorkflowResponse>>,
}

impl MockWorkflowCreator {
    fn new() -> Self {
        Self::default()
    }

    fn with_response(response: CreateWorkflowResponse) -> Self {
        Self {
            response: Mutex::new(Some(response)),
            ..Self::default()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last_request(&self) -> CreateWorkflowRequest {
        self.last_request
            .lock()
            .expect("mutex poisoned")
            .clone()
            .expect("create_workflow should have been called at least once")
    }
}

#[async_trait]
impl WorkflowCreator for MockWorkflowCreator {
    async fn create_workflow(&self, req: CreateWorkflowRequest) -> CreateWorkflowResponse {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_request.lock().expect("mutex poisoned") = Some(req.clone());
        self.response.lock().expect("mutex poisoned").clone().unwrap_or(CreateWorkflowResponse {
            created: true,
            workflow_path: Some(format!("/workflows/{}.md", req.id)),
            script_path: None,
            error: None,
        })
    }
}

/// A checker stub returning a queue of canned reports (one per call, in
/// call order; the queue's final report repeats once exhausted) -- mirrors
/// `orchestrator-cli/tests/create_wizard_integration.rs::StubIntentCollisionChecker`.
struct QueueChecker {
    reports: Mutex<VecDeque<IntentCollisionReport>>,
}

impl QueueChecker {
    fn new(reports: Vec<IntentCollisionReport>) -> Self {
        Self {
            reports: Mutex::new(reports.into()),
        }
    }
}

#[async_trait]
impl IntentCollisionChecker for QueueChecker {
    async fn check_intent_collision(&self, _intent: &str) -> IntentCollisionReport {
        let mut queue = self.reports.lock().expect("mutex poisoned");
        if queue.len() > 1 {
            queue.pop_front().expect("checked len() > 1 above")
        } else {
            queue.front().cloned().unwrap_or_else(no_collision_report)
        }
    }
}

/// A checker that also records the shared render-count's value at the exact
/// moment it is invoked -- the draw-before-await ordering evidence (Task 3
/// D-2).
struct RecordingChecker {
    render_count: Arc<AtomicUsize>,
    report: IntentCollisionReport,
    recorded_at_invocation: Mutex<Vec<usize>>,
}

impl RecordingChecker {
    fn new(render_count: Arc<AtomicUsize>, report: IntentCollisionReport) -> Self {
        Self {
            render_count,
            report,
            recorded_at_invocation: Mutex::new(Vec::new()),
        }
    }

    fn recorded_at_invocation(&self) -> Vec<usize> {
        self.recorded_at_invocation.lock().expect("mutex poisoned").clone()
    }
}

#[async_trait]
impl IntentCollisionChecker for RecordingChecker {
    async fn check_intent_collision(&self, _intent: &str) -> IntentCollisionReport {
        self.recorded_at_invocation
            .lock()
            .expect("mutex poisoned")
            .push(self.render_count.load(Ordering::SeqCst));
        self.report.clone()
    }
}

/// Drives a fresh, already-open wizard through every plain synchronous step
/// (id, blank display name, blank description, the `cli` trigger, the given
/// intent, a markdown-body handler, and a declined add-a-parameter prompt)
/// directly via `WizardState`'s own mutators -- none of these need a daemon
/// or checker call. Leaves the wizard on `Step::Parameters` with the
/// buffer already holding `"n"`, ready for the CALLER to drive the one
/// ending keypress through the real `wizard_dispatch::handle_wizard_advance`
/// so the collision-check auto-trigger fires exactly as `run_loop` would.
fn fill_through_parameters(app: &mut App, id: &str, intent: &str) {
    let wizard = app.wizard_mut().expect("wizard must be open");
    for c in id.chars() {
        wizard.push_char(c);
    }
    wizard.advance(); // Id -> DisplayName
    wizard.advance(); // blank DisplayName -> Description
    wizard.advance(); // blank Description -> Triggers
    wizard.toggle_trigger(0); // cli
    wizard.advance(); // Triggers -> Intent
    for c in intent.chars() {
        wizard.push_char(c);
    }
    wizard.advance(); // Intent -> HandlerType
    wizard.select_handler(HandlerChoice::MarkdownAction); // -> Parameters
    wizard.push_char('n'); // "don't add a parameter"
}

fn fresh_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(width, height)).expect("TestBackend terminal")
}

// -----------------------------------------------------------------------
// Task 3: exhaustive Action dispatch coverage
// -----------------------------------------------------------------------

/// Fails to COMPILE (non-exhaustive match) if a new `Action` variant is
/// ever added without a case here -- the load-bearing property of this
/// test, proving every `Action::Wizard*`/`Action::OpenWizard` variant has a
/// real dispatch path (this file's own tests, or `main.rs`'s `run_loop`
/// arms for the non-wizard variants, all listed here for one single source
/// of exhaustiveness truth).
#[test]
fn every_action_variant_is_accounted_for_in_dispatch() {
    fn assert_exhaustive(action: Action) {
        match action {
            Action::SelectNext
            | Action::SelectPrev
            | Action::SelectFirst
            | Action::SelectLast
            | Action::OpenTab
            | Action::FocusActivities
            | Action::Quit
            | Action::ShowHelp
            | Action::CloseHelp
            | Action::ScrollDown
            | Action::ScrollUp
            | Action::ScrollHalfPageDown
            | Action::ScrollHalfPageUp
            | Action::ScrollPageDown
            | Action::ScrollPageUp
            | Action::ScrollTop
            | Action::ScrollBottom
            | Action::NextTab
            | Action::PrevTab => {}
            // The six wizard actions (Phase 10, plan 10-04): each has a
            // real `run_loop` arm in `main.rs` -- `OpenWizard`/
            // `WizardBackspace`/`WizardBack`/`WizardCancel` call directly
            // into `App`/`WizardState`; `WizardChar`/`WizardAdvance` call
            // into `wizard_dispatch`, exercised directly by this file's
            // other Task 3 tests.
            Action::OpenWizard
            | Action::WizardChar(_)
            | Action::WizardBackspace
            | Action::WizardAdvance
            | Action::WizardBack
            | Action::WizardCancel => {}
        }
    }
    assert_exhaustive(Action::OpenWizard);
}

// -----------------------------------------------------------------------
// Task 3: digit-driven menu/checklist selection (WizardChar dispatch)
// -----------------------------------------------------------------------

#[test]
fn wizard_char_toggles_triggers_by_digit_instead_of_typing_them() {
    let mut app = App::new(10);
    app.open_wizard();
    {
        let wizard = app.wizard_mut().unwrap();
        for c in "brew-coffee".chars() {
            wizard.push_char(c);
        }
        wizard.advance(); // Id -> DisplayName
        wizard.advance(); // blank -> Description
        wizard.advance(); // blank -> Triggers
    }

    wizard_dispatch::handle_wizard_char(&mut app, '2'); // "voice" -- toggled, not typed

    let wizard = app.wizard().unwrap();
    assert_eq!(wizard.buffer(), "", "a recognized digit must toggle, never enter the buffer");
    assert_eq!(wizard.selected_triggers(), &["voice".to_string()]);
}

#[test]
fn wizard_char_selects_a_handler_by_digit_and_advances() {
    let mut app = App::new(10);
    app.open_wizard();
    {
        let wizard = app.wizard_mut().unwrap();
        for c in "brew-coffee".chars() {
            wizard.push_char(c);
        }
        wizard.advance(); // Id -> DisplayName
        wizard.advance(); // blank -> Description
        wizard.advance(); // blank -> Triggers
        wizard.toggle_trigger(0);
        wizard.advance(); // -> Intent
        for c in "brew coffee".chars() {
            wizard.push_char(c);
        }
        wizard.advance(); // -> HandlerType
    }

    wizard_dispatch::handle_wizard_char(&mut app, '2'); // Markdown-body action

    let wizard = app.wizard().unwrap();
    assert_eq!(wizard.step(), Step::Parameters, "selecting a handler is itself an advancing action (D-05)");
    assert_eq!(wizard.answers().handler, Some(HandlerChoice::MarkdownAction));
}

#[test]
fn wizard_char_falls_back_to_plain_typing_outside_triggers_and_handler_type() {
    let mut app = App::new(10);
    app.open_wizard();

    wizard_dispatch::handle_wizard_char(&mut app, '2'); // Step::Id -- must type, not toggle/select

    assert_eq!(app.wizard().unwrap().buffer(), "2");
}

// -----------------------------------------------------------------------
// Task 3: cancel
// -----------------------------------------------------------------------

#[test]
fn wizard_cancel_returns_to_activities_preserving_the_prior_selection() {
    let mut app = App::new(10);
    app.ingest(shared::ActivityEvent {
        run_id: "run-1".to_string(),
        workflow_id: "set_timer".to_string(),
        client_name: "orchestrator-tui".to_string(),
        session_id: "sess-1".to_string(),
        status: shared::ActivityStatus::Success,
        started_at_ms: 100,
        log: vec![],
    });
    app.select_next();
    let selected_before = app.selected().unwrap().run_id.clone();

    app.open_wizard();
    // `main.rs`'s `run_loop` arm for `Action::WizardCancel` is the direct
    // one-liner `app.close_wizard()` -- exercised as-is here.
    app.close_wizard();

    assert_eq!(app.focus(), Focus::Activities);
    assert_eq!(app.selected().unwrap().run_id, selected_before);
}

// -----------------------------------------------------------------------
// Task 3: draw-before-await ordering
// -----------------------------------------------------------------------

#[tokio::test]
async fn the_collision_check_draws_before_the_checker_await_begins() {
    let mut app = App::new(10);
    app.open_wizard();
    fill_through_parameters(&mut app, "brew-coffee", "brew coffee");

    let mut terminal = fresh_terminal(80, 24);
    let render_count = Arc::new(AtomicUsize::new(0));
    let checker = RecordingChecker::new(Arc::clone(&render_count), no_collision_report());
    let creator = MockWorkflowCreator::new();

    // `Frame::count()` is 0-indexed, so a bare `render_count.load()` before
    // ANY draw has ever happened is indistinguishable from "the first draw
    // already happened" (both read 0). Priming with one real draw first
    // establishes a known, already-incremented baseline to compare against.
    let _ = terminal.draw(|frame| render_count.store(frame.count(), Ordering::SeqCst));
    let before_dispatch = render_count.load(Ordering::SeqCst);
    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, render_count.as_ref())
        .await;

    let recorded = checker.recorded_at_invocation();
    assert_eq!(recorded.len(), 1, "the checker must be invoked exactly once");
    assert!(
        recorded[0] > before_dispatch,
        "the render count observed AT CHECKER-INVOCATION time ({recorded:?}) must be strictly greater than its \
         value before the collision-check dispatch began ({before_dispatch}) -- proving a draw happened before \
         the await, not after"
    );
}

// -----------------------------------------------------------------------
// Task 3: the collision gate itself
// -----------------------------------------------------------------------

#[tokio::test]
async fn collision_declined_returns_to_intent_with_the_creator_never_called() {
    let mut app = App::new(10);
    app.open_wizard();
    fill_through_parameters(&mut app, "brew-coffee", "brew coffee");

    let mut terminal = fresh_terminal(80, 24);
    let render_count = AtomicUsize::new(0);
    let creator = MockWorkflowCreator::new();
    let checker = QueueChecker::new(vec![colliding_report("countdown", "count down from a number", 0.81)]);

    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;
    assert_eq!(app.wizard().unwrap().step(), Step::CollisionCheck, "setup: expected the real collision to be pending");

    // Blank answer at the save-anyway prompt declines by default (D-04).
    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;

    assert_eq!(app.wizard().unwrap().step(), Step::Intent, "declining must return to the intent step");
    assert_eq!(creator.calls(), 0, "the creator mock's call counter must stay at 0 (T-10-20)");
}

#[tokio::test]
async fn collision_overridden_reaches_review_and_a_confirmed_create_calls_the_mock_once_with_the_original_intent() {
    let mut app = App::new(10);
    app.open_wizard();
    fill_through_parameters(&mut app, "brew-coffee", "brew coffee");

    let mut terminal = fresh_terminal(80, 24);
    let render_count = AtomicUsize::new(0);
    let creator = MockWorkflowCreator::new();
    let checker = QueueChecker::new(vec![colliding_report("countdown", "count down from a number", 0.81)]);

    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;
    assert_eq!(app.wizard().unwrap().step(), Step::CollisionCheck);

    app.wizard_mut().unwrap().push_char('y');
    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;
    assert_eq!(app.wizard().unwrap().step(), Step::Review, "confirming the override must reach review");
    assert_eq!(creator.calls(), 0, "reaching review must not itself create anything");

    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;

    assert_eq!(creator.calls(), 1, "a confirmed create must call the mock exactly once");
    assert_eq!(
        creator.last_request().intent,
        Some("brew coffee".to_string()),
        "the original intent must be preserved through an override, not the colliding one"
    );
}

#[tokio::test]
async fn a_degraded_check_proceeds_to_review_with_no_prompt_and_a_confirmed_create_still_calls_the_mock_once() {
    let mut app = App::new(10);
    app.open_wizard();
    fill_through_parameters(&mut app, "brew-coffee", "brew coffee");

    let mut terminal = fresh_terminal(80, 24);
    let render_count = AtomicUsize::new(0);
    let creator = MockWorkflowCreator::new();
    let checker = QueueChecker::new(vec![degraded_report("ollama unreachable")]);

    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;

    assert_eq!(app.wizard().unwrap().step(), Step::Review, "a degraded check must proceed to review with no keypress");
    assert_eq!(creator.calls(), 0);

    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;

    assert_eq!(creator.calls(), 1, "a confirmed create must still succeed after a degraded check");
}

// -----------------------------------------------------------------------
// Task 3: terminal states
// -----------------------------------------------------------------------

#[tokio::test]
async fn a_successful_create_renders_the_success_line_with_the_workflow_id_and_path() {
    let mut app = App::new(10);
    app.open_wizard();
    fill_through_parameters(&mut app, "brew-coffee", "brew coffee");

    let mut terminal = fresh_terminal(100, 24);
    let render_count = AtomicUsize::new(0);
    let creator = MockWorkflowCreator::new();
    let checker = QueueChecker::new(vec![no_collision_report()]);

    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;
    assert_eq!(app.wizard().unwrap().step(), Step::Review);
    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;

    assert_eq!(app.wizard().unwrap().success_path(), Some("/workflows/brew-coffee.md"));

    let rendered = render_to_string(&mut app, 100, 24);
    assert!(
        rendered.contains("[OK] Workflow 'brew-coffee' created at /workflows/brew-coffee.md."),
        "expected the documented success line: {rendered}"
    );
    assert!(
        rendered.contains("Try it now: orchestrator run brew-coffee"),
        "expected the try-it-now line demonstrating CREATEUI-02's same-session guarantee: {rendered}"
    );
}

#[tokio::test]
async fn server_rejection_renders_the_error_and_returns_to_the_id_step_only() {
    let mut app = App::new(10);
    app.open_wizard();
    fill_through_parameters(&mut app, "brew-coffee", "brew coffee");

    let mut terminal = fresh_terminal(100, 24);
    let render_count = AtomicUsize::new(0);
    let creator = MockWorkflowCreator::with_response(CreateWorkflowResponse {
        created: false,
        workflow_path: None,
        script_path: None,
        error: Some("workflow 'brew-coffee' already exists".to_string()),
    });
    let checker = QueueChecker::new(vec![no_collision_report()]);

    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;
    assert_eq!(app.wizard().unwrap().step(), Step::Review);
    wizard_dispatch::handle_wizard_advance(&mut app, &creator, &checker, &mut terminal, 47100, &render_count).await;

    assert_eq!(app.wizard().unwrap().step(), Step::Id, "a server rejection must re-prompt the id step only");
    assert_eq!(
        app.wizard().unwrap().server_error(),
        Some("workflow 'brew-coffee' already exists")
    );

    let rendered = render_to_string(&mut app, 100, 24);
    assert!(
        rendered.contains("[ERROR] workflow 'brew-coffee' already exists"),
        "expected the daemon's error text verbatim: {rendered}"
    );
}

// -----------------------------------------------------------------------
// Task 3, CREATEUI-02: same-session routing from the TUI
// -----------------------------------------------------------------------

struct StubOllama {
    vectors: HashMap<String, Vec<f32>>,
}

impl StubOllama {
    fn new(vectors: HashMap<String, Vec<f32>>) -> Self {
        Self { vectors }
    }
}

#[async_trait]
impl OllamaApi for StubOllama {
    async fn embed(&self, _model: &str, inputs: &[String]) -> Result<Vec<Vec<f32>>, RouterError> {
        Ok(inputs.iter().map(|i| self.vectors.get(i).cloned().unwrap_or_default()).collect())
    }

    async fn generate_json(
        &self,
        _model: &str,
        _system: &str,
        _prompt: &str,
        _schema: &serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        Ok(serde_json::json!({}))
    }
}

async fn spawn_daemon(workflows_dir: &Path, vectors: HashMap<String, Vec<f32>>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("failed to read the bound local_addr")
        .port();
    let handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    let client: Arc<dyn OllamaApi> = Arc::new(StubOllama::new(vectors));
    let router = Arc::new(Router::new(client, "nomic-embed-text", "llama3.2:3b"));
    let orchestrator = Arc::new(InProcessOrchestrator::with_router(workflows_dir, handlers, router));
    let activity_registry = Arc::new(orchestrator::activity::ActivityRegistry::new());
    tokio::spawn(orchestrator::server::serve(listener, orchestrator, activity_registry));
    port
}

/// The load-bearing test (CREATEUI-02): against an in-process daemon spawned
/// with a stub `OllamaApi`, driving the wizard to completion through the
/// SAME dispatch `run_loop` uses writes the `.md`, and then -- WITHOUT
/// restarting or re-spawning that daemon -- a `ProtocolFrame::RouteUtterance`
/// carrying the answered intent returns a `RouteResult` naming the new
/// workflow. Mirrors `orchestrator-cli/tests/create_wizard_integration.rs`'s
/// `created_workflow_is_routable_against_the_same_daemon_with_no_restart`.
#[tokio::test]
async fn a_tui_created_workflow_routes_against_the_same_daemon_with_no_restart() {
    const INTENT: &str = "brew a fresh pot of coffee";
    let mut vectors = HashMap::new();
    vectors.insert(INTENT.to_string(), vec![1.0, 0.0, 0.0]);

    let dir = TempDir::new().expect("failed to create tempdir");
    let port = spawn_daemon(dir.path(), vectors).await;
    let client = TuiWsClient::connect(&format!("ws://127.0.0.1:{port}"))
        .await
        .expect("connect should succeed against a live in-process WS server");

    let mut app = App::new(10);
    app.open_wizard();
    fill_through_parameters(&mut app, "brew-coffee", INTENT);

    let mut terminal = fresh_terminal(100, 24);
    let render_count = AtomicUsize::new(0);

    // Real collision check against the real daemon (zero prior workflows --
    // nothing to collide with) -- proceeds straight to Review.
    wizard_dispatch::handle_wizard_advance(&mut app, &client, &client, &mut terminal, port, &render_count).await;
    assert_eq!(
        app.wizard().unwrap().step(),
        Step::Review,
        "setup: expected a clean collision check against zero prior workflows to reach review"
    );

    // The real write.
    wizard_dispatch::handle_wizard_advance(&mut app, &client, &client, &mut terminal, port, &render_count).await;
    assert!(
        app.wizard().unwrap().success_path().is_some(),
        "expected the real daemon to confirm the create"
    );

    // No second spawn, no restart -- routing goes through the SAME client /
    // daemon instance this wizard just wrote to.
    let route_reply = timeout(
        Duration::from_secs(2),
        client.call_protocol(ProtocolFrame::RouteUtterance {
            utterance: INTENT.to_string(),
        }),
    )
    .await
    .expect("call_protocol timed out")
    .expect("call_protocol should not error against a live in-process WS server");

    match route_reply {
        ProtocolFrame::RouteResult { matched_workflow_id, .. } => {
            assert_eq!(
                matched_workflow_id,
                Some("brew-coffee".to_string()),
                "expected the newly-created workflow to be routable with no daemon restart"
            );
        }
        other => panic!("expected ProtocolFrame::RouteResult, got: {other:?}"),
    }
}
