//! `App` state (D-11/D-05): the bounded, upsert-by-run-id view-model the
//! render loop (05-07 main.rs) drains WS `ActivityEvent`s into, and `ui.rs`
//! renders from. A SINGLE `ingest` chokepoint is the only insertion path --
//! both the initial replay-burst and every subsequent live event route
//! through it, so the D-11 buffer bound (Pitfall 4) can never be bypassed
//! by a second bare `Vec::push` call site.

use ratatui::widgets::ListState;
use shared::{ActivityEvent, ActivityLogEvent, ActivityStatus};

/// Client-side view-model derived from a wire `ActivityEvent` -- the shape
/// `ui.rs` renders one list row (and, if open, one detail tab) from.
#[derive(Debug, Clone)]
pub struct Activity {
    pub run_id: String,
    pub workflow_id: String,
    pub client_name: String,
    pub status: ActivityStatus,
    pub started_at_ms: u64,
    pub log: Vec<ActivityLogEvent>,
}

impl From<ActivityEvent> for Activity {
    fn from(event: ActivityEvent) -> Self {
        Activity {
            run_id: event.run_id,
            workflow_id: event.workflow_id,
            client_name: event.client_name,
            status: event.status,
            started_at_ms: event.started_at_ms,
            log: event.log,
        }
    }
}

/// Which panel currently owns keyboard focus (Task 1 D-1): `map_key` routes
/// `j`/`k`/`g`/`G` (and, from Task 2, the detail-scroll keys) differently
/// depending on this, so the same physical keys mean "move the list
/// selection" while `Activities` is focused and "scroll the detail panel"
/// while `Detail` is focused. A fresh `App` starts focused on `Activities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Activities,
    Detail,
}

/// The TUI's bounded client-side state (D-11): a `Vec<Activity>` capped at
/// `max` entries (oldest evicted first), a `ListState` for `j`/`k`
/// navigation, and two DELIBERATELY SEPARATE fields (260816-ems D-1) --
/// re-scoped from the prior cross-activity history-list-plus-pointer shape
/// down to a single open activity: `active_run_id` is the ONE activity that
/// is currently open (set by `Enter`, `None` before anything has been
/// opened -- there is no cross-activity tab history any more); `current_phase`
/// is an index into THAT activity's own `log`, i.e. which lifecycle-phase
/// tab is selected. Two fields, one job each -- `ui.rs` resolves both
/// through `App::active_run_id`/`App::current_phase`, never a cross-activity
/// history list. `user_has_selected` (260816-fsz D-1) answers exactly one
/// question -- has the user ever explicitly chosen a row? -- distinguishing
/// "nobody has looked at anything yet, so the newest is the only sensible
/// default" from "the user chose this row, keep it under them"; it gates
/// which of those two rules `restore_selection` applies.
pub struct App {
    activities: Vec<Activity>,
    list_state: ListState,
    user_has_selected: bool,
    active_run_id: Option<String>,
    current_phase: usize,
    max: usize,
    help_open: bool,
    focus: Focus,
    detail_scroll: u16,
    detail_content_rows: usize,
    detail_viewport_rows: u16,
}

impl App {
    pub fn new(max: usize) -> Self {
        App {
            activities: Vec::new(),
            list_state: ListState::default(),
            user_has_selected: false,
            active_run_id: None,
            current_phase: 0,
            max,
            help_open: false,
            focus: Focus::Activities,
            detail_scroll: 0,
            detail_content_rows: 0,
            detail_viewport_rows: 0,
        }
    }

    /// Whether the `?` help overlay (D-2) is currently shown.
    pub fn help_open(&self) -> bool {
        self.help_open
    }

    /// Opens the help overlay.
    pub fn show_help(&mut self) {
        self.help_open = true;
    }

    /// Closes the help overlay.
    pub fn close_help(&mut self) {
        self.help_open = false;
    }

    /// Which panel currently has keyboard focus (Task 1 D-1).
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Moves keyboard focus to the Activities list, WITHOUT closing any open
    /// detail tab (D-1's "focus, not visibility" distinction) -- `Esc`'s
    /// whole job is to hand keyboard control back while leaving the tab on
    /// screen.
    pub fn focus_activities(&mut self) {
        self.focus = Focus::Activities;
    }

    /// Moves keyboard focus to the Detail panel.
    pub fn focus_detail(&mut self) {
        self.focus = Focus::Detail;
    }

    pub fn activities(&self) -> &[Activity] {
        &self.activities
    }

    pub fn list_state(&self) -> &ListState {
        &self.list_state
    }

    /// The single activity currently open (260816-ems D-1) -- `None` before
    /// any activity has ever been opened. There is no cross-activity tab
    /// history any more; this is the whole of it.
    pub fn active_run_id(&self) -> Option<&str> {
        self.active_run_id.as_deref()
    }

    /// The index into the open activity's own `log` that is currently
    /// selected (260816-ems D-1) -- the single source of truth `ui.rs`
    /// consults for both the panel's content and the `Tabs` bar's
    /// highlight (D-3). Only meaningful alongside `Some(active_run_id())`;
    /// a fresh `App` starts at 0.
    pub fn current_phase(&self) -> usize {
        self.current_phase
    }

    pub fn selected(&self) -> Option<&Activity> {
        self.list_state
            .selected()
            .and_then(|i| self.activities.get(i))
    }

