//! `render(frame, &App, port)`: the locked ROADMAP UI layout -- header
//! (PID/port top-right, date+time), summary (`Activities: N/M`), a
//! color-coded activity list (gray/white/green/red per `ActivityStatus`,
//! each row showing date-stamp/name/status/`via {client}` -- D-03), a
//! trailing keystroke-hint footer, and, once a tab is opened (`Enter`), a
//! `Tabs` bar plus a detail-log panel (D-06). When `app.help_open()` (D-2),
//! a centered `Help` overlay is drawn last, on top of everything else.
//!
//! `port` is passed explicitly rather than stored on `App` (Task 1's state
//! struct) -- header rendering is the only consumer of the daemon port, and
//! keeping it out of `App` keeps that struct's Task-1-tested surface
//! (bounded buffer + upsert-by-run-id) untouched by this task.
//!
//! No `chrono`/`time` dependency: date/time is derived from
//! `SystemTime`/`UNIX_EPOCH` via Howard Hinnant's public-domain
//! `civil_from_days` algorithm (same "avoid a new dependency for a display
//! value" precedent as `apps/shared/src/activity.rs`'s epoch-ms fields).

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::{Alignment, Constraint, Direction, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Tabs, Wrap,
};
use ratatui::Frame;
use serde_json::Value;
use shared::{ActivityLogEvent, ActivityPhase, ActivityStatus};

use crate::app::{Activity, App, Focus};
use crate::event::{HintScope, KEY_HINTS};

/// Renders the full locked ROADMAP layout into `frame` from `app`'s current
/// state. `port` is the daemon WS port this client connected to (header
/// display only): a header (PID/port/date+time), a summary
/// (`Activities: N/M`), a color-coded activity list with `via {client}`
/// attribution (D-03), a keystroke-hint footer, and, once a tab is open, a
/// `Tabs` bar plus a detail-log panel (D-06) -- wrapped, scrollable, with a
/// scrollbar, and JSON payloads pretty-printed and syntax-highlighted (Task
/// 4). The `?` help overlay (D-2), if open, is drawn last so it sits above
/// the list and any open detail panel.
///
/// Takes `&mut App` (Task 4 D-4): the detail panel's render pass measures
/// its own wrapped-row content height and records it back onto `App`
/// (`App::record_detail_metrics`) so every scroll method clamps against the
/// REAL rendered geometry, not a guessed constant.
pub fn render(frame: &mut Frame, app: &mut App, port: u16) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, chunks[0], port);
    render_summary(frame, chunks[1], app);
    render_body(frame, chunks[2], app);
    render_footer(frame, chunks[3], app.focus());

    if app.help_open() {
        render_help_overlay(frame, area);
    }
}

fn render_body(frame: &mut Frame, area: Rect, app: &mut App) {
    if active_activity(app).is_none() {
        render_list(frame, area, app);
        return;
    }

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_list(frame, body[0], app);
    render_tabs_and_detail(frame, body[1], app);
}

fn render_header(frame: &mut Frame, area: Rect, port: u16) {
    let pid = std::process::id();
    let now = format_now();
    let text = format!("PID {pid}  Port {port}  {now}");
    let paragraph = Paragraph::new(text).alignment(Alignment::Right);
    frame.render_widget(paragraph, area);
}

fn render_summary(frame: &mut Frame, area: Rect, app: &App) {
    let running = app
        .activities()
        .iter()
        .filter(|a| a.status == ActivityStatus::Running)
        .count();
    let total = app.activities().len();
    let text = format!("Activities: {running}/{total}");
    frame.render_widget(Paragraph::new(text), area);
}

/// Renders the persistent one-line keystroke-hint footer (D-2/Task 1) by
/// calling `footer_text` for the joining/width logic and rendering the
/// result into `area`. `KEY_HINTS` remains the single source of truth this,
/// `render_help_overlay`, and the `key_hints_are_all_really_mapped` drift
/// guard all render from, so an advertised key can never silently drift
/// from a real binding.
fn render_footer(frame: &mut Frame, area: Rect, focus: Focus) {
    let text = footer_text(focus, area.width);
    frame.render_widget(Paragraph::new(text), area);
}

/// Builds the footer's text for `focus` at the given terminal `width`
/// (quick task 260815-rx6 D-5) -- pure and terminal-free, so it is
/// unit-tested directly without a `TestBackend`.
///
/// Filters `KEY_HINTS` through `hint_visible_for` exactly as before (a
/// `Global`-scoped hint always shows; `Activities`/`Detail`-scoped hints
/// only show under their matching focus) and formats each surviving hint
/// as `"{keys} {short}"` -- unchanged from before this task. What is new is
/// the joining separator: before this task the Detail-focus footer occupied
/// 77 of 80 available columns, so the ninth hint added by this task (the
/// `Tab/S-Tab` binding) would have taken it to 93 columns and silently
/// clipped `Esc back`, `? help`, and `q quit` off the right edge -- a
/// `Paragraph` on a `Constraint::Length(1)` row with no `.wrap(..)` clips
/// silently, it does not wrap or error. This tries the widest separator
/// that still fits: three spaces, then two, then one, falling back to one
/// space if none fit. Measured against the post-change table: Activities
/// stays at three spaces / 53 columns (byte-identical to before), and
/// Detail settles at one space / 79 columns -- one column of headroom, all
/// nine hints intact. Columns are measured by character count, not byte
/// length; every label in `KEY_HINTS` is ASCII today, and counting
/// characters keeps that assumption from becoming a silent bug if one ever
/// is not. Below 80 columns the Detail footer still clips -- unchanged from
/// before this task, and not in scope; 80 columns is the width this table
/// is sized to fit.
fn footer_text(focus: Focus, width: u16) -> String {
    let pairs: Vec<String> = KEY_HINTS
        .iter()
        .filter(|hint| hint_visible_for(hint.scope, focus))
        .map(|hint| format!("{} {}", hint.keys, hint.short))
        .collect();

    for separator in ["   ", "  ", " "] {
        let candidate = pairs.join(separator);
        if candidate.chars().count() <= width as usize {
            return candidate;
        }
    }
    pairs.join(" ")
}

/// Whether a hint of the given `scope` should appear in the footer while
/// `focus` is the current panel (Task 1 D-1): `Global` always shows;
/// `Activities`/`Detail` show only under their matching focus.
fn hint_visible_for(scope: HintScope, focus: Focus) -> bool {
    match scope {
        HintScope::Global => true,
        HintScope::Activities => focus == Focus::Activities,
        HintScope::Detail => focus == Focus::Detail,
    }
}

/// Renders the centered, bordered `Help` overlay (D-2): one line per
/// `KEY_HINTS` entry (padded key label plus its long-form description),
/// followed by a closing-hint line naming the keys that dismiss it. Sized
/// from `KEY_HINTS.len()` plus the closing-hint line plus two border rows,
/// so adding a binding cannot silently clip the overlay. Draws `Clear`
/// first so the list underneath does not bleed through the popup.
fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let key_width = KEY_HINTS.iter().map(|h| h.keys.len()).max().unwrap_or(0);
    let mut lines: Vec<Line> = KEY_HINTS
        .iter()
        .map(|hint| Line::from(format!("{:width$}  {}", hint.keys, hint.long, width = key_width)))
        .collect();
    lines.push(Line::from("Esc/q/? closes this help"));

    let content_width = lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(0)
        .max("Help".len()) as u16
        + 4;
    let content_height = lines.len() as u16 + 2;

    let popup = centered_rect(content_width, content_height, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title("Help");
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup);
}

