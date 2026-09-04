//! The ranking, as a table with a live column.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use edm_core::js::{format_integer, to_fixed_1};

use crate::tui::app::{AppState, LiveStatus};

use super::{block_lines, countdown, text_pane};

#[expect(clippy::too_many_lines, reason = "one screen, drawn top to bottom")]
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(results) = &state.results else {
        let mut lines = vec![Line::raw("no search has run yet: Enter opens the form")];
        if let Some(job) = &state.jobs.active {
            lines = vec![Line::styled(format!("running {job}…"), Style::new().fg(Color::Yellow))];
            if let Some(progress) = &state.jobs.solving {
                lines.push(Line::raw(progress.as_str()));
            }
            lines.push(Line::styled("Esc cancels; Ctrl-L shows what it is saying", Style::new().fg(Color::DarkGray)));
        }
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Results ")),
            area,
        );
        return;
    };
    let wide = area.width >= 140;
    let rows = if wide {
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area)
    } else {
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).split(area)
    };

    let header_text = format!(
        " Results  sort: {}{}  {}  next {} ",
        results.sort.label(),
        if results.filter.is_empty() { String::new() } else { format!("  filter: {}", results.filter) },
        if results.quick {
            if results.auto { "auto-refresh on" } else { "auto-refresh off (f)" }
        } else {
            "survey"
        },
        if results.quick { countdown(results.next_due_ms, state.now_ms) } else { "off".to_owned() },
    );
    let block = Block::default().borders(Borders::ALL).title(header_text);
    let inner = block.inner(rows[0]);
    frame.render_widget(block, rows[0]);
    let visible = results.visible();
    let narrow = inner.width < 110;
    let header = Row::new(
        if narrow {
            vec!["#", "Route", "Cargo", "Profit", "cr/h", "Live"]
        } else {
            vec!["#", "From", "To", "Cargo", "Tons", "Buy", "Sell", "Profit", "cr/h", "Ly", "To start", "Live"]
        }
        .into_iter()
        .map(|h| Cell::from(h).style(Style::new().add_modifier(Modifier::BOLD))),
    );
    let table_rows: Vec<Row<'_>> = visible
        .iter()
        .enumerate()
        .map(|(n, index)| {
            let row = &results.rows[*index];
            let card = &row.card;
            let leg = card.legs.first();
            let status_style = match row.status {
                LiveStatus::Live => Style::new().fg(Color::Green),
                LiveStatus::Cached => Style::new().fg(Color::Yellow),
                LiveStatus::Unpriced => Style::new().fg(Color::Red),
                LiveStatus::Verifying => Style::new().fg(Color::Cyan),
            };
            let pinned = state.pins.iter().any(|pin| pin.key == card.key);
            let rank = format!("{}{}", if pinned { "*" } else { "" }, row.rank);
            let flown: f64 = card.legs.iter().map(|l| l.distance_ly).sum();
            let approach = card.approach_ly.map_or_else(|| "-".to_owned(), to_fixed_1);
            let cells: Vec<Cell<'_>> = if narrow {
                vec![
                    Cell::from(rank),
                    Cell::from(card.path()),
                    Cell::from(card.cargo()),
                    Cell::from(format_integer(card.profit as f64)),
                    Cell::from(format_integer(card.per_hour as f64)),
                    Cell::from(row.status.label()).style(status_style),
                ]
            } else {
                vec![
                    Cell::from(rank),
                    Cell::from(leg.map_or(String::new(), |l| format!("{} ({})", l.from, l.from_system))),
                    Cell::from(card.legs.last().map_or(String::new(), |l| format!("{} ({})", l.to, l.to_system))),
                    Cell::from(card.cargo()),
                    Cell::from(leg.map_or(String::new(), |l| format_integer(l.units as f64))),
                    Cell::from(leg.map_or(String::new(), |l| format_integer(l.buy as f64))),
                    Cell::from(leg.map_or(String::new(), |l| format_integer(l.sell as f64))),
                    Cell::from(format_integer(card.profit as f64)),
                    Cell::from(format_integer(card.per_hour as f64)),
                    Cell::from(to_fixed_1(flown)),
                    Cell::from(approach),
                    Cell::from(row.status.label()).style(status_style),
                ]
            };
            let mut styled = Row::new(cells);
            if n == results.selected {
                styled = styled.style(Style::new().add_modifier(Modifier::REVERSED));
            } else if row.status == LiveStatus::Unpriced {
                styled = styled.style(Style::new().fg(Color::DarkGray));
            }
            styled
        })
        .collect();
    let widths: Vec<Constraint> = if narrow {
        vec![
            Constraint::Length(4),
            Constraint::Min(24),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(9),
        ]
    } else {
        vec![
            Constraint::Length(4),
            Constraint::Min(22),
            Constraint::Min(22),
            Constraint::Length(14),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(9),
        ]
    };
    let table_area = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(2));
    frame.render_widget(Table::new(table_rows, widths).header(header), table_area);
    let mut foot = vec![Line::styled(
        " Enter pins & opens  p pin  c copy commands  / filter  r P d s t sort  R refresh  f auto ",
        Style::new().fg(Color::DarkGray),
    )];
    if let Some(last) = &results.last_round {
        foot.push(Line::styled(last.as_str(), Style::new().fg(Color::Yellow)));
    } else if let Some(progress) = &state.jobs.solving {
        foot.push(Line::raw(progress.as_str()));
    }
    let foot_area = Rect::new(inner.x, inner.y + inner.height.saturating_sub(2), inner.width, 2.min(inner.height));
    frame.render_widget(Paragraph::new(foot), foot_area);

    // The selected route's legs, then the candidate and coverage notes.
    let mut lines: Vec<String> = Vec::new();
    if let Some(row) = results.selected_row() {
        lines.extend(block_lines(&row.card.legs_blocks, rows[1].width.saturating_sub(2)));
        for command in &row.card.commands {
            lines.push(format!("  {command}"));
        }
        lines.push(String::new());
        lines.push(format!("  {}; {}", row.card.guarantee, row.card.caveats.join("; ")));
        lines.push(String::new());
    }
    lines.extend(block_lines(&results.notes, rows[1].width.saturating_sub(2)));
    text_pane(frame, rows[1], "Route and notes  PgUp/PgDn", &lines, results.notes_scroll);
    let _ = Span::raw("");
}
