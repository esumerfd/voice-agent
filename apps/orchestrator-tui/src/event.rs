//! Crossterm key polling -> `Action` mapping (05-07 Task 3). `poll_action`
//! never `.unwrap()`s on `poll`/`read` (T-05-14) -- any polling/read error
//! is treated the same as "no event this tick" rather than a panic.
//!
//! The actual key -> `Action` mapping (`map_key`) is a pure function with no
//! TTY dependency, so it's unit-tested directly; `poll_action` itself is a
//! thin integration wrapper over `ratatui::crossterm::event::{poll, read}`
//! that can only be exercised against a real terminal (covered by this
//! plan's `cargo build` + full `cargo test` verification gate, per the
//! plan's Task 3 note).

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyModifiers};

use crate::app::Focus;

/// A user-driven navigation/exit action, mapped from a raw crossterm key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SelectNext,
    SelectPrev,
    SelectFirst,
    SelectLast,
    OpenTab,
    FocusActivities,
    Quit,
    ShowHelp,
    CloseHelp,
    ScrollDown,
    ScrollUp,
    ScrollHalfPageDown,
    ScrollHalfPageUp,
    ScrollPageDown,
    ScrollPageUp,
    ScrollTop,
    ScrollBottom,
    NextTab,
    PrevTab,
}

/// Which panel a `KeyHint` applies to (Task 1 D-1): the footer filters on
/// this so it only ever shows the keys that do something in the
/// currently-focused panel, while the `?` help overlay ignores it and lists
/// every hint regardless of scope (the full reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintScope {
    Global,
    Activities,
    Detail,
}

/// One advertised keybinding: the raw `KeyCode` it maps from, its terse
/// footer label (`short`), and its long-form label for the help overlay.
///
/// `KEY_HINTS` is the single source of truth the footer and the `?` overlay
/// both render from (D-2/Task 4) -- `key_hints_are_all_really_mapped` proves
/// every entry here is a live `map_key` binding, so an advertised key that
/// stops working fails the build rather than silently drifting from the
/// UI. Caveat: the movement pair is advertised as one hint (`j/k`) but this
/// table carries only the `j` `KeyCode`, so the drift guard covers `j` and
/// the `k` label rides on the adjacent match arm in `map_key`. The
/// `Tab`/`Shift-Tab` pair (quick task 260815-rx6) has the same shape --
/// this table carries only `KeyCode::Tab`, so the drift guard covers `Tab`
/// and the previous-tab (`BackTab`) key is covered instead by its own
/// direct `map_key` test.
///
/// 260816-ems D-7: `Tab`/`Shift-Tab`'s `short` label deliberately stays
/// `"tab"` rather than becoming `"phase"` even though the redesign re-scoped
/// what a tab IS -- measured against `footer_text`: the Detail-focus footer
/// sits at 79 of 80 columns, one column of headroom, and `"phase"` would add
/// two characters, silently clipping `q quit` off the right edge.
pub struct KeyHint {
    // Read only by `key_hints_are_all_really_mapped` (the drift guard) --
    // never by production rendering code, which uses `keys`/`short`/`long`.
    // Kept as a real field (not a test-only constant) so the guard asserts
    // against the same declaration the footer/overlay render from.
    #[allow(dead_code)]
    pub code: KeyCode,
    pub keys: &'static str,
    pub short: &'static str,
    pub long: &'static str,
    /// Which panel this hint applies to -- the footer filters on this (see
    /// `HintScope`); the help overlay ignores it and lists everything.
    pub scope: HintScope,
    /// The `ctrl` bit `map_key`'s `(code, ctrl)` match key requires for this
    /// binding (D-2) -- read by the drift guard to drive `map_key` with the
    /// right modifiers. Same "real field, test-read only" shape as `code`.
    #[allow(dead_code)]
    pub ctrl: bool,
}