/// Centers a `width` x `height` rect within `area` using `Flex::Center`.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn render_list(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app.activities().iter().map(activity_list_item).collect();
    let list = List::new(items)
        .block(Block::bordered().title("Activities"))
        .highlight_symbol(">> ");
    let mut list_state = *app.list_state();
    frame.render_stateful_widget(list, area, &mut list_state);
}

/// Renders one row: `mm/dd/yyyy HH:MM:SS` stamp, name, status, and
/// `via {client_name}` attribution (D-03), colored by the ROADMAP-locked
/// status mapping. The stamp leads the row so the newest-first order (D-1)
/// reads directly off the list without hunting for a bare `HH:MM:SS` behind
/// the workflow name.
fn activity_list_item(activity: &Activity) -> ListItem<'static> {
    let text = format!(
        "{}  {}  {:?}  via {}",
        format_epoch_date_time(activity.started_at_ms),
        activity.workflow_id,
        activity.status,
        activity.client_name,
    );
    ListItem::new(text).style(Style::new().fg(status_color(activity.status)))
}

/// ROADMAP-locked status -> color mapping: gray=not-started, white=running,
/// green=success, red=failure.
fn status_color(status: ActivityStatus) -> Color {
    match status {
        ActivityStatus::NotStarted => Color::Gray,
        ActivityStatus::Running => Color::White,
        ActivityStatus::Success => Color::Green,
        ActivityStatus::Failure => Color::Red,
    }
}

/// Renders the `Tabs` bar -- one title per lifecycle-phase entry in the
/// SINGLE open activity's own `log` (`phase_label(e.phase)`, D-3), with the
/// selected phase marked -- plus a detail-log panel (D-06) showing ONLY
/// that selected phase's line and payload.
///
/// Verified against the vendored `ratatui-widgets 0.3.2` (D-3, carried
/// forward from 260815-pjn): `Tabs::new` defaults its own `selected` field
/// to `Some(0)` whenever titles are non-empty -- so omitting the
/// `.select(..)` call below is not neutral, it actively highlights the
/// FIRST phase as soon as more than one exists. `Some(current_phase)` is
/// passed only when `current_phase < titles.len()`; otherwise `None` --  an
/// out-of-range index yields NO highlight rather than a confident wrong
/// one.
fn render_tabs_and_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);

    // Resolve the active activity ONCE (D-1 key link): the strip's titles
    // and the panel's content both come from this SAME lookup and the SAME
    // `current_phase` index, so the strip can never advertise a phase the
    // panel is not showing. Build the owned panel title and lines BEFORE
    // releasing this immutable borrow -- `render_detail_panel` still needs
    // `&mut App` for `record_detail_metrics` (Task 4 D-4). This is exactly
    // why Task 3's walker emits `'static` spans: the owned data outlives
    // the borrowed `Activity`.
    let current_phase = app.current_phase();
    let detail = active_activity(app).map(|activity| {
        let titles: Vec<Line> = activity
            .log
            .iter()
            .map(|e| Line::from(phase_label(e.phase)))
            .collect();
        let selection = (current_phase < titles.len()).then_some(current_phase);
        let entry_lines = activity.log.get(current_phase).map(single_phase_lines).unwrap_or_default();
        (titles, selection, activity.workflow_id.clone(), entry_lines)
    });

    let Some((titles, selection, title, lines)) = detail else {
        return;
    };

    let tabs = Tabs::new(titles).block(Block::bordered().title("Tabs")).select(selection);
    frame.render_widget(tabs, chunks[0]);

    render_detail_panel(frame, chunks[1], title, lines, app);
}

/// Exhaustive `ActivityPhase` -> tab-title `&'static str` match (D-3), NOT
/// `format!("{:?}", phase)`: makes each tab title a deliberate UI string a
/// variant rename cannot silently change, and a newly-added `ActivityPhase`
/// variant fails the build here instead of leaking a debug token into the
/// UI. Returning `&'static str` keeps `Line::from(phase_label(..))` a
/// `Line<'static>` with no borrow of `App`, which the borrow ordering in
/// `render_tabs_and_detail` depends on.
///
/// The detail panel's own `[{phase:?}] {at_ms}` line deliberately keeps its
/// raw `Debug` form instead -- the tab title is a label, the log line is a
/// record, and several tests correctly pin the record form (`[Completed]
/// 1050`).
fn phase_label(phase: ActivityPhase) -> &'static str {
    match phase {
        ActivityPhase::Invoked => "Invoked",
        ActivityPhase::Running => "Running",
        ActivityPhase::Completed => "Completed",
        ActivityPhase::Failed => "Failed",
    }
}

/// Builds ONE log event's lines: its plain `[{phase:?}] {at_ms}` prefix
/// line, followed -- when the transition carries a `detail` payload -- by
/// that payload's indented, syntax-highlighted JSON block (Task 3's
/// `json_detail_lines`). Replaces the prior `detail_panel_lines`, which
/// stacked every phase in the log; the redesign renders exactly one.
fn single_phase_lines(log_event: &ActivityLogEvent) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!("[{:?}] {}", log_event.phase, log_event.at_ms))];
    if let Some(detail) = &log_event.detail {
        lines.extend(json_detail_lines(detail, 1));
    }
    lines
}

/// Resolves `app.active_run_id()` (260816-ems D-1) against
/// `app.activities()` -- the single activity currently open. Keeps the
/// `find`-by-`run_id` lookup's `Option` result, so an activity evicted from
/// the bounded buffer (D-11) while it was open still degrades to rendering
/// no strip and no panel.
fn active_activity(app: &App) -> Option<&Activity> {
    app.active_run_id()
        .and_then(|run_id| app.activities().iter().find(|a| a.run_id == run_id))
}