    /// SINGLE insertion chokepoint (Pitfall 4): every `ActivityEvent`, from
    /// either the initial replay burst or a subsequent live push, must come
    /// through here -- never a second bare `Vec::push` call site.
    ///
    /// Upserts by `run_id` (D-05 -- one activity per run_id, update status
    /// and log in place rather than duplicating a row). `self.activities` is
    /// always kept sorted descending by `started_at_ms` (D-1) -- there is no
    /// separate render-time sort, so display order and storage order (the
    /// index `ListState`/`select_next`/`select_prev`/`open_selected_activity`
    /// all index by) can never disagree.
    ///
    /// Because `started_at_ms` is part of the sort key, an upsert never
    /// mutates through `iter_mut` -- a re-sent event carrying a different
    /// start time would leave the Vec unsorted while looking correct.
    /// Instead this removes the existing entry (if any) and re-inserts at
    /// the correct sorted position. Eviction (D-11) `truncate`s the tail
    /// instead of `remove(0)`-ing the head, because sorted-descending puts
    /// the oldest entry last -- the bound itself (never exceed `max`, always
    /// drop the oldest by time) is unchanged, only which end it lives at.
    ///
    /// After every insert this calls `restore_selection`, which normally
    /// re-anchors the selection by `run_id` (tracking whatever activity was
    /// selected before this ingest to its new index). 260816-fsz D-2: before
    /// the user's first explicit selection, that re-anchoring is bypassed
    /// entirely in favor of pinning the highlight to the newest entry -- a
    /// reader of `ingest` should not have to open `restore_selection` to
    /// learn that the replay burst does not move the highlight around.
    pub fn ingest(&mut self, event: ActivityEvent) {
        let selected_run_id = self.selected().map(|a| a.run_id.clone());

        if let Some(pos) = self.activities.iter().position(|a| a.run_id == event.run_id) {
            self.activities.remove(pos);
        }

        let new_activity = Activity::from(event);
        // Descending Vec: find the first entry that is NOT newer than the
        // new activity, i.e. partition on "started_at_ms > new.started_at_ms".
        // Ties place the newest-ingested entry first among equals.
        let insert_at = self
            .activities
            .partition_point(|a| a.started_at_ms > new_activity.started_at_ms);
        self.activities.insert(insert_at, new_activity);

        self.activities.truncate(self.max);

        self.restore_selection(selected_run_id);
        self.clamp_current_phase();
    }

    /// Re-anchors the selection after a mutation that may have reordered or
    /// evicted entries (260816-fsz D-2/D-3). While the user has never
    /// explicitly selected anything (`user_has_selected` is `false`), this
    /// pins to the top (newest) entry on EVERY call, ignoring the passed
    /// `run_id` entirely -- the daemon replays history as an unordered
    /// burst, so whichever run happened to arrive first is not a meaningful
    /// thing to track (that was the bug). Once the user has explicitly
    /// selected a row, the run_id-tracking-then-clamping path below takes
    /// over and this function behaves as it always has: if `run_id` is
    /// still present, selects its new index; if it was evicted, clamps the
    /// existing index to the last valid one.
    ///
    /// The empty-list check must run FIRST, ahead of the pin -- otherwise
    /// `select(Some(0))` could name a row that does not exist on an empty
    /// list (T-fsz-01).
    fn restore_selection(&mut self, run_id: Option<String>) {
        if self.activities.is_empty() {
            self.list_state.select(None);
            return;
        }

        if !self.user_has_selected {
            self.list_state.select(Some(0));
            return;
        }

        if let Some(run_id) = run_id {
            if let Some(pos) = self.activities.iter().position(|a| a.run_id == run_id) {
                self.list_state.select(Some(pos));
                return;
            }
        }

        let clamped = self
            .list_state
            .selected()
            .map_or(0, |current| current.min(self.activities.len() - 1));
        self.list_state.select(Some(clamped));
    }

    /// Moves the selection forward, clamping at the last index. On an empty
    /// activity list, leaves the selection at `None` rather than selecting
    /// a phantom row. Calling this hands selection control to the user
    /// permanently (260816-fsz D-5) -- including a clamped or empty-list
    /// no-op.
    pub fn select_next(&mut self) {
        self.user_has_selected = true;
        if self.activities.is_empty() {
            self.list_state.select(None);
            return;
        }
        let last = self.activities.len() - 1;
        let i = self
            .list_state
            .selected()
            .map_or(0, |i| (i + 1).min(last));
        self.list_state.select(Some(i));
    }

    /// Moves the selection backward, saturating at index 0. On an empty
    /// activity list, leaves the selection at `None`. Calling this hands
    /// selection control to the user permanently (260816-fsz D-5) --
    /// including a clamped or empty-list no-op.
    pub fn select_prev(&mut self) {
        self.user_has_selected = true;
        if self.activities.is_empty() {
            self.list_state.select(None);
            return;
        }
        let i = self.list_state.selected().map_or(0, |i| i.saturating_sub(1));
        self.list_state.select(Some(i));
    }

    /// Jumps the selection straight to the first activity. On an empty
    /// list, leaves the selection at `None` rather than selecting a
    /// phantom row -- same empty-list discipline as `select_next`/
    /// `select_prev`. Calling this hands selection control to the user
    /// permanently (260816-fsz D-5) -- including a clamped or empty-list
    /// no-op.
    pub fn select_first(&mut self) {
        self.user_has_selected = true;
        if self.activities.is_empty() {
            self.list_state.select(None);
            return;
        }
        self.list_state.select(Some(0));
    }

    /// Jumps the selection straight to the last activity. On an empty
    /// list, leaves the selection at `None`. Calling this hands selection
    /// control to the user permanently (260816-fsz D-5) -- including a
    /// clamped or empty-list no-op.
    pub fn select_last(&mut self) {
        self.user_has_selected = true;
        if self.activities.is_empty() {
            self.list_state.select(None);
            return;
        }
        self.list_state.select(Some(self.activities.len() - 1));
    }

    /// Opens the currently-selected activity as the one open activity and
    /// moves keyboard focus to it (260816-ems D-1/D-2) -- including when it
    /// was already open, so pressing `Enter` on the already-open activity
    /// still moves focus rather than doing nothing visible.
    ///
    /// Per D-2: opening a DIFFERENT activity defaults `current_phase` to its
    /// last `log` entry (`log.len().saturating_sub(1)`) -- the
    /// most-recently-added, typically terminal phase, which is what a user
    /// opening a finished run wants to read first. Re-opening the activity
    /// that is ALREADY active does NOT re-derive that default: it keeps
    /// `current_phase` (and, via `set_displayed`, `detail_scroll`) exactly
    /// where the user left it -- pressing `Enter` on what you are already
    /// reading is a re-focus, not a reset.
    ///
    /// Reads everything needed off the borrowed `Activity` into owned/`Copy`
    /// locals before calling `set_displayed`, which needs `&mut self`.
    ///
    /// Also hands selection control to the user permanently (260816-fsz
    /// D-4): opening an activity is a deliberate act aimed at one specific
    /// run, so once it happens, later arrivals must not slide the list
    /// highlight onto a different run underneath the reader. Set inside
    /// this `if let` branch specifically, so `Enter` with nothing selected
    /// opens nothing and claims nothing.
    pub fn open_selected_activity(&mut self) {
        if let Some(activity) = self.selected() {
            let run_id = activity.run_id.clone();
            let last_phase = activity.log.len().saturating_sub(1);
            let is_reopen = self.active_run_id.as_deref() == Some(run_id.as_str());
            let phase = if is_reopen { self.current_phase } else { last_phase };
            self.user_has_selected = true;
            self.set_displayed(run_id, phase);
            self.focus_detail();
        }
    }

