//! `orchestrator-tui` binary entry point.
//!
//! Connects to `orchestratord` over the D-03 envelope protocol, identifying
//! itself via `Envelope::Hello` (D-04) before any other frame -- see
//! `ws_client::TuiWsClient::connect`. A failure to reach orchestratord
//! prints a clear, actionable message and exits nonzero -- never
//! auto-spawning the daemon (D-04 precedent, mirroring
//! `orchestrator-cli`'s own connect-or-fail `main.rs`).
//!
//! Once connected, this runs the full ratatui render loop (05-07 Task 3):
//! each tick drains whatever `Activity` events have arrived through
//! `App::ingest` (the single bounded chokepoint -- D-11/Pitfall 4, never a
//! second bare push), redraws via `ui::render`, then polls for a keyboard
//! `Action` (`j`/`k`/`g`/`G`/`Enter`/`Esc`/`q`/`?`/`Tab`/`Shift-Tab`) and
//! applies it, until `q` breaks the loop. `j`/`k`/`g`/`G` are routed by
//! which panel has focus (quick task 260815-oay D-1): they move the
//! Activities selection or scroll the Detail panel depending on
//! `App::focus()`. `Tab`/`Shift-Tab` (260816-ems, re-scoped from quick task
//! 260815-rx6) cycle `current_phase` forward/backward through the ONE open
//! activity's own lifecycle `log` while the Detail panel is focused --
//! inert anywhere else. While the `?` help overlay is open, `q` closes the
//! overlay instead of quitting (D-2) -- `map_key` enforces that modal
//! remap, not this loop.
//! `ratatui::init()`/`restore()` provide panic-safe terminal teardown
//! (T-05-14).

mod app;
mod cli;
mod event;
mod ui;
mod ws_client;

use std::time::Duration;

use clap::Parser;

use app::App;
use cli::Cli;
use event::Action;
use ws_client::TuiWsClient;

/// Client-side bounded activity buffer (D-11) -- oldest evicted first once
/// this many activities are tracked.
const MAX_ACTIVITIES: usize = 200;

/// How long each render-loop tick waits for a keyboard event before
/// re-checking for new activity events and redrawing.
const POLL_TIMEOUT: Duration = Duration::from_millis(100);

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let addr = format!("ws://127.0.0.1:{}", cli.port);

    let client = match TuiWsClient::connect(&addr).await {
        Ok(client) => client,
        Err(err) => {
            // D-04: a clear, nonzero-exit error naming the fix -- never an
            // auto-spawn of the daemon.
            eprintln!(
                "cannot reach orchestratord at {addr}: {err}. Start it with `orchestratord --port {}` (see packaging/README.md).",
                cli.port
            );
            std::process::exit(1);
        }
    };

    let mut app = App::new(MAX_ACTIVITIES);

    // ratatui::init()/restore() install a panic hook that restores the
    // terminal even on an unexpected panic mid-loop (T-05-14 panic-safe
    // teardown) -- restore() always runs, even if run_loop below returns
    // early.
    let mut terminal = ratatui::init();
    run_loop(&mut terminal, &mut app, &client, cli.port).await;
    ratatui::restore();
}

/// The main render loop: drain pending activity events through the bounded
/// `App::ingest` chokepoint, redraw, poll for a key action, apply it, and
/// repeat until `q` (`Action::Quit`) breaks the loop.
async fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    client: &TuiWsClient,
    port: u16,
) {
    loop {
        // Drain whatever Activity events have arrived since the last tick
        // -- the initial replay burst and every subsequent live push both
        // route through this same `App::ingest` chokepoint (D-11/Pitfall
        // 4), never a second bare `Vec::push`.
        for event in client.try_drain_activity_events().await {
            app.ingest(event);
        }

        // A draw failure is never fatal -- the loop simply tries again
        // next tick (never `.unwrap()`/`.expect()` on terminal I/O).
        let _ = terminal.draw(|frame| ui::render(frame, app, port));

        // Each arm is a direct one-line call into `App` -- no scroll
        // arithmetic lives here. `run_loop` is the one place in this crate
        // no unit test can reach (an async loop over a real terminal and a
        // live WS client), so the actual math lives in `App`'s pure,
        // independently-tested scroll methods (Task 2 D-1).
        match event::poll_action(POLL_TIMEOUT, app.focus(), app.help_open()) {
            Some(Action::SelectNext) => app.select_next(),
            Some(Action::SelectPrev) => app.select_prev(),
            Some(Action::SelectFirst) => app.select_first(),
            Some(Action::SelectLast) => app.select_last(),
            Some(Action::OpenTab) => app.open_selected_activity(),
            Some(Action::FocusActivities) => app.focus_activities(),
            Some(Action::ScrollDown) => app.scroll_detail_lines(1),
            Some(Action::ScrollUp) => app.scroll_detail_lines(-1),
            Some(Action::ScrollHalfPageDown) => app.scroll_detail_half_page(true),
            Some(Action::ScrollHalfPageUp) => app.scroll_detail_half_page(false),
            Some(Action::ScrollPageDown) => app.scroll_detail_page(true),
            Some(Action::ScrollPageUp) => app.scroll_detail_page(false),
            Some(Action::ScrollTop) => app.scroll_detail_top(),
            Some(Action::ScrollBottom) => app.scroll_detail_bottom(),
            Some(Action::NextTab) => app.next_phase(),
            Some(Action::PrevTab) => app.prev_phase(),
            // Quit can no longer be produced while the help overlay is up
            // (D-2, enforced in map_key -- see event.rs) -- it still simply
            // breaks the loop here.
            Some(Action::Quit) => break,
            Some(Action::ShowHelp) => app.show_help(),
            Some(Action::CloseHelp) => app.close_help(),
            None => {}
        }
    }
}