/// Renders the detail-log panel (D-06/Task 4): a bordered block labeled
/// with the activity's name, its lines soft-wrapped (long values wrap
/// instead of clipping at the right edge), scrolled to `app`'s recorded
/// offset, and carrying a right-edge scrollbar once the content overflows
/// the viewport.
///
/// Follows D-3's verified sequence exactly (measured against the pinned
/// ratatui 0.30.2 / ratatui-widgets 0.3.2 source):
/// 1. Compute the bordered block's INNER width/height.
/// 2. Build the `Paragraph` with `.wrap(Wrap { trim: false })` -- `trim:
///    true` would strip the JSON block's leading indentation (Task 3),
///    defeating it entirely.
/// 3. Measure `content_rows` via `Paragraph::line_count(inner_w)` -- this
///    runs the SAME `WordWrapper` the renderer runs, so it is correct by
///    construction rather than a re-implementation that could drift.
///    `line_count` wraps at the width it is given WITHOUT subtracting the
///    block's own horizontal space, and it ADDS the block's two vertical
///    border rows to its result -- so the caller passes the INNER width
///    and subtracts 2 from the result.
/// 4. Record what was measured via `App::record_detail_metrics`, which
///    also re-clamps the stored scroll offset -- a resize that shrinks the
///    content self-corrects on the next frame (D-4) instead of costing the
///    user a wasted keypress.
/// 5. Render with `.scroll((scroll, 0))`. With wrap enabled this offset
///    counts RENDERED WRAPPED ROWS, not logical lines -- the fact the
///    whole clamp bound depends on.
/// 6. Render a scrollbar sized from the SAME `content_rows`/`inner_h` the
///    clamp used, so the thumb's size reflects how much of the log is
///    visible -- but only when there is something to scroll: rendering a
///    `Scrollbar` with `content_length <= viewport_length` still draws a
///    full-track thumb (ratatui-widgets `part_lengths`), which would be
///    indistinguishable from "everything fits"; guarding on overflow keeps
///    the open/closed behavior the tests pin.
fn render_detail_panel(frame: &mut Frame, area: Rect, title: String, lines: Vec<Line<'static>>, app: &mut App) {
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);

    let block = Block::bordered().title(title);
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });

    let content_rows = paragraph.line_count(inner_w).saturating_sub(2);
    app.record_detail_metrics(content_rows, inner_h);
    let scroll = app.detail_scroll();

    frame.render_widget(paragraph.scroll((scroll, 0)), area);

    if content_rows > inner_h as usize {
        let mut scrollbar_state = ScrollbarState::new(content_rows)
            .position(scroll as usize)
            .viewport_content_length(inner_h as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar_state,
        );
    }
}

/// Renders an activity row's leading stamp as month-first `mm/dd/yyyy
/// HH:MM:SS`. Deliberately distinct from `format_now`'s ISO `yyyy-mm-dd`
/// header stamp (the header is a clock; this is a sortable log stamp) --
/// see the module doc comment's non-goal note. Both derive the same
/// day-count/seconds-of-day split from epoch ms and feed the same shared
/// `civil_from_days`/`format_hms` algorithm, so no `chrono`/`time`
/// dependency is added for either.
fn format_epoch_date_time(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{m:02}/{d:02}/{y:04} {}", format_hms(secs % 86_400))
}

fn format_hms(secs_of_day: u64) -> String {
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{hh:02}:{mm:02}:{ss:02}")
}

fn format_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {}", format_hms(secs % 86_400))
}

/// Howard Hinnant's `civil_from_days` (public domain): converts a day count
/// since the 1970-01-01 epoch into a proleptic-Gregorian `(year, month,
/// day)` triple, without pulling in `chrono`/`time`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// -- Task 3: JSON pretty-print + syntax-highlight walker ----------------
//
// `ActivityLogEvent.detail` (`apps/shared/src/activity.rs:51`) arrives
// already parsed as `serde_json::Value` -- D-5 walks it ONCE, recursively,
// emitting indented `Line`/`Span`s directly. No highlighting crate is added
// and no pretty string is serialized and then re-tokenized: that would mean
// writing a JSON lexer to parse our own output, with two representations to
// keep in agreement. Same "avoid a new dependency for a display value"
// precedent this module already follows for `civil_from_days` (see the
// module doc comment).

/// Object/array key color.
const JSON_KEY_COLOR: Color = Color::Cyan;
/// String value color.
const JSON_STRING_COLOR: Color = Color::Green;
/// Number value color.
const JSON_NUMBER_COLOR: Color = Color::Yellow;
/// Bool value color.
const JSON_BOOL_COLOR: Color = Color::Magenta;
/// Null value color -- muted, reusing the list's `Gray` register.
const JSON_NULL_COLOR: Color = Color::Gray;
/// Brace/bracket/colon/comma color -- muted, reusing the list's `DarkGray`
/// register so the panel reads consistently with the rest of the UI.
const JSON_PUNCTUATION_COLOR: Color = Color::DarkGray;

/// Entry point (Task 4 calls this): walks `value` once and returns owned,
/// styled `Line<'static>`s ready to render, starting at `base_indent`
/// indent levels (two spaces each).
pub fn json_detail_lines(value: &Value, base_indent: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    push_json_lines(&mut out, value, base_indent, Vec::new(), false);
    out
}

/// Recursive walker: appends one or more `Line`s to `out` for `value`.
/// `indent` is the level of the value's OPENING line. `prefix` is the
/// already-styled spans placed before the value on that first line -- the
/// quoted key plus a muted colon-space for an object entry, or empty for an
/// array element. `trailing_comma` appends a muted separator after the
/// value's closing line (or, for a scalar/empty-container, the single line
/// itself) -- commas and indentation are decided at this one place, so a
/// nested value can never emit a mismatched separator.
fn push_json_lines(
    out: &mut Vec<Line<'static>>,
    value: &Value,
    indent: usize,
    prefix: Vec<Span<'static>>,
    trailing_comma: bool,
) {
    match value {
        // An empty container collapses to a single line carrying both
        // delimiters, so a near-empty payload does not eat two rows.
        Value::Object(map) if map.is_empty() => {
            push_single_line(out, indent, prefix, punct_span("{}"), trailing_comma);
        }
        Value::Array(items) if items.is_empty() => {
            push_single_line(out, indent, prefix, punct_span("[]"), trailing_comma);
        }
        Value::Object(map) => {
            let mut opening = indent_prefix_spans(indent, prefix);
            opening.push(punct_span("{"));
            out.push(Line::from(opening));

            let last = map.len() - 1;
            for (i, (key, val)) in map.iter().enumerate() {
                let entry_prefix = vec![key_span(key), punct_span(": ")];
                push_json_lines(out, val, indent + 1, entry_prefix, i != last);
            }

            let mut closing = vec![indent_span(indent), punct_span("}")];
            if trailing_comma {
                closing.push(punct_span(","));
            }
            out.push(Line::from(closing));
        }
        Value::Array(items) => {
            let mut opening = indent_prefix_spans(indent, prefix);
            opening.push(punct_span("["));
            out.push(Line::from(opening));

            let last = items.len() - 1;
            for (i, val) in items.iter().enumerate() {
                push_json_lines(out, val, indent + 1, Vec::new(), i != last);
            }

            let mut closing = vec![indent_span(indent), punct_span("]")];
            if trailing_comma {
                closing.push(punct_span(","));
            }
            out.push(Line::from(closing));
        }
        scalar => {
            push_single_line(out, indent, prefix, scalar_span(scalar), trailing_comma);
        }
    }
}

/// Emits one line: indent, `prefix`, then `value_span`, then the optional
/// trailing separator. Shared by scalars and empty containers -- both are
/// exactly one line.
fn push_single_line(
    out: &mut Vec<Line<'static>>,
    indent: usize,
    prefix: Vec<Span<'static>>,
    value_span: Span<'static>,
    trailing_comma: bool,
) {
    let mut spans = indent_prefix_spans(indent, prefix);
    spans.push(value_span);
    if trailing_comma {
        spans.push(punct_span(","));
    }
    out.push(Line::from(spans));
}

