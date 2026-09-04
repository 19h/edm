//! Drawing. Reads [`AppState`], writes a frame, decides nothing.

pub(crate) mod detail;
pub(crate) mod pins;
pub(crate) mod results;
pub(crate) mod search;
pub(crate) mod sell;
#[cfg(test)]
mod tests;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use edm_core::js::text::Metric;
use edm_core::render::write_blocks;

use crate::out::Stream;

use super::app::{AppState, Modal, Screen};
use super::keys;

/// How many lines the log strip takes when shown.
const LOG_STRIP_LINES: u16 = 6;

pub(crate) fn draw(frame: &mut Frame<'_>, state: &AppState) {
    let area = frame.area();
    let mut constraints = vec![Constraint::Length(1), Constraint::Min(3)];
    if state.log_strip && state.screen != Screen::Log {
        constraints.push(Constraint::Length(LOG_STRIP_LINES));
    }
    constraints.push(Constraint::Length(1));
    let rows = Layout::vertical(constraints).split(area);
    title_bar(frame, rows[0], state);
    match state.screen {
        Screen::Search => search::draw(frame, rows[1], state),
        Screen::Results => results::draw(frame, rows[1], state),
        Screen::Detail => detail::draw(frame, rows[1], state),
        Screen::Pins => pins::draw(frame, rows[1], state),
        Screen::Sell => sell::draw(frame, rows[1], state),
        Screen::Log => log_pane(frame, rows[1], state, false),
    }
    if state.log_strip && state.screen != Screen::Log {
        log_pane(frame, rows[2], state, true);
    }
    status_bar(frame, rows[rows.len() - 1], state);
    if let Some(modal) = &state.modal {
        draw_modal(frame, area, state, modal);
    }
}

fn title_bar(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mut spans = vec![Span::styled(" edm ", Style::new().add_modifier(Modifier::BOLD).fg(Color::Black).bg(Color::Cyan))];
    for (n, screen) in [
        Screen::Search,
        Screen::Results,
        Screen::Detail,
        Screen::Pins,
        Screen::Sell,
        Screen::Log,
    ]
    .into_iter()
    .enumerate()
    {
        let key = if screen == Screen::Log { "L".to_owned() } else { (n + 1).to_string() };
        let label = format!(" {key}:{} ", screen.title());
        spans.push(if screen == state.screen {
            Span::styled(label, Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan))
        } else {
            Span::styled(label, Style::new().fg(Color::DarkGray))
        });
    }
    if let Some(job) = &state.jobs.active {
        spans.push(Span::styled(format!("  ⟳ {job}"), Style::new().fg(Color::Yellow)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn status_bar(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let requests = format!(
        " requests {} / {} ",
        edm_core::js::format_integer(state.spent as f64),
        edm_core::js::format_integer(state.ceiling),
    );
    let ship = format!(" {} ", state.ship_line());
    let hint = " F1 help  Ctrl-L log  Ctrl-C quit ";
    let line = Line::from(vec![
        Span::styled(requests, Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::styled(ship, Style::new().fg(Color::Cyan)),
        Span::styled(hint, Style::new().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Blocks, rendered by the console renderer at the pane's width.
pub(crate) fn block_lines(blocks: &[edm_core::render::Block<'_>], width: u16) -> Vec<String> {
    let mut text = String::new();
    write_blocks(&mut text, blocks, usize::from(width).max(48), Metric::Display);
    text.lines().map(ToOwned::to_owned).collect()
}

/// `n` seconds from now, in words.
pub(crate) fn countdown(due_ms: f64, now_ms: f64) -> String {
    if !due_ms.is_finite() {
        return "off".to_owned();
    }
    let seconds = ((due_ms - now_ms) / 1_000.0).ceil();
    if seconds <= 0.0 {
        "due".to_owned()
    } else {
        format!("in {}", edm_core::spend::duration_estimate(seconds))
    }
}

/// How long ago, in words.
pub(crate) fn ago(at_ms: f64, now_ms: f64) -> String {
    let seconds = edm_core::js::js_max((now_ms - at_ms) / 1_000.0, 0.0);
    format!("{} ago", edm_core::spend::duration_estimate(seconds))
}

/// The log, in full or as a strip along the bottom.
fn log_pane(frame: &mut Frame<'_>, area: Rect, state: &AppState, strip: bool) {
    let block = Block::default().borders(Borders::TOP).title(if strip { "log" } else { "Log" });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = usize::from(inner.height);
    let scroll = if strip { 0 } else { state.log_scroll };
    let end = state.log.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    let lines: Vec<Line<'_>> = state.log.range(start..end).map(|entry| {
        let style = match entry.stream {
            Stream::Stdout => Style::new(),
            Stream::Stderr => Style::new().fg(Color::Yellow),
        };
        Line::styled(entry.text.as_str(), style)
    }).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_modal(frame: &mut Frame<'_>, area: Rect, state: &AppState, modal: &Modal) {
    let (title, lines, footer): (&str, Vec<Line<'_>>, &str) = match modal {
        Modal::Help => (
            "Keys",
            keys::bindings(state.screen)
                .into_iter()
                .map(|(key, what)| {
                    Line::from(vec![
                        Span::styled(format!("{key:<28}"), Style::new().add_modifier(Modifier::BOLD)),
                        Span::raw(what),
                    ])
                })
                .collect(),
            " Esc closes ",
        ),
        Modal::Message(text) => ("Notice", text.lines().map(Line::from).collect(), " Esc closes "),
        Modal::Confirm { lines, message } => {
            let mut all: Vec<Line<'_>> = lines.iter().map(|line| Line::from(line.as_str())).collect();
            all.push(Line::raw(""));
            all.push(Line::styled(message.as_str(), Style::new().add_modifier(Modifier::BOLD).fg(Color::Yellow)));
            ("Send these requests?", all, " y / Enter sends   n / Esc declines ")
        }
        Modal::Copied(text) => {
            let mut all = vec![Line::styled(
                "sent to the clipboard through the terminal (OSC 52); if it did not arrive, this is the text:",
                Style::new().fg(Color::DarkGray),
            )];
            all.extend(text.lines().map(Line::from));
            ("Copied", all, " Esc closes ")
        }
    };
    let width = area.width.saturating_sub(4).clamp(20, 110);
    let height = (lines.len() as u16 + 4).min(area.height.saturating_sub(2)).max(5);
    let popup = centred(area, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .title_bottom(Line::from(footer).right_aligned());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), popup);
}

pub(crate) fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

/// A scrollable list of plain lines with a border.
pub(crate) fn text_pane(frame: &mut Frame<'_>, area: Rect, title: &str, lines: &[String], scroll: usize) {
    let block = Block::default().borders(Borders::ALL).title(format!(" {title} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = usize::from(inner.height);
    let start = scroll.min(lines.len().saturating_sub(height));
    let shown: Vec<Line<'_>> = lines.iter().skip(start).take(height).map(|line| Line::from(line.as_str())).collect();
    frame.render_widget(Paragraph::new(shown), inner);
}
