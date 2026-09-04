//! One pinned route, kept fresh.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};

use edm_core::js::{format_integer, to_fixed_1};
use edm_core::render::views::bracket_meter;

use crate::tui::app::AppState;

use super::{ago, block_lines, countdown, text_pane};

#[expect(clippy::too_many_lines, reason = "one screen, drawn top to bottom")]
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let Some(pin) = state.pins.get(state.detail) else {
        frame.render_widget(
            Paragraph::new("nothing is pinned: pin a route from the results with Enter or p")
                .block(Block::default().borders(Borders::ALL).title(" Detail ")),
            area,
        );
        return;
    };
    let title = format!(
        " {} of {}  {}  refresh {}{} ",
        state.detail + 1,
        state.pins.len(),
        pin.label,
        countdown(pin.next_due_ms, state.now_ms),
        if pin.refreshing { "  reading…" } else { "" },
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let wide = inner.width >= 140;
    let panes = if wide {
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner)
    } else {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner)
    };

    // Left: the route.
    let mut left: Vec<String> = Vec::new();
    match &pin.card {
        Some(card) => {
            left.push(format!(
                "{}  profit {} cr  {} cr/h{}",
                card.path(),
                format_integer(card.profit as f64),
                format_integer(card.per_hour as f64),
                card.steady_per_hour
                    .map_or_else(String::new, |steady| format!("  steady {} cr/h", format_integer(steady as f64))),
            ));
            left.push(format!(
                "lap {}  first lap {}{}",
                edm_core::spend::duration_estimate(card.lap_millis as f64 / 1_000.0),
                edm_core::spend::duration_estimate(card.first_lap_millis as f64 / 1_000.0),
                card.approach_ly
                    .map_or_else(String::new, |ly| format!("  {} Ly from the ship", to_fixed_1(ly))),
            ));
            left.push(format!("{}; {}", card.guarantee, card.caveats.join("; ")));
            left.push(String::new());
            left.extend(block_lines(&card.legs_blocks, panes[0].width.saturating_sub(2)));
            left.push(String::new());
            left.push("TRADE COMMANDS  c copies all, 1..9 copies one".to_owned());
            for (n, command) in card.commands.iter().enumerate() {
                left.push(format!("  {}  {command}", n + 1));
            }
        }
        None => {
            left.push("not priced yet".to_owned());
        }
    }
    if let Some(reason) = pin.unpriced_reason() {
        left.push(String::new());
        left.push(format!(
            "UNPRICED since {}: {reason}",
            pin.unpriced_since_ms.map_or_else(|| "?".to_owned(), |at| ago(at, state.now_ms)),
        ));
        left.push("kept and re-read on the interval; u unpins".to_owned());
    }
    if let Some(state_) = &pin.state {
        left.push(String::new());
        left.push(format!(
            "last read {} ({} requests)",
            ago(state_.refreshed_at_ms, state.now_ms),
            state_.requests
        ));
    }
    left.push(String::new());
    left.push(state.ship_line());

    let left_area = if pin.history.len() >= 2 {
        let split = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).split(panes[0]);
        let data: Vec<u64> = pin
            .history
            .iter()
            .map(|(_, per_hour)| (*per_hour).max(0) as u64)
            .collect();
        frame.render_widget(
            Sparkline::default()
                .data(&data)
                .style(Style::new().fg(Color::Cyan))
                .block(Block::default().borders(Borders::TOP).title(format!(
                    " cr/h over the last {} reads ",
                    pin.history.len()
                ))),
            split[1],
        );
        split[0]
    } else {
        panes[0]
    };
    text_pane(frame, left_area, "Route  [ ] switch pin  R refresh  u unpin", &left, state.detail_scroll);

    // Right: the markets.
    let mut right: Vec<Line<'_>> = Vec::new();
    match &pin.state {
        Some(state_) => {
            for market in &state_.markets {
                right.push(Line::from(vec![
                    Span::styled(format!("{} ({})", market.station, market.system), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!(
                            "  {}{}{}",
                            market.station_type.as_deref().unwrap_or("?"),
                            market.pad.map_or_else(String::new, |pad| format!("  pad {}", ["-", "S", "M", "L"].get(pad as usize).unwrap_or(&"?"))),
                            market.arrival_ls.map_or_else(String::new, |ls| format!("  {} Ls", format_integer(ls))),
                        ),
                        Style::new().fg(Color::DarkGray),
                    ),
                ]));
                right.push(Line::styled(
                    format!(
                        "  {}{}",
                        market.status,
                        market.read_at_ms.map_or_else(String::new, |at| format!(", {}", ago(at, state.now_ms))),
                    ),
                    Style::new().fg(Color::DarkGray),
                ));
                if let Some(access) = &market.access {
                    let style = if access.starts_with("open") {
                        Style::new().fg(Color::Green)
                    } else if access.starts_with("restricted") {
                        Style::new().fg(Color::Red)
                    } else {
                        Style::new().fg(Color::Yellow)
                    };
                    right.push(Line::styled(format!("  docking: {access}"), style));
                }
                if let Some(door) = &market.door {
                    right.push(Line::styled(format!("  journal: {door}"), Style::new().fg(Color::Cyan)));
                }
                right.push(Line::styled(
                    format!("  {:<22} {:>8} {:>4} {:>8} {:>4} {:>8} {:>8}", "commodity", "stock", "", "demand", "", "buy", "sell"),
                    Style::new().fg(Color::DarkGray),
                ));
                for row in &market.rows {
                    right.push(Line::raw(format!(
                        "  {:<22} {:>8} {:>4} {:>8} {:>4} {:>8} {:>8}",
                        row.name,
                        format_integer(row.stock),
                        bracket_meter(row.stock_bracket),
                        format_integer(row.demand),
                        bracket_meter(row.demand_bracket),
                        format_integer(row.buy),
                        format_integer(row.sell),
                    )));
                }
                right.push(Line::raw(""));
            }
        }
        None => right.push(Line::raw(if pin.refreshing { "reading the markets…" } else { "not read yet; R reads now" })),
    }
    frame.render_widget(
        Paragraph::new(right).block(Block::default().borders(Borders::ALL).title(" Markets ")),
        panes[1],
    );
}
