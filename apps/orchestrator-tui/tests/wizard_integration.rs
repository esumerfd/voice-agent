//! `App`-integration tests for the wizard (Phase 10, plan 10-03, Task 2).
//! Included via the same `#[path]` trick `ws_client_integration.rs` uses
//! (there is no `[lib]` target for `orchestrator-tui`) so these tests
//! exercise the exact `App`/`WizardState` types `main.rs` constructs.
//!
//! `wizard.rs`'s own pure step-machine tests live in its `#[cfg(test)]`
//! module; this file covers only the `App`-level seam: opening/closing the
//! wizard, and proving the existing read-only Activities/Detail behavior is
//! untouched while a wizard is open.

#[path = "../src/wizard.rs"]
mod wizard;
#[path = "../src/app.rs"]
mod app;

use app::{App, Focus};
use shared::wizard::Step;
use shared::{ActivityEvent, ActivityLogEvent, ActivityPhase, ActivityStatus};

fn event(run_id: &str, started_at_ms: u64) -> ActivityEvent {
    ActivityEvent {
        run_id: run_id.to_string(),
        workflow_id: "set_timer".to_string(),
        client_name: "orchestrator-cli".to_string(),
        session_id: "sess-1".to_string(),
        status: ActivityStatus::NotStarted,
        started_at_ms,
        log: vec![ActivityLogEvent {
            phase: ActivityPhase::Invoked,
            at_ms: started_at_ms,
            detail: None,
        }],
    }
}

#[test]
fn open_wizard_sets_focus_and_installs_a_fresh_state() {
    let mut app = App::new(10);
    assert!(app.wizard().is_none(), "a fresh App must have no wizard open");

    app.open_wizard();

    assert_eq!(app.focus(), Focus::Wizard, "open_wizard must move focus to Focus::Wizard");
    let state = app.wizard().expect("open_wizard must install a WizardState");
    assert_eq!(state.step(), Step::Id, "a freshly-opened wizard must start on the id step");
}

#[test]
fn close_wizard_restores_activities_focus_and_discards_state() {
    let mut app = App::new(10);
    app.open_wizard();

    app.close_wizard();

    assert_eq!(app.focus(), Focus::Activities, "close_wizard must return focus to Activities");
    assert!(app.wizard().is_none(), "close_wizard must discard the in-progress WizardState");
}

#[test]
fn close_wizard_leaves_the_activities_selection_exactly_as_it_was() {
    let mut app = App::new(10);
    app.ingest(event("run-1", 100));
    app.ingest(event("run-2", 200));
    app.select_next();
    app.select_next();
    let selected_before = app.selected().unwrap().run_id.clone();

    app.open_wizard();
    app.close_wizard();

    assert_eq!(
        app.selected().unwrap().run_id,
        selected_before,
        "the Activities selection must be exactly what it was before the wizard opened"
    );
}

#[test]
fn activity_ingest_and_selection_are_unaffected_while_a_wizard_is_open() {
    let mut app = App::new(10);
    app.ingest(event("run-1", 100));
    app.select_next();

    app.open_wizard();
    app.ingest(event("run-2", 200));

    assert_eq!(app.activities().len(), 2, "ingest must keep working while a wizard is open");
    assert_eq!(
        app.selected().unwrap().run_id,
        "run-1",
        "the existing selection-tracking behavior must be unaffected by an open wizard"
    );
    assert_eq!(app.focus(), Focus::Wizard, "the wizard must stay open across an unrelated ingest");
}

#[test]
fn wizard_mut_drives_the_same_state_wizard_returns() {
    let mut app = App::new(10);
    app.open_wizard();

    app.wizard_mut().unwrap().push_char('x');

    assert_eq!(app.wizard().unwrap().buffer(), "x");
}