pub const KEY_HINTS: &[KeyHint] = &[
    KeyHint {
        code: KeyCode::Char('j'),
        keys: "j/k",
        short: "move",
        long: "move selection",
        scope: HintScope::Activities,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Enter,
        keys: "Enter",
        short: "open",
        long: "open the selected activity",
        scope: HintScope::Activities,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Char('g'),
        keys: "g/G",
        short: "top/bot",
        long: "jump to first/last activity",
        scope: HintScope::Activities,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Char('j'),
        keys: "j/k",
        short: "scroll",
        long: "scroll one line",
        scope: HintScope::Detail,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Char('d'),
        keys: "d/u",
        short: "half",
        long: "scroll half page",
        scope: HintScope::Detail,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Char('d'),
        keys: "^d/^u",
        short: "pg",
        long: "scroll full page",
        scope: HintScope::Detail,
        ctrl: true,
    },
    KeyHint {
        code: KeyCode::Char('g'),
        keys: "g/G",
        // Deliberately distinct from Activities' "g/G top/bot" hint's short
        // label -- the same `keys` text appears twice (correct: same
        // physical key, different meaning per focus), but the SHORT label
        // must differ so the focus-filtered footer test can assert one is
        // absent while the other is shown without a false-positive
        // substring match.
        short: "start/end",
        long: "scroll to top/bottom",
        scope: HintScope::Detail,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Tab,
        keys: "Tab/S-Tab",
        short: "tab",
        long: "cycle the open activity's phases",
        scope: HintScope::Detail,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Esc,
        keys: "Esc",
        short: "back",
        long: "return focus to the activity list",
        scope: HintScope::Detail,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Char('?'),
        keys: "?",
        short: "help",
        long: "show this help",
        scope: HintScope::Global,
        ctrl: false,
    },
    KeyHint {
        code: KeyCode::Char('q'),
        keys: "q",
        short: "quit",
        long: "quit",
        scope: HintScope::Global,
        ctrl: false,
    },
];

/// Maps a raw crossterm `KeyCode` to an `Action`, modal on `help_open` (D-2)
/// and, once past that gate, contextual on `focus` (D-1): while the help
/// overlay is open, `q`/`?`/`Esc` all close it (`CloseHelp`) and every other
/// key is inert (`None`) -- navigation must not act on a hidden list. This
/// remap lives here, in a pure function unit tests can drive directly,
/// rather than in `run_loop` (an async loop over a real terminal and a live
/// WS client that no unit test in this codebase can reach -- see this
/// module's own doc comment). Putting "the quit key must not quit" or "`j`
/// scrolls the detail panel instead of moving the list selection" in the one
/// function nothing can test would make it verifiable only by hand; here
/// each is a match arm tested like every other.
///
/// `modifiers` is reduced to a single `ctrl` bit (D-2) -- `code` combined
/// with `ctrl` is the whole match key, so SHIFT and ALT are deliberately
/// ignored. `G` in particular arrives as `KeyCode::Char('G')` with
/// `KeyModifiers::SHIFT` set on some terminals and without it on others;
/// matching on the full modifier set would make the binding terminal-
/// dependent.
///
/// Once past the help gate: `q` and `?` are focus-independent (`Quit`,
/// `ShowHelp`). Under `Focus::Activities`: `j`/`k` -> `SelectNext`/
/// `SelectPrev`, `Enter` -> `OpenTab`, `g`/`G` -> `SelectFirst`/
/// `SelectLast`. Under `Focus::Detail` (Task 2): `Esc` -> `FocusActivities`,
/// `j`/`k` -> `ScrollDown`/`ScrollUp`, `d`/`u` -> `ScrollHalfPageDown`/
/// `ScrollHalfPageUp`, `Ctrl-d`/`Ctrl-u` -> `ScrollPageDown`/`ScrollPageUp`,
/// `g`/`G` -> `ScrollTop`/`ScrollBottom` -- the SAME physical keys as
/// Activities' `j`/`k`/`g`/`G`, routed to a different meaning purely by
/// which panel has focus. Also under `Focus::Detail` (quick task 260815-rx6
/// D-1, re-scoped by 260816-ems to cycle the OPEN activity's own lifecycle
/// phases rather than switch between activities): `Tab` -> `NextTab`,
/// `BackTab` -> `PrevTab`. Shift-Tab arrives as
/// its own `KeyCode::BackTab` in the pinned `crossterm 0.29.0` -- never as
/// `Tab` plus a modifier -- so there is no `(KeyCode::Tab, SHIFT)` arm to
/// write; the SHIFT some terminals attach to the unix legacy `BackTab` path
/// is irrelevant here since `modifiers` is already reduced to the `ctrl`
/// bit above, so `BackTab` matches on its own code alone regardless of
/// whether SHIFT was reported.
fn map_key(code: KeyCode, modifiers: KeyModifiers, focus: Focus, help_open: bool) -> Option<Action> {
    if help_open {
        return match code {
            KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Esc => Some(Action::CloseHelp),
            _ => None,
        };
    }

    let ctrl = modifiers.contains(KeyModifiers::CONTROL);

    // Focus-independent bindings.
    match code {
        KeyCode::Char('q') => return Some(Action::Quit),
        KeyCode::Char('?') => return Some(Action::ShowHelp),
        _ => {}
    }

    match focus {
        Focus::Activities => match (code, ctrl) {
            (KeyCode::Char('j'), false) => Some(Action::SelectNext),
            (KeyCode::Char('k'), false) => Some(Action::SelectPrev),
            (KeyCode::Enter, false) => Some(Action::OpenTab),
            (KeyCode::Char('g'), false) => Some(Action::SelectFirst),
            (KeyCode::Char('G'), _) => Some(Action::SelectLast),
            _ => None,
        },
        Focus::Detail => match (code, ctrl) {
            (KeyCode::Esc, false) => Some(Action::FocusActivities),
            (KeyCode::Char('j'), false) => Some(Action::ScrollDown),
            (KeyCode::Char('k'), false) => Some(Action::ScrollUp),
            (KeyCode::Char('d'), false) => Some(Action::ScrollHalfPageDown),
            (KeyCode::Char('u'), false) => Some(Action::ScrollHalfPageUp),
            (KeyCode::Char('d'), true) => Some(Action::ScrollPageDown),
            (KeyCode::Char('u'), true) => Some(Action::ScrollPageUp),
            (KeyCode::Char('g'), false) => Some(Action::ScrollTop),
            (KeyCode::Char('G'), _) => Some(Action::ScrollBottom),
            (KeyCode::Tab, false) => Some(Action::NextTab),
            (KeyCode::BackTab, false) => Some(Action::PrevTab),
            _ => None,
        },
    }
}

