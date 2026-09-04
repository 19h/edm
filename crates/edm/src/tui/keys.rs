//! Key presses, named.
//!
//! The reducer never sees a key: it sees what the key *means* on the screen it
//! landed on, so the same binding table is what the help overlay prints.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::Screen;

/// What a key asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Quit,
    Help,
    ToggleLogStrip,
    Go(Screen),
    Back,
    /// Move focus within the current screen.
    Up,
    Down,
    Left,
    Right,
    Next,
    Previous,
    /// Confirm, run, open.
    Enter,
    /// Toggle the focused switch or cycle the focused choice.
    Space,
    Backspace,
    Delete,
    Home,
    End,
    /// Typed text, on a screen with a text field focused.
    Type(char),
    PageUp,
    PageDown,
}

/// The action a key means on `screen`, if any.
///
/// `typing` says whether a text field owns plain characters there: on the
/// search form every printable key is text, and only the modified keys and
/// the function row reach the rest of the program.
#[expect(
    clippy::match_same_arms,
    reason = "one arm per key, in the order the help prints them"
)]
pub(crate) fn action(key: &KeyEvent, screen: Screen, typing: bool) -> Option<Action> {
    // Presses only. A terminal that reports releases, or repeats a held key
    // as its own event, would otherwise type every character twice.
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let action = match key.code {
        KeyCode::Char('c' | 'q') if ctrl => Action::Quit,
        KeyCode::Char('l') if ctrl => Action::ToggleLogStrip,
        KeyCode::F(1) => Action::Help,
        KeyCode::F(2) => Action::Go(Screen::Search),
        KeyCode::F(3) => Action::Go(Screen::Results),
        KeyCode::F(4) => Action::Go(Screen::Detail),
        KeyCode::F(5) => Action::Go(Screen::Pins),
        KeyCode::F(6) => Action::Go(Screen::Sell),
        KeyCode::F(7) => Action::Go(Screen::Log),
        KeyCode::Esc => Action::Back,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Tab => Action::Next,
        KeyCode::BackTab => Action::Previous,
        KeyCode::Enter => Action::Enter,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Char(' ') if !typing => Action::Space,
        KeyCode::Char(c) if typing && !ctrl => Action::Type(c),
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('1') => Action::Go(Screen::Search),
        KeyCode::Char('2') => Action::Go(Screen::Results),
        KeyCode::Char('3') => Action::Go(Screen::Detail),
        KeyCode::Char('4') => Action::Go(Screen::Pins),
        KeyCode::Char('5') => Action::Go(Screen::Sell),
        KeyCode::Char('l' | 'L') => Action::Go(Screen::Log),
        KeyCode::Char(c) => Action::Type(c),
        _ => return None,
    };
    let _ = screen;
    Some(action)
}

/// The key table the help overlay prints for `screen`.
pub(crate) fn bindings(screen: Screen) -> Vec<(&'static str, &'static str)> {
    let mut rows = vec![
        ("Ctrl-C / Ctrl-Q", "quit"),
        ("F1 / ?", "this help"),
        ("F2..F7", "search, results, detail, pins, sell, log"),
        ("1..5, L", "the same, where nothing is being typed"),
        ("Ctrl-L", "show or hide the log strip"),
        ("Esc", "back, or close this"),
    ];
    rows.extend(match screen {
        Screen::Search => vec![
            ("Tab / Shift-Tab, Up / Down", "move between fields"),
            ("Space, Left / Right", "toggle a switch, cycle a choice"),
            ("Enter", "run the search"),
        ],
        Screen::Results => vec![
            ("Up / Down", "select a route"),
            ("Enter", "pin the route and open it"),
            ("p", "pin or unpin in place"),
            ("R", "re-read the shortlist now"),
            ("f", "keep re-reading the shortlist on the interval"),
            ("c", "copy the route's trade commands"),
            ("r / P / d / t", "sort by rate, profit, distance, time"),
        ],
        Screen::Detail => vec![
            ("R", "re-read this route now"),
            ("u", "unpin"),
            ("[ / ]", "previous / next pin"),
            ("c", "copy the trade commands"),
        ],
        Screen::Pins => vec![
            ("Up / Down, Enter", "select, open"),
            ("d", "delete the pin"),
            ("o", "re-open the search that found it"),
        ],
        Screen::Sell => vec![
            ("R", "re-plan now"),
            ("s", "keep re-planning on the interval"),
            ("c", "copy the trade commands"),
        ],
        Screen::Log => vec![("Up / Down, PageUp / PageDown", "scroll")],
    });
    rows
}
