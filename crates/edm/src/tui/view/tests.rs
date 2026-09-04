//! Every screen, drawn into a test backend and pinned as text.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::out::Stream;
use crate::tui::app::{AppState, Modal, Screen};

fn frame(state: &AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal
        .draw(|frame| super::draw(frame, state))
        .expect("a frame");
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        text.push_str(line.trim_end_matches(' '));
        text.push('\n');
    }
    text
}

fn state() -> AppState {
    let mut state = AppState::new(2_000.0, 60.0);
    state.now_ms = 1_700_000_000_000.0;
    state.size = (120, 40);
    state.log(Stream::Stdout, "resolving \"Sol\" through Ardent for quick lookup...");
    state.log(Stream::Stderr, "warning: something the pipeline said on stderr");
    state
}

#[test]
fn the_search_form_at_two_widths() {
    let mut state = state();
    state.search.fields[0].text = "Sol".to_owned();
    state.search.fields[2].text = "gold, silver".to_owned();
    insta::assert_snapshot!("search_wide", frame(&state, 130, 36));
    insta::assert_snapshot!("search_narrow", frame(&state, 80, 36));
}

#[test]
fn the_empty_screens_say_what_to_do() {
    let mut state = state();
    for screen in [Screen::Results, Screen::Detail, Screen::Pins, Screen::Sell, Screen::Log] {
        state.screen = screen;
        insta::assert_snapshot!(format!("empty_{}", screen.title().to_lowercase()), frame(&state, 100, 20));
    }
}

#[test]
fn the_modals_and_the_log_strip() {
    let mut state = state();
    state.log_strip = true;
    state.modal = Some(Modal::Confirm {
        lines: vec!["== ROUTE PLAN ==".to_owned(), "| requests | 300 |".to_owned()],
        message: "pass --yes to send 300 requests to the game-internal API".to_owned(),
    });
    insta::assert_snapshot!("confirm_modal", frame(&state, 100, 30));
    state.modal = Some(Modal::Help);
    insta::assert_snapshot!("help_modal", frame(&state, 100, 30));
}