/// Polls for a key event within `timeout` and returns the mapped `Action`,
/// threading `focus` (D-1) and `help_open` (D-2) through to `map_key`'s
/// contextual/modal remap. Returns `None` if no event arrived within the
/// timeout, a non-key event arrived (e.g. resize), the key has no mapping,
/// or `poll`/`read` itself errored -- never `.unwrap()`/`.expect()` on
/// external terminal I/O (T-05-14, Pitfall-style never-panic discipline).
pub fn poll_action(timeout: Duration, focus: Focus, help_open: bool) -> Option<Action> {
    let has_event = event::poll(timeout).unwrap_or(false);
    if !has_event {
        return None;
    }

    let Ok(ev) = event::read() else {
        return None;
    };
    let Event::Key(key) = ev else {
        return None;
    };
    map_key(key.code, key.modifiers, focus, help_open)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: KeyModifiers = KeyModifiers::NONE;

    #[test]
    fn map_key_j_selects_next() {
        assert_eq!(
            map_key(KeyCode::Char('j'), NONE, Focus::Activities, false),
            Some(Action::SelectNext)
        );
    }

    #[test]
    fn map_key_k_selects_prev() {
        assert_eq!(
            map_key(KeyCode::Char('k'), NONE, Focus::Activities, false),
            Some(Action::SelectPrev)
        );
    }

    #[test]
    fn map_key_enter_opens_tab() {
        assert_eq!(
            map_key(KeyCode::Enter, NONE, Focus::Activities, false),
            Some(Action::OpenTab)
        );
    }

    #[test]
    fn map_key_q_quits() {
        assert_eq!(
            map_key(KeyCode::Char('q'), NONE, Focus::Activities, false),
            Some(Action::Quit)
        );
    }

    #[test]
    fn map_key_unmapped_char_returns_none() {
        assert_eq!(map_key(KeyCode::Char('x'), NONE, Focus::Activities, false), None);
        assert_eq!(map_key(KeyCode::Esc, NONE, Focus::Activities, false), None);
        assert_eq!(map_key(KeyCode::Left, NONE, Focus::Activities, false), None);
    }

    #[test]
    fn map_key_question_mark_shows_help() {
        assert_eq!(
            map_key(KeyCode::Char('?'), NONE, Focus::Activities, false),
            Some(Action::ShowHelp)
        );
    }

    #[test]
    fn map_key_q_closes_help_instead_of_quitting_while_help_is_open() {
        let action = map_key(KeyCode::Char('q'), NONE, Focus::Activities, true);
        assert_eq!(action, Some(Action::CloseHelp));
        assert_ne!(
            action,
            Some(Action::Quit),
            "q must close the help overlay, not quit the app, while it is open"
        );
    }

    #[test]
    fn map_key_esc_and_question_mark_also_close_help_while_open() {
        assert_eq!(
            map_key(KeyCode::Esc, NONE, Focus::Activities, true),
            Some(Action::CloseHelp)
        );
        assert_eq!(
            map_key(KeyCode::Char('?'), NONE, Focus::Activities, true),
            Some(Action::CloseHelp)
        );
    }

    #[test]
    fn map_key_navigation_is_inert_while_help_is_open() {
        assert_eq!(map_key(KeyCode::Char('j'), NONE, Focus::Activities, true), None);
        assert_eq!(map_key(KeyCode::Char('k'), NONE, Focus::Activities, true), None);
        assert_eq!(map_key(KeyCode::Enter, NONE, Focus::Activities, true), None);
    }

    #[test]
    fn map_key_g_and_shift_g_select_first_and_last_on_activities() {
        assert_eq!(
            map_key(KeyCode::Char('g'), NONE, Focus::Activities, false),
            Some(Action::SelectFirst)
        );
        assert_eq!(
            map_key(KeyCode::Char('G'), NONE, Focus::Activities, false),
            Some(Action::SelectLast)
        );
    }

    #[test]
    fn map_key_shift_g_works_with_or_without_the_shift_modifier() {
        // D-2: `G` must select last whether or not the terminal reports
        // SHIFT alongside the already-uppercase KeyCode -- matching on the
        // full modifier set would make this terminal-dependent.
        assert_eq!(
            map_key(KeyCode::Char('G'), KeyModifiers::NONE, Focus::Activities, false),
            Some(Action::SelectLast)
        );
        assert_eq!(
            map_key(KeyCode::Char('G'), KeyModifiers::SHIFT, Focus::Activities, false),
            Some(Action::SelectLast)
        );
    }

    #[test]
    fn map_key_esc_returns_focus_to_activities_only_from_detail() {
        assert_eq!(
            map_key(KeyCode::Esc, NONE, Focus::Detail, false),
            Some(Action::FocusActivities)
        );
        assert_eq!(map_key(KeyCode::Esc, NONE, Focus::Activities, false), None);
    }

    #[test]
    fn map_key_enter_is_inert_while_the_detail_panel_is_focused() {
        let action = map_key(KeyCode::Enter, NONE, Focus::Detail, false);
        assert_eq!(action, None);
        assert_ne!(
            action,
            Some(Action::OpenTab),
            "Enter must not open a tab while the detail panel already has focus"
        );
    }

    #[test]
    fn map_key_help_still_wins_over_focus() {
        assert_eq!(
            map_key(KeyCode::Char('q'), NONE, Focus::Detail, true),
            Some(Action::CloseHelp)
        );
        assert_eq!(map_key(KeyCode::Char('j'), NONE, Focus::Detail, true), None);
    }

    #[test]
    fn map_key_detail_scroll_bindings() {
        assert_eq!(map_key(KeyCode::Char('j'), NONE, Focus::Detail, false), Some(Action::ScrollDown));
        assert_eq!(map_key(KeyCode::Char('k'), NONE, Focus::Detail, false), Some(Action::ScrollUp));
        assert_eq!(map_key(KeyCode::Char('d'), NONE, Focus::Detail, false), Some(Action::ScrollHalfPageDown));
        assert_eq!(map_key(KeyCode::Char('u'), NONE, Focus::Detail, false), Some(Action::ScrollHalfPageUp));
        assert_eq!(map_key(KeyCode::Char('g'), NONE, Focus::Detail, false), Some(Action::ScrollTop));
        assert_eq!(map_key(KeyCode::Char('G'), NONE, Focus::Detail, false), Some(Action::ScrollBottom));
    }

    #[test]
    fn map_key_ctrl_d_and_ctrl_u_page_rather_than_half_page() {
        let ctrl_d = map_key(KeyCode::Char('d'), KeyModifiers::CONTROL, Focus::Detail, false);
        assert_eq!(ctrl_d, Some(Action::ScrollPageDown));
        assert_ne!(
            ctrl_d,
            Some(Action::ScrollHalfPageDown),
            "Ctrl-d must page, not half-page -- the two differ only by the ctrl bit"
        );

        let ctrl_u = map_key(KeyCode::Char('u'), KeyModifiers::CONTROL, Focus::Detail, false);
        assert_eq!(ctrl_u, Some(Action::ScrollPageUp));
        assert_ne!(
            ctrl_u,
            Some(Action::ScrollHalfPageUp),
            "Ctrl-u must page, not half-page -- the two differ only by the ctrl bit"
        );
    }

    #[test]
    fn map_key_activities_actions_are_inert_while_detail_is_focused() {
        let action = map_key(KeyCode::Char('j'), NONE, Focus::Detail, false);
        assert_eq!(action, Some(Action::ScrollDown));
        assert_ne!(
            action,
            Some(Action::SelectNext),
            "the same physical key must not leak the Activities panel's action while Detail is focused"
        );
    }

    #[test]
    fn map_key_detail_scroll_keys_are_inert_on_activities() {
        assert_eq!(map_key(KeyCode::Char('d'), NONE, Focus::Activities, false), None);
        assert_eq!(map_key(KeyCode::Char('u'), NONE, Focus::Activities, false), None);
        assert_eq!(
            map_key(KeyCode::Char('d'), KeyModifiers::CONTROL, Focus::Activities, false),
            None
        );
    }

    #[test]
    fn map_key_tab_and_back_tab_cycle_tabs_on_detail() {
        assert_eq!(
            map_key(KeyCode::Tab, NONE, Focus::Detail, false),
            Some(Action::NextTab)
        );
        assert_eq!(
            map_key(KeyCode::BackTab, NONE, Focus::Detail, false),
            Some(Action::PrevTab)
        );
    }

    #[test]
    fn map_key_tab_keys_are_inert_on_activities() {
        assert_eq!(map_key(KeyCode::Tab, NONE, Focus::Activities, false), None);
        assert_eq!(map_key(KeyCode::BackTab, NONE, Focus::Activities, false), None);
        // Positive half under Detail, mirroring the paired-test style used
        // for the other focus-gated keys -- an implementation that mapped
        // Tab/BackTab globally would fail loudly here, not just pass the
        // Activities-inert half by accident.
        assert_eq!(
            map_key(KeyCode::Tab, NONE, Focus::Detail, false),
            Some(Action::NextTab)
        );
        assert_eq!(
            map_key(KeyCode::BackTab, NONE, Focus::Detail, false),
            Some(Action::PrevTab)
        );
    }

    #[test]
    fn map_key_back_tab_works_with_or_without_the_shift_modifier() {
        // D-1: BackTab must select the previous tab whether or not the
        // terminal reports SHIFT alongside it -- crossterm's unix legacy
        // path attaches SHIFT to BackTab, its Kitty path may not, and
        // map_key discards SHIFT before matching (mirrors
        // map_key_shift_g_works_with_or_without_the_shift_modifier).
        assert_eq!(
            map_key(KeyCode::BackTab, KeyModifiers::SHIFT, Focus::Detail, false),
            Some(Action::PrevTab)
        );
        assert_eq!(
            map_key(KeyCode::BackTab, KeyModifiers::NONE, Focus::Detail, false),
            Some(Action::PrevTab)
        );
    }

    #[test]
    fn map_key_tab_is_inert_while_help_is_open() {
        assert_eq!(map_key(KeyCode::Tab, NONE, Focus::Detail, true), None);
        assert_eq!(map_key(KeyCode::BackTab, NONE, Focus::Detail, true), None);
    }

    #[test]
    fn key_hints_are_all_really_mapped() {
        for hint in KEY_HINTS {
            let focus = match hint.scope {
                HintScope::Detail => Focus::Detail,
                HintScope::Activities | HintScope::Global => Focus::Activities,
            };
            let modifiers = if hint.ctrl { KeyModifiers::CONTROL } else { KeyModifiers::NONE };
            assert!(
                map_key(hint.code, modifiers, focus, false).is_some(),
                "advertised key hint {:?} (scope {:?}) has no live binding in map_key",
                hint.code,
                hint.scope
            );
        }
    }
}