    /// Makes `(run_id, phase)` the displayed pair, resetting `detail_scroll`
    /// to 0 exactly when EITHER component differs from what was previously
    /// displayed (260816-ems D-5) -- replaces quick task 260815-rx6's
    /// display-setter at the new per-activity-phase scope, keeping the
    /// same guarantee: this is the SINGLE place the old-vs-new comparison
    /// and the scroll reset happen, so `open_selected_activity` and
    /// `cycle_phase` (Task 2) can never disagree on what "the display
    /// changed" means.
    ///
    /// The comparison MUST happen against the OLD `(active_run_id,
    /// current_phase)`, before either is overwritten -- assigning first and
    /// trying to recover the previous values would lose exactly the thing
    /// being compared (the same trap 260815-rx6 documented at the old
    /// scope).
    ///
    /// Nothing else assigns `active_run_id`/`current_phase` except
    /// `clamp_current_phase` (Task 2 D-4), which is explicitly outside this
    /// rule and says so in its own doc comment.
    fn set_displayed(&mut self, run_id: String, phase: usize) {
        let displayed_is_changing =
            self.active_run_id.as_deref() != Some(run_id.as_str()) || self.current_phase != phase;
        if displayed_is_changing {
            self.detail_scroll = 0;
        }
        self.active_run_id = Some(run_id);
        self.current_phase = phase;
    }

    /// Moves `current_phase` forward through the open activity's own `log`,
    /// wrapping from the last entry back to the first (260816-ems Task 2).
    /// A silent no-op with fewer than two phases, with nothing open, or
    /// with the open activity evicted -- see `cycle_phase`.
    pub fn next_phase(&mut self) {
        self.cycle_phase(true);
    }

    /// Moves `current_phase` backward through the open activity's own
    /// `log`, wrapping from the first entry back to the last (260816-ems
    /// Task 2). A silent no-op with fewer than two phases, with nothing
    /// open, or with the open activity evicted -- see `cycle_phase`.
    pub fn prev_phase(&mut self) {
        self.cycle_phase(false);
    }

    /// Pure pointer movement over the open activity's own `log` (D-1/D-5):
    /// `forward` selects the direction, mirroring `scroll_detail_half_page
    /// (down)`/`scroll_detail_page(down)`'s existing `bool`-direction shape
    /// rather than inventing a new convention.
    ///
    /// Backward is expressed as a forward step of `len - 1` rather than
    /// signed arithmetic with `rem_euclid`: with `len` the phase count and
    /// `pos` the current phase's index, going back one is equivalent to
    /// going forward `len - 1`, so both directions share one `% len` and
    /// neither can underflow. That subtraction is safe only because of the
    /// guard below -- `len < 2` returns before `len - 1` is ever computed,
    /// so `len - 1` is always at least 1 by the time it runs.
    ///
    /// Three early returns:
    /// - No `active_run_id` -- nothing open to cycle.
    /// - The active `run_id` is not present in `activities` -- evicted
    ///   (D-11); silence beats a confident wrong answer, exactly as the
    ///   prior tab-cycling implementation argued.
    /// - Fewer than two phases in the open activity's `log` -- nothing to
    ///   cycle to (covers both zero and one entry; a one-entry self-cycle
    ///   would happen to preserve the scroll only because the compared
    ///   values are equal, which is a coincidence not worth relying on).
    ///
    /// Clamps the current index with `.min(len - 1)` before stepping (D-4's
    /// totality rule) -- defensive against a stale `current_phase` reading
    /// larger than the just-resolved `len`, even though `ingest`'s own
    /// `clamp_current_phase` should already keep it in range.
    ///
    /// Never touches `focus` or `active_run_id` -- cycling moves only the
    /// `current_phase` pointer, via `set_displayed` (so the scroll-reset
    /// rule is shared with `open_selected_activity`, D-5).
    fn cycle_phase(&mut self, forward: bool) {
        let Some(run_id) = self.active_run_id.clone() else {
            return;
        };
        let Some(activity) = self.activities.iter().find(|a| a.run_id == run_id) else {
            return;
        };
        let len = activity.log.len();
        if len < 2 {
            return;
        }
        let pos = self.current_phase.min(len - 1);
        let step = if forward { 1 } else { len - 1 };
        let destination = (pos + step) % len;
        self.set_displayed(run_id, destination);
    }

    /// Re-anchors `current_phase` into range after a mutation that may have
    /// shrunk the OPEN activity's `log` (260816-ems Task 2 D-4) -- called
    /// from `ingest`, right beside the existing `restore_selection` call,
    /// so the two re-anchor-after-mutation steps sit together.
    ///
    /// Three things this deliberately does NOT do:
    /// - It does NOT advance `current_phase` when the log GROWS -- the user
    ///   stays on the entry they were reading; the new phase simply becomes
    ///   reachable as an additional tab. Auto-tracking would yank the view
    ///   mid-read.
    /// - It NEVER touches `detail_scroll` -- this is a defensive bounds
    ///   correction, not user navigation. Routing it through `set_displayed`
    ///   would risk resetting the scroll on ordinary live traffic.
    /// - It is the ONE assignment to `current_phase` that deliberately
    ///   bypasses `set_displayed` -- `set_displayed` owns the scroll-reset
    ///   rule for user-driven navigation (D-5); this is not that.
    ///
    /// No-op when there is no open activity or it is not present in
    /// `activities` (evicted) -- nothing to clamp.
    fn clamp_current_phase(&mut self) {
        let Some(run_id) = self.active_run_id.as_deref() else {
            return;
        };
        let Some(activity) = self.activities.iter().find(|a| a.run_id == run_id) else {
            return;
        };
        let max_index = activity.log.len().saturating_sub(1);
        if self.current_phase > max_index {
            self.current_phase = max_index;
        }
    }

    /// Current detail-panel scroll offset, in rendered wrapped rows (Task 2
    /// D-3/D-4) -- 0 is the top.
    pub fn detail_scroll(&self) -> u16 {
        self.detail_scroll
    }