/// The indent spans followed by the caller-supplied `prefix` -- the common
/// start of every line this walker emits.
fn indent_prefix_spans(indent: usize, prefix: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut spans = vec![indent_span(indent)];
    spans.extend(prefix);
    spans
}

/// Two spaces per indent level (Task 3's "nested_values_are_indented_two_
/// spaces_per_level" contract), as an unstyled span.
fn indent_span(indent: usize) -> Span<'static> {
    Span::raw(" ".repeat(indent * 2))
}

/// A brace/bracket/colon/comma span in the muted punctuation color.
fn punct_span(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::new().fg(JSON_PUNCTUATION_COLOR))
}

/// A quoted, key-colored object-key span. Uses `serde_json::to_string` (not
/// Rust's `Debug` escaping) so the emitted quoting/escaping is exactly the
/// JSON string-literal grammar -- required for the round-trip contract.
fn key_span(key: &str) -> Span<'static> {
    let text = serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""));
    Span::styled(text, Style::new().fg(JSON_KEY_COLOR))
}

/// A styled span for a scalar (string/number/bool/null) `Value`.
/// `Value::to_string()` renders exactly that scalar's JSON token (quotes
/// and escaping included for strings), so the emitted text is always
/// valid, re-parseable JSON -- never called on `Object`/`Array`.
fn scalar_span(value: &Value) -> Span<'static> {
    let color = match value {
        Value::String(_) => JSON_STRING_COLOR,
        Value::Number(_) => JSON_NUMBER_COLOR,
        Value::Bool(_) => JSON_BOOL_COLOR,
        Value::Null => JSON_NULL_COLOR,
        Value::Object(_) | Value::Array(_) => unreachable!("scalar_span called on a container Value"),
    };
    Span::styled(value.to_string(), Style::new().fg(color))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use shared::{ActivityEvent, ActivityLogEvent, ActivityPhase};

    /// Flattens a `Line`'s spans into `(content, fg color)` pairs, in span
    /// order -- Task 3's tests assert on span content and `style.fg`
    /// directly, since the JSON walker's whole job is styling.
    fn flatten_spans(lines: &[Line<'static>]) -> Vec<(String, Color)> {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| (span.content.to_string(), span.style.fg.unwrap_or(Color::Reset)))
            .collect()
    }

    /// Concatenates one `Line`'s span contents into a single string (no
    /// separators inserted) -- used to inspect a single line's leading
    /// indentation or trailing punctuation.
    fn flatten_line(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn leading_spaces(s: &str) -> usize {
        s.chars().take_while(|c| *c == ' ').count()
    }

    #[test]
    fn json_scalars_get_distinct_colors() {
        let value = serde_json::json!({
            "s": "hello",
            "n": 42,
            "b": true,
            "z": null,
        });
        let lines = json_detail_lines(&value, 0);
        let spans = flatten_spans(&lines);

        let mut value_colors = Vec::new();
        for (content, color) in &spans {
            if ["\"hello\"", "42", "true", "null"].contains(&content.as_str()) {
                value_colors.push(*color);
            }
        }
        assert_eq!(
            value_colors.len(),
            4,
            "all four scalar values (string/number/bool/null) must be present: {spans:?}"
        );
        let unique: HashSet<Color> = value_colors.iter().copied().collect();
        assert_eq!(
            unique.len(),
            4,
            "the four scalar value colors must be pairwise distinct: {value_colors:?}"
        );

        for key_text in ["\"s\"", "\"n\"", "\"b\"", "\"z\""] {
            let key_color = spans.iter().find(|(c, _)| c == key_text).map(|(_, col)| *col);
            assert_eq!(
                key_color,
                Some(JSON_KEY_COLOR),
                "key {key_text} must carry the key color: {spans:?}"
            );
        }
    }

    #[test]
    fn object_keys_render_quoted_with_the_key_color() {
        let value = serde_json::json!({"name": "claude"});
        let lines = json_detail_lines(&value, 0);
        let spans = flatten_spans(&lines);

        let key = spans.iter().find(|(c, _)| c == "\"name\"");
        assert!(key.is_some(), "the key must render quoted with its surrounding quotes: {spans:?}");
        assert_eq!(key.unwrap().1, JSON_KEY_COLOR, "the key span must carry the key color");
    }

    #[test]
    fn nested_values_are_indented_two_spaces_per_level() {
        let value = serde_json::json!({"outer": {"inner": 1}});
        let lines = json_detail_lines(&value, 0);

        let outer_line = lines
            .iter()
            .find(|l| flatten_line(l).contains("\"outer\""))
            .expect("the outer key's line must be present");
        let inner_line = lines
            .iter()
            .find(|l| flatten_line(l).contains("\"inner\""))
            .expect("the inner key's line must be present");

        let outer_indent = leading_spaces(&flatten_line(outer_line));
        let inner_indent = leading_spaces(&flatten_line(inner_line));

        assert!(
            inner_indent > outer_indent,
            "the inner key's line ({inner_indent} leading spaces) must be indented further than the outer key's line ({outer_indent}): outer={outer_line:?} inner={inner_line:?}"
        );
    }

    #[test]
    fn every_element_but_the_last_carries_a_separator() {
        let value = serde_json::json!({"a": 1, "b": 2});
        let lines = json_detail_lines(&value, 0);

        let comma_lines = lines
            .iter()
            .filter(|l| flatten_line(l).trim_end().ends_with(','))
            .count();
        assert_eq!(
            comma_lines, 1,
            "exactly one line must carry the separator between the object's two entries: {lines:?}"
        );
    }

    #[test]
    fn empty_object_and_empty_array_render_on_one_line_each() {
        let obj_lines = json_detail_lines(&serde_json::json!({}), 0);
        assert_eq!(obj_lines.len(), 1, "an empty object must collapse to a single line");
        assert!(flatten_line(&obj_lines[0]).contains("{}"));

        let arr_lines = json_detail_lines(&serde_json::json!([]), 0);
        assert_eq!(arr_lines.len(), 1, "an empty array must collapse to a single line");
        assert!(flatten_line(&arr_lines[0]).contains("[]"));
    }

    #[test]
    fn arrays_of_objects_round_trip_structurally() {
        let value = serde_json::json!([
            {"a": 1, "nested": {"x": [1, 2, 3]}},
            {"b": "two"}
        ]);
        let lines = json_detail_lines(&value, 0);

        let concatenated: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.to_string())
            .collect();
        let parsed: Value = serde_json::from_str(&concatenated)
            .unwrap_or_else(|e| panic!("walker output must be valid JSON ({e}): {concatenated}"));

        assert_eq!(parsed, value, "round-tripping the walker's output must yield the original value");
    }

    #[test]
    fn punctuation_is_muted_relative_to_values() {
        let value = serde_json::json!({"a": [1, 2]});
        let lines = json_detail_lines(&value, 0);
        let spans = flatten_spans(&lines);

        let punctuation_texts = ["{", "}", "[", "]", ": "];
        let mut found_any = false;
        for (content, color) in &spans {
            if punctuation_texts.contains(&content.as_str()) {
                found_any = true;
                assert_eq!(
                    *color, JSON_PUNCTUATION_COLOR,
                    "punctuation span {content:?} must carry the punctuation color, not a value color"
                );
            }
        }
        assert!(found_any, "expected at least one punctuation span in: {spans:?}");
    }

    fn event(
        run_id: &str,
        client_name: &str,
        status: ActivityStatus,
        started_at_ms: u64,
    ) -> ActivityEvent {
        ActivityEvent {
            run_id: run_id.to_string(),
            workflow_id: "set_timer".to_string(),
            client_name: client_name.to_string(),
            status,
            started_at_ms,
            log: vec![
                ActivityLogEvent {
                    phase: ActivityPhase::Invoked,
                    at_ms: started_at_ms,
                    detail: None,
                },
                ActivityLogEvent {
                    phase: ActivityPhase::Completed,
                    at_ms: started_at_ms + 50,
                    detail: Some(serde_json::json!({"ok": true})),
                },
            ],
        }
    }

    /// Flattens the rendered buffer into one string (row-joined) so tests
    /// can assert substrings without depending on exact column positions.
    /// Takes `&mut App` (Task 4 D-4): `render` records what the detail
    /// panel's render pass measured back onto `App`.
    fn render_to_string(app: &mut App, port: u16, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
        terminal
            .draw(|frame| render(frame, app, port))
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

    /// Finds the foreground color of the first cell whose rendered text
    /// contains `needle`, by scanning row-by-row for the needle substring
    /// and returning that row's first non-space cell color. Takes
    /// `&mut App` for the same reason as `render_to_string` (Task 4 D-4).
    fn row_color_containing(app: &mut App, port: u16, width: u16, height: u16, needle: &str) -> Option<Color> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
        terminal
            .draw(|frame| render(frame, app, port))
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

    /// Renders and returns the concatenated symbols of every buffer cell
    /// carrying `Modifier::REVERSED` (Task 2 D-4). `Tabs` is the only widget
    /// in this UI that emits that modifier -- `render_list` leaves
    /// `List::highlight_style` at its plain default (only `highlight_symbol`
    /// is set) and no `Table` is rendered -- so the result is exactly the
    /// active tab's label, with nothing else contaminating it. A
    /// column-index search (like `row_color_containing`) cannot be used
    /// here: `render_body` splits the body 50/50, so the activity list
    /// renders the SAME workflow name on the left half of the very same
    /// rows the tab strip occupies, and a leftmost-match search would land
    /// in the list, not the strip.
    fn reversed_cells_text(app: &mut App, port: u16, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
        terminal
            .draw(|frame| render(frame, app, port))
            .expect("render into TestBackend");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for cell in buffer.content() {
            if cell.modifier.contains(ratatui::style::Modifier::REVERSED) {
                out.push_str(cell.symbol());
            }
        }
        out
    }

    /// Isolates the `Tabs` block's title row from the activity list occupying
    /// the same rows on the left half of the body (260816-ems Task 1). The
    /// layout is deterministic: row 0 header, row 1 summary, body from row
    /// 2; `render_tabs_and_detail` gives the `Tabs` block `Constraint::
    /// Length(3)` so its top border is row 2, its titles row 3, its bottom
    /// border row 4; `render_body`'s 50/50 horizontal split puts the strip
    /// in columns `width / 2` onward. Asserts row 2's right half carries the
    /// `Tabs` block title FIRST, so a future layout change fails loudly here
    /// instead of silently reading the wrong row.
    fn tab_strip_text(app: &mut App, port: u16, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
        terminal
            .draw(|frame| render(frame, app, port))
            .expect("render into TestBackend");
        let buffer = terminal.backend().buffer();
        let row_text = |row: usize| -> String {
            buffer
                .content()
                .chunks(width as usize)
                .nth(row)
                .map(|cells| cells.iter().map(|c| c.symbol()).collect::<String>())
                .unwrap_or_default()
        };

        let half = (width / 2) as usize;
        let border_row = row_text(2);
        let border_right_half: String = border_row.chars().skip(half).collect();
        assert!(
            border_right_half.contains("Tabs"),
            "layout assumption broken: row 2's right half must contain the Tabs block title: {border_row:?}"
        );

        let titles_row = row_text(3);
        titles_row.chars().skip(half).collect()
    }

    // -- 260816-ems Task 1: new per-activity-phase-tab model (RED) --------

    #[test]
    fn the_tab_strip_shows_the_open_activity_s_phases_not_workflow_names() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        ev.workflow_id = "alpha_wf".to_string();
        ev.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Completed,
                at_ms: 1_050,
                detail: None,
            },
        ];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        let strip = tab_strip_text(&mut app, 47100, 120, 30);

        assert!(strip.contains("Invoked"), "strip must show the phase name Invoked: {strip:?}");
        assert!(strip.contains("Completed"), "strip must show the phase name Completed: {strip:?}");
        assert!(
            !strip.contains("alpha_wf"),
            "strip must NOT show the workflow name: {strip:?}"
        );
    }

    #[test]
    fn opening_a_different_activity_replaces_the_whole_tab_set() {
        let mut app = App::new(10);
        let mut ev_a = event("run-a", "orchestrator-cli", ActivityStatus::Success, 1_000);
        ev_a.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Completed,
                at_ms: 1_050,
                detail: None,
            },
        ];
        let mut ev_b = event("run-b", "orchestrator-cli", ActivityStatus::Running, 2_000);
        ev_b.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 2_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Running,
                at_ms: 2_050,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Failed,
                at_ms: 2_100,
                detail: None,
            },
        ];
        app.ingest(ev_a);
        app.ingest(ev_b);

        // Descending order: run-b (started 2000) sorts before run-a (started 1000).
        app.select_next();
        app.select_next();
        assert_eq!(app.selected().unwrap().run_id, "run-a");
        app.open_selected_activity();
        let strip_a = tab_strip_text(&mut app, 47100, 120, 30);
        assert!(strip_a.contains("Completed"), "run-a's strip must show Completed: {strip_a:?}");
        assert!(!strip_a.contains("Failed"), "run-a's strip must NOT show Failed: {strip_a:?}");

        app.select_prev();
        assert_eq!(app.selected().unwrap().run_id, "run-b");
        app.open_selected_activity();
        let strip_b = tab_strip_text(&mut app, 47100, 120, 30);
        assert!(strip_b.contains("Failed"), "run-b's strip must show Failed: {strip_b:?}");
        assert!(!strip_b.contains("Completed"), "run-b's strip must NOT show Completed: {strip_b:?}");
    }

    #[test]
    fn the_detail_panel_renders_only_the_selected_phase() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        ev.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Completed,
                at_ms: 1_050,
                detail: Some(serde_json::json!({"ok": true})),
            },
        ];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        let rendered = render_to_string(&mut app, 47100, 120, 30);

        assert!(
            rendered.contains("[Completed] 1050"),
            "panel must show the selected phase's line: {rendered}"
        );
        assert!(rendered.contains("\"ok\""), "panel must show the selected phase's payload: {rendered}");
        assert!(
            !rendered.contains("[Invoked] 1000"),
            "panel must NOT show the other phase's line: {rendered}"
        );
    }

    #[test]
    fn the_tab_strip_highlights_the_selected_phase() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        ev.workflow_id = "alpha_wf".to_string();
        ev.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Completed,
                at_ms: 1_050,
                detail: None,
            },
        ];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        let reversed = reversed_cells_text(&mut app, 47100, 120, 30);

        assert_eq!(
            reversed, "Completed",
            "the selected (default last) phase must be the only highlighted cells: {reversed:?}"
        );
    }

    #[test]
    fn no_tab_strip_or_detail_panel_before_anything_is_opened() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(
            !rendered.contains("Tabs"),
            "no Tabs block should render before anything is opened: {rendered}"
        );
    }

    #[test]
    fn an_evicted_open_activity_renders_no_strip_or_panel() {
        let mut app = App::new(1);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.select_next();
        app.open_selected_activity();

        app.ingest(event("run-2", "orchestrator-cli", ActivityStatus::Success, 2_000));

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(
            !rendered.contains("[Invoked] 1000"),
            "an evicted open activity's log must not render: {rendered}"
        );
    }

    #[test]
    fn render_shows_header_summary_and_activity_rows() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.ingest(event("run-2", "orchestrator-tui", ActivityStatus::Running, 2_000));

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(rendered.contains("47100"), "header must show the port: {rendered}");
        assert!(
            rendered.contains("Activities: 1/2"),
            "summary must show running/total: {rendered}"
        );
        assert!(
            rendered.contains("set_timer"),
            "each row must show the activity name: {rendered}"
        );
    }

    #[test]
    fn render_shows_via_client_attribution_per_row() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.ingest(event("run-2", "orchestrator-tui", ActivityStatus::Running, 2_000));

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(
            rendered.contains("via orchestrator-cli"),
            "row must attribute the initiating client (D-03): {rendered}"
        );
        assert!(
            rendered.contains("via orchestrator-tui"),
            "row must attribute the initiating client (D-03): {rendered}"
        );
    }

    #[test]
    fn render_colors_rows_by_status() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::NotStarted, 1_000));
        app.ingest(event("run-2", "orchestrator-cli", ActivityStatus::Running, 2_000));
        app.ingest(event("run-3", "orchestrator-cli", ActivityStatus::Success, 3_000));
        app.ingest(event("run-4", "orchestrator-cli", ActivityStatus::Failure, 4_000));

        assert_eq!(
            row_color_containing(&mut app, 47100, 80, 24, "NotStarted"),
            Some(Color::Gray),
            "NotStarted rows must be gray"
        );
        assert_eq!(
            row_color_containing(&mut app, 47100, 80, 24, "Running"),
            Some(Color::White),
            "Running rows must be white"
        );
        assert_eq!(
            row_color_containing(&mut app, 47100, 80, 24, "Success"),
            Some(Color::Green),
            "Success rows must be green"
        );
        assert_eq!(
            row_color_containing(&mut app, 47100, 80, 24, "Failure"),
            Some(Color::Red),
            "Failure rows must be red"
        );
    }

    #[test]
    fn render_shows_detail_log_panel_for_an_open_activity() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.select_next();
        app.open_selected_activity();

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(
            rendered.contains("set_timer"),
            "an open activity must render a detail panel labeled with the activity name (D-06): {rendered}"
        );
        // event()'s 2-entry log ([Invoked, Completed]) defaults to the last
        // phase on open (D-2): the bare word "Completed" also appears in the
        // tab strip, so the bracketed record form is what unambiguously
        // pins the PANEL's content (260816-ems D-6).
        assert!(
            rendered.contains("[Completed] 1050"),
            "the detail panel must show the selected (last, default) phase's line (D-06): {rendered}"
        );
        assert!(
            !rendered.contains("[Invoked] 1000"),
            "the detail panel must NOT show the non-selected phase's line under the new one-phase-at-a-time model: {rendered}"
        );
    }

    #[test]
    fn render_shows_date_and_time_as_the_first_column() {
        let mut app = App::new(10);
        app.ingest(event(
            "run-1",
            "orchestrator-cli",
            ActivityStatus::Success,
            1_700_000_000_000,
        ));

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(
            rendered.contains("11/14/2023 22:13:20  set_timer"),
            "row must lead with mm/dd/yyyy HH:MM:SS before the workflow name: {rendered}"
        );
    }

    #[test]
    fn render_lists_activities_newest_first() {
        let mut app = App::new(10);
        app.ingest(event("run-old", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.ingest(event("run-new", "orchestrator-cli", ActivityStatus::Success, 2_000_000));

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        let newer_stamp = format_epoch_date_time(2_000_000);
        let older_stamp = format_epoch_date_time(1_000);
        let newer_pos = rendered
            .find(&newer_stamp)
            .expect("newer stamp must appear in the render");
        let older_pos = rendered
            .find(&older_stamp)
            .expect("older stamp must appear in the render");
        assert!(
            newer_pos < older_pos,
            "the newer activity must render above (at a lower byte offset than) the older one: {rendered}"
        );
    }

    #[test]
    fn highlighted_row_shows_the_selected_activity() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.ingest(event("run-2", "orchestrator-cli", ActivityStatus::Success, 2_000));
        app.select_next();
        app.ingest(event(
            "run-3",
            "orchestrator-cli",
            ActivityStatus::Success,
            3_000,
        ));

        let rendered = render_to_string(&mut app, 47100, 80, 24);
        let selected_stamp = format_epoch_date_time(app.selected().unwrap().started_at_ms);

        let highlighted_row = rendered
            .lines()
            .find(|line| line.contains(">> "))
            .expect("a highlighted row must be present");
        assert!(
            highlighted_row.contains(&selected_stamp),
            "the highlighted row must carry the same activity App::selected() returns: {rendered}"
        );
    }

    #[test]
    fn the_footer_advertises_every_visible_binding_at_eighty_columns() {
        for focus in [Focus::Activities, Focus::Detail] {
            let text = footer_text(focus, 80);
            assert!(
                text.chars().count() <= 80,
                "footer text for {focus:?} must fit within 80 columns: {text:?} ({} cols)",
                text.chars().count()
            );
            for hint in KEY_HINTS.iter().filter(|h| hint_visible_for(h.scope, focus)) {
                let pair = format!("{} {}", hint.keys, hint.short);
                assert!(
                    text.contains(&pair),
                    "footer for {focus:?} must contain the visible hint {pair:?}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn the_footer_keeps_its_wide_separator_when_everything_fits() {
        let text = footer_text(Focus::Activities, 80);
        assert!(
            text.contains("j/k move   "),
            "the Activities footer, which already fits, must keep its three-space separator rather than always narrowing: {text:?}"
        );
    }

    #[test]
    fn footer_shows_the_tab_switching_hint_only_while_the_detail_panel_is_focused() {
        let mut app = App::new(10);

        let activities_rendered = render_to_string(&mut app, 47100, 80, 24);
        assert!(
            !activities_rendered.contains("Tab/S-Tab tab"),
            "the tab-switching hint must not appear while Activities is focused: {activities_rendered}"
        );

        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.select_next();
        app.open_selected_activity();

        let detail_rendered = render_to_string(&mut app, 47100, 80, 24);
        assert!(
            detail_rendered.contains("tab"),
            "the tab-switching hint's short label must reach the buffer once Detail is focused: {detail_rendered}"
        );
    }

    #[test]
    fn help_overlay_lists_the_tab_switching_binding() {
        let mut app = App::new(10);
        app.show_help();

        let rendered = render_to_string(&mut app, 80, 80, 24);

        assert!(
            rendered.contains("cycle the open activity's phases"),
            "overlay must list the tab-switching binding's new phase-scoped long-form label: {rendered}"
        );
        assert!(
            !rendered.contains("switch to the next/previous open tab"),
            "the old cross-activity wording must not remain (260816-ems D-7): {rendered}"
        );
    }

    #[test]
    fn render_shows_a_keystroke_hint_footer() {
        let mut app = App::new(10);

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(rendered.contains("j/k move"), "footer must show the j/k hint: {rendered}");
        assert!(rendered.contains("q quit"), "footer must show the q hint: {rendered}");
    }

    #[test]
    fn footer_shows_activities_keys_while_the_list_is_focused() {
        let mut app = App::new(10);

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(
            rendered.contains("j/k move"),
            "footer must show the j/k movement hint while Activities is focused: {rendered}"
        );
        assert!(
            rendered.contains("g/G top/bot"),
            "footer must show the g/G jump hint while Activities is focused: {rendered}"
        );
    }

    #[test]
    fn footer_switches_to_detail_keys_once_the_detail_panel_is_focused() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.select_next();
        app.open_selected_activity();

        let rendered = render_to_string(&mut app, 47100, 80, 24);

        assert!(
            rendered.contains("Esc back"),
            "footer must show the Esc hint once the detail panel is focused: {rendered}"
        );
        assert!(
            !rendered.contains("g/G top/bot"),
            "the Activities-only jump hint must not appear while Detail is focused: {rendered}"
        );
    }

    #[test]
    fn help_overlay_is_hidden_until_requested() {
        let mut app = App::new(10);

        let rendered = render_to_string(&mut app, 80, 80, 24);

        assert!(
            !rendered.contains("move selection"),
            "the overlay's long-form label must not appear while help is closed: {rendered}"
        );
    }

    #[test]
    fn help_overlay_lists_every_binding_when_open() {
        let mut app = App::new(10);
        app.show_help();

        let rendered = render_to_string(&mut app, 80, 80, 24);

        assert!(rendered.contains("Help"), "overlay must be titled Help: {rendered}");
        assert!(
            rendered.contains("move selection"),
            "overlay must list the move-selection binding: {rendered}"
        );
        assert!(
            rendered.contains("open the selected activity"),
            "overlay must list the new activity-scoped open binding: {rendered}"
        );
        assert!(
            !rendered.contains("open tab"),
            "the old tab-scoped wording must not remain (260816-ems D-7): {rendered}"
        );
        assert!(
            rendered.contains("show this help"),
            "overlay must list the help binding: {rendered}"
        );
    }

    #[test]
    fn detail_payload_renders_as_indented_highlighted_json() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        ev.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Completed,
                at_ms: 1_050,
                detail: Some(serde_json::json!({"outer": {"inner": "value"}})),
            },
        ];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        // Wide enough that nothing here wraps -- isolates the indentation
        // assertion from Task 4's separate wrap behavior.
        let rendered = render_to_string(&mut app, 120, 120, 30);

        assert!(
            rendered.contains("[Completed] 1050"),
            "the phase-prefix line must stay present and intact -- only the payload is highlighted: {rendered}"
        );
        assert!(rendered.contains("\"inner\""), "the nested key must appear: {rendered}");

        let completed_row = rendered
            .lines()
            .find(|l| l.contains("[Completed] 1050"))
            .expect("the phase-prefix line must be present");
        let inner_row = rendered
            .lines()
            .find(|l| l.contains("\"inner\""))
            .expect("the nested key's line must be present");

        assert!(
            inner_row.find("\"inner\"").unwrap() > completed_row.find("[Completed]").unwrap(),
            "the nested key's line must be indented further right than the phase-prefix line: prefix={completed_row:?} inner={inner_row:?}"
        );
    }

    #[test]
    fn long_detail_lines_wrap_instead_of_clipping() {
        let mut app = App::new(10);
        let long_value = format!("{}TAIL_MARKER_END", "x".repeat(40));
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        ev.log = vec![ActivityLogEvent {
            phase: ActivityPhase::Completed,
            at_ms: 1_050,
            detail: Some(serde_json::json!({"note": long_value})),
        }];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        // Narrow terminal so the detail panel's inner width is well under
        // the value's length -- without `.wrap(..)` the tail is clipped and
        // never reaches the buffer, so this fails before the change and
        // passes after.
        let rendered = render_to_string(&mut app, 60, 60, 24);

        assert!(
            rendered.contains("TAIL_MARKER_END"),
            "a long value must wrap onto the next row rather than clip at the panel's right edge: {rendered}"
        );
    }

    /// Builds a single-phase `detail` payload of `n` distinctly-named,
    /// alphabetically-sortable keys (`field_00`..`field_{n-1}`) -- the
    /// 260816-ems D-6 replacement for spreading bulk across many `log`
    /// entries. Under the new one-phase-at-a-time panel, a wide payload on
    /// ONE phase is what overflows the viewport now; `serde_json`'s default
    /// (non-`preserve_order`) `Map` is a `BTreeMap`, so keys render sorted --
    /// zero-padding keeps that sort order identical to insertion order, so
    /// "first" and "last" stay meaningful landmarks.
    fn wide_json_payload(n: usize) -> Value {
        let mut map = serde_json::Map::new();
        for i in 0..n {
            map.insert(format!("field_{i:02}"), Value::String(format!("value_{i:02}")));
        }
        Value::Object(map)
    }

    #[test]
    fn detail_panel_renders_a_scrollbar_when_content_exceeds_the_viewport() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        // A single Completed entry whose payload comfortably exceeds the
        // panel's inner height (16 rows at 80x24) -- the bulk now lives in
        // one phase's JSON payload rather than spread across many phases
        // (260816-ems D-6): the panel renders exactly one phase, so a
        // many-phase log no longer overflows anything.
        ev.log = vec![ActivityLogEvent {
            phase: ActivityPhase::Completed,
            at_ms: 1_050,
            detail: Some(wide_json_payload(40)),
        }];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        let rendered = render_to_string(&mut app, 80, 80, 24);

        assert!(
            rendered.contains('\u{2588}'),
            "a scrollbar thumb must render once the log overflows the viewport: {rendered}"
        );
    }

    #[test]
    fn no_scrollbar_thumb_when_the_content_fits() {
        let mut app = App::new(10);
        app.ingest(event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.select_next();
        app.open_selected_activity();

        let rendered = render_to_string(&mut app, 80, 80, 24);

        assert!(
            !rendered.contains('\u{2588}'),
            "no scrollbar thumb should render when the log fits entirely within the viewport: {rendered}"
        );
    }

    #[test]
    fn scrolling_to_the_bottom_reveals_the_last_log_line() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        // Single Completed entry with a wide payload (260816-ems D-6) --
        // "field_00" and "field_39" are the first/last-rendered-key
        // landmarks the old test used the phase-per-entry `at_ms` values
        // for.
        ev.log = vec![ActivityLogEvent {
            phase: ActivityPhase::Completed,
            at_ms: 1_050,
            detail: Some(wide_json_payload(40)),
        }];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        // The first render is the one that records the metrics (D-4's
        // measure -> record -> clamp -> apply loop).
        let first = render_to_string(&mut app, 80, 80, 24);
        assert!(
            first.contains("\"field_00\""),
            "the first key must be visible before scrolling: {first}"
        );

        app.scroll_detail_bottom();
        let second = render_to_string(&mut app, 80, 80, 24);

        assert!(
            second.contains("\"field_39\""),
            "the final key must be visible once scrolled to the bottom: {second}"
        );
        assert!(
            !second.contains("\"field_00\""),
            "the first key must have scrolled out of view, never into blank space: {second}"
        );
    }

    #[test]
    fn reopening_a_different_activity_switches_the_displayed_detail_panel() {
        let mut app = App::new(10);
        // event()'s 2-entry log ([Invoked @ started_at_ms, Completed @
        // started_at_ms + 50]) defaults to the LAST phase on open (D-2), so
        // "[Completed] {started_at_ms + 50}" -- not "[Invoked] {started_at_ms}"
        // -- is what identifies which activity the panel is showing. It
        // appears ONLY in the detail panel: the activity list shows the
        // formatted mm/dd/yyyy stamp instead.
        app.ingest(event("run-1000", "orchestrator-cli", ActivityStatus::Success, 1_000));
        app.ingest(event("run-2000", "orchestrator-cli", ActivityStatus::Success, 2_000));

        // Open the 2000-activity (it sorts first, descending), then the
        // 1000-activity.
        app.select_next();
        app.open_selected_activity();
        app.select_next();
        app.open_selected_activity();

        let intermediate = render_to_string(&mut app, 47100, 120, 30);
        assert!(
            intermediate.contains("[Completed] 1050"),
            "setup check: the panel must show the 1000-activity's default (last) phase after opening it: {intermediate}"
        );

        // Reselect the 2000-activity and reopen it.
        app.select_prev();
        app.open_selected_activity();

        let rendered = render_to_string(&mut app, 47100, 120, 30);
        assert!(
            rendered.contains("[Completed] 2050"),
            "reopening a different activity must switch the panel back to its own default (last) phase: {rendered}"
        );
        assert!(
            !rendered.contains("[Completed] 1050"),
            "the panel must no longer show the previously-displayed activity: {rendered}"
        );
    }

    #[test]
    fn recorded_viewport_height_matches_the_rendered_inner_height() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        // Single Completed entry with a wide payload (260816-ems D-6): 40
        // keys renders as 1 phase-line + 40 key lines + 2 brace lines = 43
        // content rows, comfortably more than double the 16-row viewport so
        // the resulting max scroll (27) stays above the 16-row page step --
        // the single page-down below lands unclamped.
        ev.log = vec![ActivityLogEvent {
            phase: ActivityPhase::Completed,
            at_ms: 1_050,
            detail: Some(wide_json_payload(40)),
        }];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        // Known terminal size -> known layout -> known detail panel inner
        // height: 24 total rows - header(1) - summary(1) - footer(1) = 21
        // body rows; the Tabs bar takes 3, leaving 18 for the detail panel;
        // minus 2 border rows = 16 inner rows.
        let _ = render_to_string(&mut app, 80, 80, 24);

        // Pins the recorded PLUMBING itself, not just a downstream effect:
        // App::scroll_detail_page's step is exactly `detail_viewport_rows.
        // max(1)` (Task 2). Content (43 rows) comfortably exceeds both the
        // viewport (16) and the resulting max scroll (27), so this single
        // page-down is not clamped and its exact delta reveals the recorded
        // viewport height directly.
        app.scroll_detail_page(true);

        assert_eq!(
            app.detail_scroll(),
            16,
            "the recorded viewport height must equal the rendered detail panel's inner height"
        );
    }

    // -- 260816-ems Task 2: cycling phases + live log growth (RED) --------

    #[test]
    fn cycling_moves_both_the_strip_highlight_and_the_panel() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        ev.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Completed,
                at_ms: 1_050,
                detail: Some(serde_json::json!({"ok": true})),
            },
        ];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        let reversed = reversed_cells_text(&mut app, 47100, 120, 30);
        assert_eq!(reversed, "Completed", "must default to the last phase, Completed, highlighted: {reversed:?}");
        let rendered = render_to_string(&mut app, 47100, 120, 30);
        assert!(rendered.contains("[Completed] 1050"), "panel must show Completed: {rendered}");
        assert!(!rendered.contains("[Invoked] 1000"), "panel must not show Invoked: {rendered}");

        app.next_phase();

        let reversed = reversed_cells_text(&mut app, 47100, 120, 30);
        assert_eq!(reversed, "Invoked", "cycling forward from Completed must wrap the highlight to Invoked: {reversed:?}");
        let rendered = render_to_string(&mut app, 47100, 120, 30);
        assert!(rendered.contains("[Invoked] 1000"), "panel must follow the cycle to Invoked: {rendered}");
        assert!(!rendered.contains("[Completed] 1050"), "panel must no longer show Completed: {rendered}");
    }

    #[test]
    fn a_phase_arriving_live_appears_as_a_new_tab() {
        let mut app = App::new(10);
        let mut ev = event("run-1", "orchestrator-cli", ActivityStatus::Running, 1_000);
        ev.log = vec![ActivityLogEvent {
            phase: ActivityPhase::Invoked,
            at_ms: 1_000,
            detail: None,
        }];
        app.ingest(ev);
        app.select_next();
        app.open_selected_activity();

        let strip = tab_strip_text(&mut app, 47100, 120, 30);
        assert!(strip.contains("Invoked"), "strip must show Invoked before the phase arrives: {strip:?}");
        assert!(!strip.contains("Completed"), "strip must not show Completed yet: {strip:?}");

        let mut grown = event("run-1", "orchestrator-cli", ActivityStatus::Success, 1_000);
        grown.log = vec![
            ActivityLogEvent {
                phase: ActivityPhase::Invoked,
                at_ms: 1_000,
                detail: None,
            },
            ActivityLogEvent {
                phase: ActivityPhase::Completed,
                at_ms: 1_050,
                detail: None,
            },
        ];
        app.ingest(grown);

        let strip = tab_strip_text(&mut app, 47100, 120, 30);
        assert!(strip.contains("Invoked"), "strip must still show Invoked: {strip:?}");
        assert!(strip.contains("Completed"), "strip must gain the newly-arrived Completed tab: {strip:?}");

        let rendered = render_to_string(&mut app, 47100, 120, 30);
        assert!(
            rendered.contains("[Invoked] 1000"),
            "the panel must stay pinned to the phase the user was already reading: {rendered}"
        );
        assert!(
            !rendered.contains("[Completed] 1050"),
            "the panel must NOT jump to the newly-arrived phase: {rendered}"
        );
    }
}
