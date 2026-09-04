//! The disposal plan.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::app::AppState;

use super::{block_lines, countdown, text_pane};

pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(sell) = &state.sell else {
        let text = if state.jobs.active.is_some() {
            "planning the sale… Ctrl-L shows what it is saying"
        } else {
            "no sale has been planned: Enter opens the form in sell mode"
        };
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Sell ")),
            area,
        );
        return;
    };
    let mut lines = vec![
        format!("aboard: {}", sell.aboard),
        format!(
            "re-plan {}  {}{}",
            if sell.auto { "on (s stops it)" } else { "off (s starts it, R once)" },
            countdown(if sell.auto { sell.next_due_ms } else { f64::INFINITY }, state.now_ms),
            if sell.rounding { "  reading…" } else { "" },
        ),
    ];
    if let Some(last) = &sell.last_round {
        lines.push(last.clone());
    }
    lines.push(String::new());
    lines.extend(block_lines(&sell.blocks, area.width.saturating_sub(2)));
    text_pane(frame, area, "Sell  c copies the commands", &lines, sell.scroll);
}
