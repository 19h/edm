//! Every pinned route.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use edm_core::js::format_integer;

use crate::tui::app::AppState;
use crate::tui::engine::cards::RouteCard;

use super::{ago, countdown};

pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Pins  Enter opens  d deletes  R refreshes  o re-opens its search ");
    if state.pins.is_empty() {
        frame.render_widget(
            Paragraph::new("nothing is pinned yet").block(block),
            area,
        );
        return;
    }
    let header = Row::new(
        ["Route", "Cargo", "cr/h", "Profit", "Status", "Read", "Next"]
            .into_iter()
            .map(|h| Cell::from(h).style(Style::new().add_modifier(Modifier::BOLD))),
    );
    let rows: Vec<Row<'_>> = state
        .pins
        .iter()
        .enumerate()
        .map(|(n, pin)| {
            let (status, style) = if pin.refreshing {
                ("reading…".to_owned(), Style::new().fg(Color::Cyan))
            } else if let Some(reason) = pin.unpriced_reason() {
                (format!("unpriced: {reason}"), Style::new().fg(Color::Red))
            } else if pin.state.is_some() {
                ("priced".to_owned(), Style::new().fg(Color::Green))
            } else {
                ("from the search".to_owned(), Style::new().fg(Color::Yellow))
            };
            let card = pin.card.as_ref();
            let mut row = Row::new(vec![
                Cell::from(card.map_or_else(|| pin.key.describe(&[]), RouteCard::path)),
                Cell::from(card.map_or_else(|| pin.key.commodities.join(", "), RouteCard::cargo)),
                Cell::from(pin.per_hour().map_or_else(|| "-".to_owned(), |rate| format_integer(rate as f64))),
                Cell::from(card.map_or_else(|| pin.last.as_ref().map_or_else(|| "-".to_owned(), |last| format_integer(last.profit as f64)), |card| format_integer(card.profit as f64))),
                Cell::from(status).style(style),
                Cell::from(pin.last.as_ref().map_or_else(|| "never".to_owned(), |last| ago(last.refreshed_at_ms, state.now_ms))),
                Cell::from(countdown(pin.next_due_ms, state.now_ms)),
            ]);
            if n == state.pins_selected {
                row = row.style(Style::new().add_modifier(Modifier::REVERSED));
            }
            row
        })
        .collect();
    let widths = [
        Constraint::Min(30),
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Min(18),
        Constraint::Length(12),
        Constraint::Length(10),
    ];
    frame.render_widget(Table::new(rows, widths).header(header).block(block), area);
}
