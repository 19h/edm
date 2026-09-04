//! The search form and its completion popup.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::tui::app::{AppState, FieldKind, Mode};

#[expect(clippy::too_many_lines, reason = "one screen, drawn top to bottom")]
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let form = &state.search;
    let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(4)]).split(area);
    let block = Block::default().borders(Borders::ALL).title(" Search ");
    let inner = block.inner(rows[0]);
    frame.render_widget(block, rows[0]);

    // The mode line spans the form; the fields split into two columns when
    // there is room for both a label and a hint on each side.
    let split = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
    frame.render_widget(Paragraph::new(mode_line(form.mode)), split[0]);
    let inner = split[1];
    let two_columns = inner.width >= 120;
    let columns: Vec<Rect> = if two_columns {
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner)
            .to_vec()
    } else {
        vec![inner]
    };

    let mut lines: Vec<Line<'_>> = Vec::new();
    let label_width = 22;
    // Where the focused row lands, so the popup can sit under it.
    let mut focused_at: (usize, u16) = (0, 0);
    for (n, index) in form.visible().into_iter().enumerate() {
        let field = &form.fields[index];
        let focused = index == form.focus;
        let marker = if focused { "▶ " } else { "  " };
        let value = field.display();
        let value_span = match field.kind {
            FieldKind::Text | FieldKind::Number if focused => {
                Span::styled(format!("{value}▏"), Style::new().fg(Color::White).bg(Color::DarkGray))
            }
            FieldKind::Text | FieldKind::Number if value.is_empty() => {
                Span::styled(field.hint, Style::new().fg(Color::DarkGray))
            }
            _ => Span::styled(value, if focused { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() }),
        };
        if focused {
            focused_at = (n, lines.len() as u16);
        }
        lines.push(Line::from(vec![
            Span::styled(marker, Style::new().fg(Color::Cyan)),
            Span::styled(
                format!("{:<label_width$}", field.label),
                if focused { Style::new().add_modifier(Modifier::BOLD).fg(Color::Cyan) } else { Style::new() },
            ),
            value_span,
        ]));
    }
    let total = lines.len();
    let half = total.div_ceil(2).max(2);
    if two_columns {
        let (left, right) = lines.split_at(half.min(total));
        frame.render_widget(Paragraph::new(left.to_vec()), columns[0]);
        frame.render_widget(Paragraph::new(right.to_vec()), columns[1]);
    } else {
        frame.render_widget(Paragraph::new(lines), columns[0]);
    }

    let focused = form.focused();
    let mut footer = vec![Line::from(vec![
        Span::styled("edm ", Style::new().fg(Color::DarkGray)),
        Span::raw(form.argv().join(" ")),
    ])];
    if !focused.hint.is_empty() {
        footer.push(Line::styled(focused.hint, Style::new().fg(Color::DarkGray)));
    }
    if let Some(status) = &form.status {
        footer.push(Line::styled(status.as_str(), Style::new().fg(Color::Yellow)));
    }
    frame.render_widget(
        Paragraph::new(footer)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::TOP).title(" Enter runs  PgUp/PgDn changes the mode ")),
        rows[1],
    );

    if state.completion.open && !state.completion.items.is_empty() {
        let (_, line_in_column) = focused_at;
        let (column, y_offset) = if two_columns && usize::from(focused_at.1) >= half {
            (columns[1], focused_at.1 - half as u16)
        } else {
            (columns[0], line_in_column)
        };
        let x = column.x + 2 + label_width as u16;
        let y = column.y + y_offset + 1;
        let width = column.width.saturating_sub(2 + label_width as u16).clamp(20, 60);
        let height = (state.completion.items.len() as u16 + 2).min(area.height.saturating_sub(y).max(3));
        let popup = Rect::new(x.min(area.x + area.width.saturating_sub(width)), y.min(area.y + area.height.saturating_sub(height)), width, height);
        frame.render_widget(Clear, popup);
        let lines: Vec<Line<'_>> = state
            .completion
            .items
            .iter()
            .enumerate()
            .map(|(n, item)| {
                let style = if n == state.completion.selected {
                    Style::new().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::new()
                };
                Line::from(vec![
                    Span::styled(format!(" {:<24}", item.label), style),
                    Span::styled(format!(" {:<10}", item.kind.label()), style.fg(Color::DarkGray)),
                    Span::styled(format!(" {}", item.hint), style.fg(Color::DarkGray)),
                ])
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Enter picks, Esc closes ")),
            popup,
        );
    }
}

fn mode_line(current: Mode) -> Line<'static> {
    let mut spans = vec![Span::styled("  Mode                  ", Style::new().add_modifier(Modifier::BOLD))];
    for mode in Mode::ALL {
        let label = format!(" {} ", mode.label());
        spans.push(if mode == current {
            Span::styled(label, Style::new().add_modifier(Modifier::BOLD).fg(Color::Black).bg(Color::Cyan))
        } else {
            Span::styled(label, Style::new().fg(Color::DarkGray))
        });
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}