    /// Records what the LAST render pass measured for the open detail panel
    /// (Task 2 D-4): `content_rows` is the wrapped-row count of the panel's
    /// content (already corrected for the block's border rows -- see
    /// `ui.rs`'s `render_detail_panel`), `viewport_rows` is the panel's
    /// inner height. `map_key`/`App` cannot know either value themselves --
    /// only the render pass, which runs the real `Paragraph` wrapper, can.
    /// `run_loop` redraws before every `poll_action`, so these are never
    /// stale by more than the current frame.
    ///
    /// Re-clamps the stored offset to the new maximum every call, so a
    /// terminal resize that shrinks the content self-corrects on the next
    /// frame rather than costing the user a wasted keypress.
    pub fn record_detail_metrics(&mut self, content_rows: usize, viewport_rows: u16) {
        self.detail_content_rows = content_rows;
        self.detail_viewport_rows = viewport_rows;
        self.detail_scroll = self.detail_scroll.min(self.max_detail_scroll());
    }

    /// The furthest the detail panel can scroll down: `content_rows -
    /// viewport_rows`, saturating at 0 when content fits entirely within
    /// the viewport. Every scroll method is built on this so none of them
    /// can invent its own bound (Task 2).
    fn max_detail_scroll(&self) -> u16 {
        let max = self.detail_content_rows.saturating_sub(self.detail_viewport_rows as usize);
        u16::try_from(max).unwrap_or(u16::MAX)
    }

    /// Scrolls the detail panel by `delta` rendered rows (positive = down,
    /// negative = up), clamping at 0 and at `max_detail_scroll()` in both
    /// directions -- never over/underflows.
    pub fn scroll_detail_lines(&mut self, delta: i32) {
        let current = i32::from(self.detail_scroll);
        let max = i32::from(self.max_detail_scroll());
        let next = (current + delta).clamp(0, max);
        self.detail_scroll = next as u16;
    }

    /// Scrolls the detail panel by half the recorded viewport height
    /// (`.max(1)` so a one-row viewport still advances). `down` selects the
    /// direction.
    pub fn scroll_detail_half_page(&mut self, down: bool) {
        let step = (self.detail_viewport_rows / 2).max(1);
        let delta = if down { i32::from(step) } else { -i32::from(step) };
        self.scroll_detail_lines(delta);
    }

    /// Scrolls the detail panel by a full recorded viewport height
    /// (`.max(1)` so a one-row viewport still advances). `down` selects the
    /// direction.
    pub fn scroll_detail_page(&mut self, down: bool) {
        let step = self.detail_viewport_rows.max(1);
        let delta = if down { i32::from(step) } else { -i32::from(step) };
        self.scroll_detail_lines(delta);
    }

    /// Jumps the detail panel scroll to the very top (offset 0).
    pub fn scroll_detail_top(&mut self) {
        self.detail_scroll = 0;
    }

    /// Jumps the detail panel scroll to the very bottom
    /// (`max_detail_scroll()`).
    pub fn scroll_detail_bottom(&mut self) {
        self.detail_scroll = self.max_detail_scroll();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::ActivityPhase;

    fn event(run_id: &str, status: ActivityStatus, started_at_ms: u64) -> ActivityEvent {
        ActivityEvent {
            run_id: run_id.to_string(),
            workflow_id: "set_timer".to_string(),
            client_name: "orchestrator-cli".to_string(),
            status,
            started_at_ms,
            log: vec![ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: started_at_ms,
                detail: None,
            }],
        }
    }

    #[test]
    fn ingest_evicts_oldest_beyond_max_on_repeated_new_run_ids() {
        let mut app = App::new(2);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-2", ActivityStatus::NotStarted, 200));
        app.ingest(event("run-3", ActivityStatus::NotStarted, 300));

        assert_eq!(app.activities().len(), 2, "buffer must never exceed max (D-11)");
        let run_ids: Vec<&str> = app.activities().iter().map(|a| a.run_id.as_str()).collect();
        // D-1: activities() is now sorted descending by started_at_ms, so the
        // surviving entries sit newest-first. The D-11 rule this test
        // protects -- oldest (run-1) evicted first -- is unchanged; only the
        // order the survivors are found in has moved.
        assert_eq!(
            run_ids,
            vec!["run-3", "run-2"],
            "oldest (run-1) must be evicted first (D-11)"
        );
    }

