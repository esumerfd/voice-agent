//! Wizard action dispatch (Phase 10, plan 10-04, Task 3): the extracted,
//! independently-testable per-action handlers `main.rs`'s `run_loop`
//! dispatches to for every `Action::Wizard*`/`Action::OpenWizard` variant.
//!
//! Lives in its own leaf module -- not inline in `main.rs` -- for the same
//! reason `App`'s scroll math lives in `app.rs`, not `main.rs`: `run_loop`
//! itself (an async loop over a real terminal and a live WS client) is the
//! one place in this crate no unit test can reach, and this crate has no
//! `[lib]` target, so a function that needs to be both driven by `main.rs`
//! AND `#[path]`-included independently into a test binary must live in a
//! leaf module with no `mod` declarations of its own (mirroring `ui.rs`/
//! `event.rs`/`app.rs`/`wizard.rs`'s existing shape) -- `main.rs` itself
//! cannot be `#[path]`-included without duplicating every module it
//! declares into a second, incompatible copy.
//!
//! Every function here is generic over `ratatui::backend::Backend`, so
//! tests drive it against `ratatui::backend::TestBackend` while `main.rs`
//! drives the identical code against the real terminal.

use std::sync::atomic::{AtomicUsize, Ordering};

use ratatui::backend::Backend;
use ratatui::Terminal;

use shared::wizard::{HandlerChoice, Step, TRIGGER_CHOICES};
use shared::{IntentCollisionChecker, WorkflowCreator};

use crate::app::App;
use crate::ui;

/// A printable character typed into the wizard's active field (T-10-12's
/// established discipline continues here, unchanged for every other step):
/// `Step::Triggers` intercepts a digit as a checklist TOGGLE (D-06) and
/// `Step::HandlerType` intercepts a digit as a menu SELECTION that itself
/// advances (D-05, `WizardState::select_handler`) -- every other character,
/// on every other step, appends to the buffer verbatim exactly as before.
pub fn handle_wizard_char(app: &mut App, c: char) {
    let Some(step) = app.wizard().map(|w| w.step()) else {
        return;
    };
    let Some(wizard) = app.wizard_mut() else {
        return;
    };

    match step {
        Step::Triggers => match c.to_digit(10) {
            Some(digit) if (1..=TRIGGER_CHOICES.len() as u32).contains(&digit) => {
                wizard.toggle_trigger(digit as usize - 1);
            }
            _ => wizard.push_char(c),
        },
        Step::HandlerType => match c {
            '1' => wizard.select_handler(HandlerChoice::ScriptAction),
            '2' => wizard.select_handler(HandlerChoice::MarkdownAction),
            '3' => wizard.select_handler(HandlerChoice::Agent),
            _ => wizard.push_char(c),
        },
        _ => wizard.push_char(c),
    }
}

/// `Action::WizardAdvance`'s full dispatch (Task 3 D-1): the wizard's
/// SINGLE `create_workflow` call site (`Step::Review`'s branch below) sits
/// strictly downstream of the collision gate (T-10-20) -- the gate's own
/// transition to `Step::Review` happens entirely inside `WizardState`
/// (`advance`/`apply_collision_report`), so no path through this function
/// can reach the create call without having passed through it first.
///
/// While the wizard is showing its terminal success screen, `WizardAdvance`
/// dismisses it (closes the wizard, returns focus to Activities) instead of
/// doing anything step-related -- the plan's own action text: "on success,
/// close the wizard and return focus to Activities."
pub async fn handle_wizard_advance<B: Backend>(
    app: &mut App,
    client: &dyn WorkflowCreator,
    checker: &dyn IntentCollisionChecker,
    terminal: &mut Terminal<B>,
    port: u16,
    render_count: &AtomicUsize,
) {
    let Some((step, has_success)) = app
        .wizard()
        .map(|w| (w.step(), w.success_path().is_some()))
    else {
        return;
    };

    if has_success {
        app.close_wizard();
        return;
    }

    if step == Step::Review {
        let Some(req) = app.wizard().map(|w| w.to_request()) else {
            return;
        };
        let resp = client.create_workflow(req).await;
        if let Some(wizard) = app.wizard_mut() {
            if resp.created {
                wizard.set_success(resp.workflow_path.unwrap_or_default());
            } else {
                wizard.set_server_error(resp.error.unwrap_or_else(|| "unknown error".to_string()));
            }
        }
    } else if let Some(wizard) = app.wizard_mut() {
        // Every other step, including `Step::CollisionCheck`'s save-anyway
        // gate (`WizardState::advance_collision_check` resolves the y/N
        // buffer answer synchronously -- no daemon call on THIS keypress).
        wizard.advance();
    }

    // The Parameters loop ending (a plain `advance()` above) lands
    // directly on `Step::CollisionCheck` with no report yet -- this is what
    // triggers the REAL async check automatically, on this SAME keypress,
    // never a separate one (UI-SPEC's own "no user prompt" for Step 8).
    let should_check_now = app
        .wizard()
        .map(|w| w.step() == Step::CollisionCheck && w.collision_report().is_none() && !w.collision_check_in_flight())
        .unwrap_or(false);
    if should_check_now {
        run_collision_check(app, checker, terminal, port, render_count).await;
    }
}

/// The Step 8 async collision check (D-03/D-04). Ordered precisely per this
/// plan's own loading contract, because this is what makes UI-SPEC's
/// loading contract real rather than aspirational: set the in-flight flag,
/// then DRAW, then await the checker, then clear the flag and draw again.
/// On this single-threaded loop, the await blocks the next redraw, so a
/// draw that happened only AFTER the await would leave the user staring at
/// a frozen screen for the duration of a local-model turn (this project's
/// own documented multi-second latency) -- the draw here happens explicitly
/// before that await begins.
async fn run_collision_check<B: Backend>(
    app: &mut App,
    checker: &dyn IntentCollisionChecker,
    terminal: &mut Terminal<B>,
    port: u16,
    render_count: &AtomicUsize,
) {
    let Some(intent) = app.wizard().map(|w| w.answers().intent.clone()) else {
        return;
    };

    if let Some(wizard) = app.wizard_mut() {
        wizard.set_collision_check_in_flight(true);
    }
    let _ = terminal.draw(|frame| {
        ui::render(frame, app, port);
        render_count.store(frame.count(), Ordering::SeqCst);
    });

    let report = checker.check_intent_collision(&intent).await;

    if let Some(wizard) = app.wizard_mut() {
        wizard.set_collision_check_in_flight(false);
        wizard.apply_collision_report(report);
    }
    let _ = terminal.draw(|frame| {
        ui::render(frame, app, port);
        render_count.store(frame.count(), Ordering::SeqCst);
    });
}