    #[test]
    fn first_ingest_into_a_fresh_app_selects_the_top_activity() {
        let mut app = App::new(10);
        assert_eq!(app.list_state().selected(), None, "a fresh app has nothing to select yet");

        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));

        assert_eq!(
            app.list_state().selected(),
            Some(0),
            "the first activity to ever arrive must be auto-selected so Enter works immediately"
        );
    }

    #[test]
    fn ingest_same_run_id_twice_upserts_instead_of_duplicating() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-1", ActivityStatus::Success, 100));

        assert_eq!(
            app.activities().len(),
            1,
            "same run_id ingested twice must yield one row (D-05)"
        );
        assert_eq!(app.activities()[0].status, ActivityStatus::Success);
    }

    #[test]
    fn ingest_replay_burst_then_live_events_both_respect_bound() {
        // Simulates the replay-burst path (many events ingested in a tight
        // loop) followed by the live path (one at a time) -- both must
        // route through the same bounded chokepoint (Pitfall 4).
        let mut app = App::new(3);
        for i in 0..5 {
            app.ingest(event(&format!("replay-{i}"), ActivityStatus::Success, i as u64));
        }
        assert_eq!(app.activities().len(), 3);

        app.ingest(event("live-1", ActivityStatus::Running, 500));
        assert_eq!(app.activities().len(), 3, "live path must also respect max (D-11)");
        let run_ids: Vec<&str> = app.activities().iter().map(|a| a.run_id.as_str()).collect();
        // D-1: activities() is now sorted descending by started_at_ms, so the
        // surviving entries sit newest-first. The D-11 rule this test
        // protects is unchanged; only the survivors' order has moved.
        assert_eq!(run_ids, vec!["live-1", "replay-4", "replay-3"]);
    }

    #[test]
    fn select_next_and_prev_move_within_bounds() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-2", ActivityStatus::NotStarted, 200));
        app.ingest(event("run-3", ActivityStatus::NotStarted, 300));

        // Establish a known baseline explicitly -- the pin (260816-fsz)
        // already leaves index 0 selected here, so this call doesn't move
        // the selection; it's kept both as an explicit baseline and because
        // it flips user_has_selected, so this test exercises only
        // select_next/select_prev's clamping behavior and never the pin.
        app.select_first();
        app.select_next();
        app.select_next();
        app.select_next(); // one past the end -- must clamp, not overflow
        assert_eq!(
            app.list_state().selected(),
            Some(2),
            "select_next must clamp at the last index"
        );

        app.select_prev();
        app.select_prev();
        app.select_prev();
        app.select_prev(); // one past the start -- must clamp, not underflow
        assert_eq!(
            app.list_state().selected(),
            Some(0),
            "select_prev must clamp at index 0"
        );
    }

    #[test]
    fn select_next_on_empty_list_does_not_select_a_phantom_row() {
        let mut app = App::new(10);
        app.select_next();
        assert_eq!(
            app.list_state().selected(),
            None,
            "selecting on an empty activity list must not select anything"
        );
    }

    #[test]
    fn ingest_keeps_activities_newest_first() {
        let mut app = App::new(10);
        app.ingest(event("run-100", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-300", ActivityStatus::NotStarted, 300));
        app.ingest(event("run-200", ActivityStatus::NotStarted, 200));

        let run_ids: Vec<&str> = app.activities().iter().map(|a| a.run_id.as_str()).collect();
        assert_eq!(
            run_ids,
            vec!["run-300", "run-200", "run-100"],
            "activities() must always be sorted descending by started_at_ms (D-1)"
        );
    }

    #[test]
    fn ingest_evicts_the_oldest_even_when_it_arrives_last() {
        let mut app = App::new(2);
        app.ingest(event("run-300", ActivityStatus::NotStarted, 300));
        app.ingest(event("run-200", ActivityStatus::NotStarted, 200));
        app.ingest(event("run-100", ActivityStatus::NotStarted, 100));

        assert_eq!(app.activities().len(), 2, "buffer must never exceed max (D-11)");
        let run_ids: Vec<&str> = app.activities().iter().map(|a| a.run_id.as_str()).collect();
        assert!(
            !run_ids.contains(&"run-100"),
            "the oldest entry by started_at_ms must be evicted, not the last-arrived: {run_ids:?}"
        );
    }

    #[test]
    fn selection_follows_its_activity_when_a_newer_one_arrives() {
        let mut app = App::new(10);
        app.ingest(event("run-old", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-new", ActivityStatus::NotStarted, 300));

        // The pin (260816-fsz) already leaves "run-new" (the newest)
        // selected at this point. The select_first() call below is
        // load-bearing for a different reason under the pin rule: it flips
        // user_has_selected and hands this test to the run_id-tracking path
        // it exists to exercise. Without it, restore_selection would keep
        // re-pinning to index 0 on the next ingest and the final assertion
        // below would fail -- which also makes this test the de-facto
        // coverage for select_first flipping the flag.
        app.select_first();
        assert_eq!(app.selected().unwrap().run_id, "run-new");

        app.ingest(event("run-newest", ActivityStatus::NotStarted, 500));

        assert_eq!(
            app.selected().unwrap().run_id,
            "run-new",
            "the highlight must track the activity, not the slot"
        );
        assert_eq!(app.list_state().selected(), Some(1));
    }

    #[test]
    fn selected_always_matches_the_activity_at_the_selected_index() {
        let mut app = App::new(10);
        app.ingest(event("run-old", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-new", ActivityStatus::NotStarted, 300));
        app.select_next();
        app.ingest(event("run-newest", ActivityStatus::NotStarted, 500));

        let idx = app.list_state().selected().unwrap();
        assert_eq!(
            app.selected().unwrap().run_id,
            app.activities()[idx].run_id,
            "selected() must always match the activity at the selected index"
        );
    }

    #[test]
    fn a_new_app_starts_with_help_closed() {
        let app = App::new(10);
        assert!(!app.help_open(), "help overlay must start closed");
    }

    #[test]
    fn a_new_app_starts_focused_on_activities() {
        let app = App::new(10);
        assert_eq!(
            app.focus(),
            Focus::Activities,
            "a new app must start focused on Activities"
        );
    }

    #[test]
    fn select_first_and_select_last_jump_to_the_ends() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-2", ActivityStatus::NotStarted, 200));
        app.ingest(event("run-3", ActivityStatus::NotStarted, 300));

        app.select_last();
        assert_eq!(app.list_state().selected(), Some(2), "select_last must select the last index");

        app.select_first();
        assert_eq!(app.list_state().selected(), Some(0), "select_first must select the first index");
    }

    #[test]
    fn select_first_and_last_on_an_empty_list_select_nothing() {
        let mut app = App::new(10);

        app.select_first();
        assert_eq!(app.list_state().selected(), None, "select_first on an empty list must not select anything");

        app.select_last();
        assert_eq!(app.list_state().selected(), None, "select_last on an empty list must not select anything");
    }

    #[test]
    fn open_selected_activity_moves_focus_to_the_detail_panel() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.select_next();

        app.open_selected_activity();

        assert_eq!(app.focus(), Focus::Detail, "opening an activity must move focus to the detail panel");
    }

    #[test]
    fn focus_activities_returns_focus_without_closing_the_activity() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.select_next();
        app.open_selected_activity();

        app.focus_activities();

        assert_eq!(app.focus(), Focus::Activities, "focus_activities must return focus to Activities");
        assert_eq!(
            app.active_run_id(),
            Some("run-1"),
            "returning focus to Activities must NOT close the open activity"
        );
    }

    #[test]
    fn show_help_then_close_help_flips_the_overlay_flag() {
        let mut app = App::new(10);
        assert!(!app.help_open());

        app.show_help();
        assert!(app.help_open(), "show_help must open the overlay");

        app.close_help();
        assert!(!app.help_open(), "close_help must close the overlay");
    }

    #[test]
    fn a_new_app_starts_scrolled_to_the_top() {
        let app = App::new(10);
        assert_eq!(app.detail_scroll(), 0, "a new app must start scrolled to the top");
    }

    #[test]
    fn scrolling_down_stops_at_the_last_row() {
        let mut app = App::new(10);
        app.record_detail_metrics(20, 5); // max scroll = 20 - 5 = 15

        for _ in 0..20 {
            app.scroll_detail_lines(1);
        }

        assert_eq!(app.detail_scroll(), 15, "scrolling down must clamp at content - viewport, never past it");
    }

    #[test]
    fn scrolling_up_stops_at_the_top() {
        let mut app = App::new(10);
        app.record_detail_metrics(20, 5);
        for _ in 0..20 {
            app.scroll_detail_lines(1);
        }
        assert_eq!(app.detail_scroll(), 15);

        for _ in 0..20 {
            app.scroll_detail_lines(-1);
        }

        assert_eq!(app.detail_scroll(), 0, "scrolling up must clamp at 0 with no underflow");
    }

    #[test]
    fn half_page_and_page_use_the_recorded_viewport_height() {
        let mut app = App::new(10);
        app.record_detail_metrics(100, 10);

        app.scroll_detail_half_page(true);
        assert_eq!(app.detail_scroll(), 5, "half page must be half the recorded viewport height");

        app.scroll_detail_page(true);
        assert_eq!(app.detail_scroll(), 15, "a full page must advance by the recorded viewport height");
    }

    #[test]
    fn page_scroll_still_advances_when_the_viewport_is_one_row() {
        let mut app = App::new(10);
        app.record_detail_metrics(100, 1);

        app.scroll_detail_page(true);

        assert!(
            app.detail_scroll() >= 1,
            "a page scroll must advance by at least 1 even at a one-row viewport"
        );
    }

    #[test]
    fn scroll_to_bottom_and_top_hit_the_exact_bounds() {
        let mut app = App::new(10);
        app.record_detail_metrics(50, 10);

        app.scroll_detail_bottom();
        assert_eq!(app.detail_scroll(), 40, "scroll_detail_bottom must land exactly on content_rows - viewport_rows");

        app.scroll_detail_top();
        assert_eq!(app.detail_scroll(), 0, "scroll_detail_top must land exactly on 0");
    }

    #[test]
    fn content_shorter_than_the_viewport_cannot_scroll() {
        let mut app = App::new(10);
        app.record_detail_metrics(3, 10);

        app.scroll_detail_lines(1);
        assert_eq!(app.detail_scroll(), 0);
        app.scroll_detail_half_page(true);
        assert_eq!(app.detail_scroll(), 0);
        app.scroll_detail_page(true);
        assert_eq!(app.detail_scroll(), 0);
        app.scroll_detail_bottom();
        assert_eq!(
            app.detail_scroll(),
            0,
            "content shorter than the viewport must never scroll"
        );
    }

    #[test]
    fn recording_smaller_metrics_reclamps_an_existing_offset() {
        let mut app = App::new(10);
        app.record_detail_metrics(100, 10);
        app.scroll_detail_bottom();
        assert_eq!(app.detail_scroll(), 90);

        // Simulate a terminal resize that shrinks the content (D-4).
        app.record_detail_metrics(20, 10);

        assert_eq!(
            app.detail_scroll(),
            10,
            "recording smaller metrics must reclamp an existing offset down to the new maximum"
        );
    }

    // -- 260816-ems Task 1: new per-activity-phase-tab model (RED) --------

    #[test]
    fn a_new_app_has_no_active_activity() {
        let app = App::new(10);
        assert_eq!(app.active_run_id(), None, "a new app must have no active activity");
        assert_eq!(app.current_phase(), 0, "a new app's current_phase must start at 0");
    }

    #[test]
    fn opening_an_activity_sets_the_active_run_id() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.select_next();

        app.open_selected_activity();

        assert_eq!(
            app.active_run_id(),
            Some("run-1"),
            "opening an activity must set active_run_id to its run_id"
        );
    }

    #[test]
    fn opening_an_activity_starts_at_its_last_log_phase() {
        let mut app = App::new(10);
        let mut ev = event("run-1", ActivityStatus::Success, 100);
        ev.log = vec![
            ActivityLogEvent { phase: ActivityPhase::Invoked, at_ms: 100, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Running, at_ms: 150, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Completed, at_ms: 200, detail: None },
        ];
        app.ingest(ev);
        app.select_next();

        app.open_selected_activity();

        assert_eq!(
            app.current_phase(),
            2,
            "opening a 3-entry log must default to the last phase (index 2)"
        );
    }

    #[test]
    fn opening_an_activity_with_a_single_phase_starts_at_zero() {
        let mut app = App::new(10);
        // event() produces a single-entry log.
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.select_next();

        app.open_selected_activity();

        assert_eq!(app.current_phase(), 0, "a single-entry log must start at phase 0");
    }

    #[test]
    fn switching_to_a_different_activity_resets_the_phase_and_the_scroll() {
        let mut app = App::new(10);
        let mut ev1 = event("run-1", ActivityStatus::Success, 100);
        ev1.log = vec![
            ActivityLogEvent { phase: ActivityPhase::Invoked, at_ms: 100, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Running, at_ms: 150, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Completed, at_ms: 200, detail: None },
        ];
        app.ingest(ev1);
        let mut ev2 = event("run-2", ActivityStatus::Running, 200);
        ev2.log = vec![
            ActivityLogEvent { phase: ActivityPhase::Invoked, at_ms: 200, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Running, at_ms: 250, detail: None },
        ];
        app.ingest(ev2);

        // Descending order: run-2 (started 200) sorts before run-1 (started 100).
        app.select_next();
        app.select_next();
        assert_eq!(app.selected().unwrap().run_id, "run-1");
        app.open_selected_activity();
        app.record_detail_metrics(100, 10);
        app.scroll_detail_bottom();
        assert_eq!(app.detail_scroll(), 90);

        app.select_prev();
        assert_eq!(app.selected().unwrap().run_id, "run-2");
        app.open_selected_activity();

        assert_eq!(app.active_run_id(), Some("run-2"));
        assert_eq!(
            app.current_phase(),
            1,
            "run-2's own last phase (1), NOT carried over from run-1's 2"
        );
        assert_eq!(app.detail_scroll(), 0);
    }

    #[test]
    fn reopening_the_already_open_activity_preserves_its_phase_and_scroll() {
        let mut app = App::new(10);
        let mut ev = event("run-1", ActivityStatus::Success, 100);
        ev.log = vec![
            ActivityLogEvent { phase: ActivityPhase::Invoked, at_ms: 100, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Running, at_ms: 150, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Completed, at_ms: 200, detail: None },
        ];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();
        assert_eq!(app.current_phase(), 2);
        app.record_detail_metrics(100, 10);
        app.scroll_detail_bottom();
        assert_eq!(app.detail_scroll(), 90);

        // Press open again on the SAME activity.
        app.open_selected_activity();

        assert_eq!(
            app.current_phase(),
            2,
            "reopening the already-open activity must preserve current_phase"
        );
        assert_eq!(
            app.detail_scroll(),
            90,
            "reopening the already-open activity must preserve detail_scroll"
        );
    }

    #[test]
    fn reopening_a_previously_visited_activity_switches_back_to_it() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-2", ActivityStatus::NotStarted, 200));

        app.select_next(); // run-2 (newer, index 0)
        app.open_selected_activity();
        app.select_next(); // run-1 (index 1)
        app.open_selected_activity();

        app.select_prev(); // back to run-2
        app.open_selected_activity();

        assert_eq!(
            app.active_run_id(),
            Some("run-2"),
            "reopening a previously-visited activity must switch back to it"
        );
    }

    #[test]
    fn selection_clamps_when_the_selected_activity_is_evicted() {
        let mut app = App::new(2);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100));
        app.select_next();
        assert_eq!(app.selected().unwrap().run_id, "run-1");

        app.ingest(event("run-2", ActivityStatus::NotStarted, 200));
        app.ingest(event("run-3", ActivityStatus::NotStarted, 300));

        assert!(
            app.selected().is_some(),
            "selection must never dangle after the selected activity is evicted"
        );
        let idx = app.list_state().selected().unwrap();
        assert!(idx < app.activities().len(), "selected index must stay in range");
    }

    // -- 260816-ems Task 2: cycling phases + live log growth/shrink (RED) --

    fn three_phase_event(run_id: &str, started_at_ms: u64) -> ActivityEvent {
        let mut ev = event(run_id, ActivityStatus::Success, started_at_ms);
        ev.log = vec![
            ActivityLogEvent { phase: ActivityPhase::Invoked, at_ms: started_at_ms, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Running, at_ms: started_at_ms + 50, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Completed, at_ms: started_at_ms + 100, detail: None },
        ];
        ev
    }

    #[test]
    fn cycling_forward_walks_the_phases_and_wraps_to_the_first() {
        let mut app = App::new(10);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();
        assert_eq!(app.current_phase(), 2, "opening must start at the last phase");

        app.next_phase();
        assert_eq!(app.current_phase(), 0, "the first step from the last phase IS the wrap");

        app.next_phase();
        assert_eq!(app.current_phase(), 1);

        app.next_phase();
        assert_eq!(app.current_phase(), 2);
    }

    #[test]
    fn cycling_backward_walks_the_phases_and_wraps_to_the_last() {
        let mut app = App::new(10);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();
        app.next_phase(); // move off the default (2) to 0
        assert_eq!(app.current_phase(), 0);

        app.prev_phase();
        assert_eq!(app.current_phase(), 2, "prev_phase from the first phase must wrap to the last");

        app.prev_phase();
        assert_eq!(app.current_phase(), 1);

        app.prev_phase();
        assert_eq!(app.current_phase(), 0);
    }

    #[test]
    fn cycling_with_a_single_phase_log_changes_nothing() {
        let mut app = App::new(10);
        app.ingest(event("run-1", ActivityStatus::NotStarted, 100)); // single-entry log
        app.select_next();
        app.open_selected_activity();
        app.record_detail_metrics(100, 10);
        app.scroll_detail_bottom();
        assert_eq!(app.detail_scroll(), 90);

        app.next_phase();
        app.prev_phase();

        assert_eq!(app.current_phase(), 0, "a one-entry self-cycle must be a real no-op");
        assert_eq!(app.detail_scroll(), 90, "a one-entry self-cycle must not reset the scroll");
    }

    #[test]
    fn cycling_with_nothing_open_does_nothing() {
        let mut app = App::new(10);

        app.next_phase();
        app.prev_phase();

        assert_eq!(app.active_run_id(), None, "cycling with nothing open must not open anything");
        assert_eq!(app.current_phase(), 0, "cycling with nothing open must not move current_phase");
    }

    #[test]
    fn cycling_with_an_evicted_open_activity_does_nothing() {
        let mut app = App::new(1);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();
        assert_eq!(app.current_phase(), 2);

        app.ingest(event("run-2", ActivityStatus::NotStarted, 200)); // evicts run-1

        app.next_phase();
        app.prev_phase();

        assert_eq!(
            app.current_phase(),
            2,
            "cycling with an evicted open activity must not panic or move current_phase"
        );
    }

    #[test]
    fn cycling_to_a_different_phase_resets_the_scroll() {
        let mut app = App::new(10);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();
        app.record_detail_metrics(100, 10);
        app.scroll_detail_bottom();
        assert_eq!(app.detail_scroll(), 90);

        app.next_phase();

        assert_eq!(app.detail_scroll(), 0, "cycling to a different phase must reset the scroll");
    }

    #[test]
    fn cycling_never_changes_focus_or_the_open_activity() {
        let mut app = App::new(10);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();

        app.next_phase();
        app.next_phase();
        app.prev_phase();
        app.prev_phase();
        app.prev_phase();

        assert_eq!(app.focus(), Focus::Detail, "cycling must never change focus");
        assert_eq!(app.active_run_id(), Some("run-1"), "cycling must never change the open activity");
    }

    #[test]
    fn a_log_growing_under_the_open_activity_pins_the_current_phase() {
        let mut app = App::new(10);
        let mut ev = event("run-1", ActivityStatus::Running, 100);
        ev.log = vec![ActivityLogEvent { phase: ActivityPhase::Invoked, at_ms: 100, detail: None }];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();
        assert_eq!(app.current_phase(), 0);

        // The upsert path: same run_id and started_at_ms, a longer log.
        let mut grown = event("run-1", ActivityStatus::Success, 100);
        grown.log = vec![
            ActivityLogEvent { phase: ActivityPhase::Invoked, at_ms: 100, detail: None },
            ActivityLogEvent { phase: ActivityPhase::Completed, at_ms: 150, detail: None },
        ];
        app.ingest(grown);

        assert_eq!(
            app.current_phase(),
            0,
            "the view must not be yanked to the newly-arrived phase (D-4 growth pins)"
        );

        app.next_phase();
        assert_eq!(
            app.current_phase(),
            1,
            "the newly-arrived phase must be reachable by cycling"
        );
    }

    #[test]
    fn a_log_shrinking_under_the_open_activity_clamps_the_current_phase() {
        let mut app = App::new(10);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();
        assert_eq!(app.current_phase(), 2);

        let mut shrunk = event("run-1", ActivityStatus::Failure, 100);
        shrunk.log = vec![ActivityLogEvent { phase: ActivityPhase::Failed, at_ms: 100, detail: None }];
        app.ingest(shrunk);

        assert_eq!(app.current_phase(), 0, "a shrinking log must clamp current_phase into range");
    }

    #[test]
    fn clamping_after_a_shrink_does_not_reset_the_scroll() {
        let mut app = App::new(10);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();
        app.record_detail_metrics(100, 10);
        app.scroll_detail_bottom();
        assert_eq!(app.detail_scroll(), 90);

        let mut shrunk = event("run-1", ActivityStatus::Failure, 100);
        shrunk.log = vec![ActivityLogEvent { phase: ActivityPhase::Failed, at_ms: 100, detail: None }];
        app.ingest(shrunk);

        assert_eq!(
            app.detail_scroll(),
            90,
            "the shrink clamp is a bounds correction, not navigation -- it must not reset the scroll"
        );
    }

    #[test]
    fn ingesting_an_unrelated_activity_never_moves_the_current_phase() {
        let mut app = App::new(10);
        app.ingest(three_phase_event("run-1", 100));
        app.select_next();
        app.open_selected_activity();
        assert_eq!(app.current_phase(), 2);

        app.ingest(event("run-2", ActivityStatus::NotStarted, 200));

        assert_eq!(
            app.current_phase(),
            2,
            "an unrelated activity's arrival must not move current_phase"
        );
    }

    // -- 260816-fsz: default selection pins to the newest activity --------

    #[test]
    fn replay_burst_out_of_order_still_selects_the_newest_activity() {
        // The reported bug, stated directly: the daemon's replay burst does
        // NOT arrive pre-sorted by time. run-200 arriving FIRST is the whole
        // point of this fixture -- today's run_id-tracking code auto-selects
        // whichever activity happened to arrive first (run-200) and then
        // rides it, via restore_selection's run_id lookup, to wherever it
        // lands once the list is re-sorted newest-first -- index 2, not the
        // top. The fix must select the newest activity (run-500) regardless
        // of arrival order.
        let mut app = App::new(10);
        app.ingest(event("run-200", ActivityStatus::NotStarted, 200));
        app.ingest(event("run-500", ActivityStatus::NotStarted, 500));
        app.ingest(event("run-100", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-300", ActivityStatus::NotStarted, 300));

        assert_eq!(
            app.list_state().selected(),
            Some(0),
            "the newest activity must be highlighted regardless of replay-burst arrival order"
        );
        assert_eq!(
            app.selected().unwrap().run_id,
            "run-500",
            "index 0 must actually be the newest activity, not a stale index"
        );
    }

    #[test]
    fn the_selection_stays_pinned_to_the_top_through_every_ingest() {
        // The multi-ingest case Test 1 alone would miss: the pin must hold
        // continuously across every ingest before the user's first
        // interaction, not just arrive at the right answer after the last
        // one. Each pair becomes the new newest activity in turn, so
        // asserting INSIDE the loop proves the pin never lapses mid-burst.
        let mut app = App::new(10);
        for (run_id, started_at_ms) in [("run-a", 100u64), ("run-b", 200), ("run-c", 300)] {
            app.ingest(event(run_id, ActivityStatus::NotStarted, started_at_ms));
            assert_eq!(
                app.list_state().selected(),
                Some(0),
                "selection must be pinned to index 0 after ingesting {run_id}"
            );
            assert_eq!(
                app.selected().unwrap().run_id,
                run_id,
                "index 0 must be the just-ingested (newest) activity {run_id}"
            );
        }
    }

    #[test]
    fn navigating_stops_the_auto_pin() {
        // The handover: this test fails on its FIRST assertion today (before
        // select_next is even called) -- with two ingests, the current code
        // is already tracking run-a (the first arrival) by run_id, which
        // after run-b's insert has been pushed to index 1, not selecting
        // run-b at the top. That is the pin bug itself, not a tracking
        // regression: read a failure here as "the pin isn't pinning yet",
        // not "run_id tracking broke".
        let mut app = App::new(10);
        app.ingest(event("run-a", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-b", ActivityStatus::NotStarted, 200));
        assert_eq!(
            app.selected().unwrap().run_id,
            "run-b",
            "the pin must still be active -- no interaction has happened yet"
        );

        app.select_next(); // the user's first explicit navigation -> index 1, run-a

        app.ingest(event("run-c", ActivityStatus::NotStarted, 300));

        assert_eq!(
            app.selected().unwrap().run_id,
            "run-a",
            "after navigating, selection must track the user's chosen activity by run_id"
        );
        assert_eq!(
            app.list_state().selected(),
            Some(2),
            "run-a must have slid down to index 2, not snapped back to the top"
        );
    }

    #[test]
    fn opening_an_activity_counts_as_the_user_taking_over() {
        // D-4, made executable. This test PASSES against today's code -- it
        // is a guard for the decision, not a reproduction of the reported
        // bug. It goes red against the tempting half-fix that flips
        // user_has_selected only inside the four select_* methods and
        // forgets open_selected_activity.
        let mut app = App::new(10);
        app.ingest(event("run-a", ActivityStatus::NotStarted, 100));
        assert_eq!(
            app.selected().unwrap().run_id,
            "run-a",
            "run-a must be auto-selected with no navigation"
        );

        app.open_selected_activity(); // Enter pressed straight onto the pinned top row

        app.ingest(event("run-b", ActivityStatus::NotStarted, 200));

        assert_eq!(
            app.selected().unwrap().run_id,
            "run-a",
            "opening an activity must hand over selection control, same as navigating"
        );
        assert_eq!(app.list_state().selected(), Some(1));
        assert_eq!(app.active_run_id(), Some("run-a"));
    }

    #[test]
    fn select_last_and_select_prev_also_count_as_the_user_taking_over() {
        // Coverage for the two navigation methods the other tests in this
        // section don't exercise on their own. Also a guard that PASSES
        // today. Honest limitation: select_next is isolated by
        // navigating_stops_the_auto_pin and select_first by
        // selection_follows_its_activity_when_a_newer_one_arrives, but
        // select_prev cannot be isolated without a prior navigation call to
        // move off index 0 -- so this test covers select_last cleanly and
        // select_prev only in combination with it.
        let mut app = App::new(10);
        app.ingest(event("run-a", ActivityStatus::NotStarted, 100));
        app.ingest(event("run-b", ActivityStatus::NotStarted, 200));
        app.ingest(event("run-c", ActivityStatus::NotStarted, 300));
        // List is now run-c(300), run-b(200), run-a(100).

        app.select_last(); // index 2, run-a
        app.select_prev(); // index 1, run-b
        assert_eq!(app.selected().unwrap().run_id, "run-b");

        app.ingest(event("run-d", ActivityStatus::NotStarted, 400));

        assert_eq!(
            app.selected().unwrap().run_id,
            "run-b",
            "select_last/select_prev must also hand over selection control"
        );
        assert_eq!(app.list_state().selected(), Some(2));
    }
}
