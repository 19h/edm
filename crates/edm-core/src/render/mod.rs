//! Terminal rendering: the fitted table, headings and wrapped notes.
//!
//! Everything here is a pure function of its input and a terminal width, so the
//! complete output of any command is a value that can be snapshot-tested at any
//! width with no runtime, no transport and no terminal attached.
//!
//! The width is therefore always a parameter. The TypeScript samples it once
//! into a module-level `TERMINAL_WIDTH` and reads that global from four
//! different functions; here the equivalent sampling lives in
//! [`terminal_width`], which takes the two ambient inputs it depends on and is
//! called once by the binary.

pub mod columns;
pub mod table;
pub mod views;

use crate::js::text::{self, Metric};

pub use table::{Column, Fit, Rendered, Row, fit, fit_with, frame_width, render_table};

/// The floor the terminal width is clamped to, on both discovery paths.
pub const MIN_WIDTH: usize = 48;

/// The width assumed when stdout is not a terminal.
///
/// A hundred, not the conventional eighty: the tables are wide and this is what
/// the TypeScript picks (`game-internal-api.ts:364`).
pub const DEFAULT_WIDTH: usize = 100;

/// The ceiling imposed on a `COLUMNS` override [C11].
///
/// `COLUMNS=99999999999999999999` makes the TypeScript attempt
/// `"=".repeat(1e20)` while initialising a module-level constant — outside
/// `main`'s `try`/`catch`, so it is not even a catchable failure. Refusing the
/// absurd width is the registered divergence.
pub const MAX_WIDTH: usize = 10_000;

/// `TERMINAL_WIDTH` (`game-internal-api.ts:360`) [R31].
///
/// `columns_env` is `$COLUMNS` as the process received it, `tty_columns` the
/// column count from the terminal, or `None` when stdout is not one. An
/// override wins only if it is a run of ASCII digits after trimming; otherwise
/// the terminal is asked, and a terminal that reports nothing usable yields
/// [`DEFAULT_WIDTH`]. Both paths are floored at [`MIN_WIDTH`].
///
/// The TypeScript samples this once at startup and ignores `SIGWINCH`, so a
/// resize mid-sweep does not reflow anything. Calling this once and threading
/// the result is what reproduces that.
#[must_use]
pub fn terminal_width(columns_env: Option<&str>, tty_columns: Option<usize>) -> usize {
    if let Some(raw) = columns_env {
        // `.trim()` here is `String.prototype.trim` [R25], not Rust's.
        let override_value = text::js_trim(raw);
        // `/^\d+$/` — JavaScript's `\d` is ASCII-only even against a fullwidth
        // digit, and the empty string is falsy, so it never reaches the test.
        if !override_value.is_empty() && override_value.bytes().all(|b| b.is_ascii_digit()) {
            let parsed = crate::js::to_number(override_value);
            let floored = crate::js::js_max(MIN_WIDTH as f64, parsed);
            return if floored >= MAX_WIDTH as f64 {
                MAX_WIDTH
            } else {
                floored as usize
            };
        }
    }
    match tty_columns {
        // `Number.isInteger(columns) && columns > 0` — a terminal reporting
        // zero columns is treated as no terminal at all.
        Some(columns) if columns > 0 => columns.max(MIN_WIDTH),
        _ => DEFAULT_WIDTH,
    }
}

/// One renderable unit of output.
///
/// A command's whole output is a `Vec<Block>` plus a width, which is what makes
/// it snapshot-testable without a terminal, a clock or a socket. Each variant
/// corresponds to one emitter in the TypeScript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block<'a> {
    /// `heading(title)` on its own (`game-internal-api.ts:467`).
    Heading(String),
    /// `emitTable` (`game-internal-api.ts:488`): a heading, the frame, and — when
    /// columns had to be dropped — a note naming them.
    Table {
        title: String,
        columns: &'a [Column],
        rows: Vec<Row<'a>>,
    },
    /// `emitNote` (`game-internal-api.ts:473`): indented, word-wrapped commentary.
    Note(String),
    /// A line printed as-is, clamped to the terminal width — the streamed
    /// progress lines of `emitProgressLine` (`game-internal-api.ts:2043`) [R33].
    Line(String),
    /// Text printed verbatim, with no clamping at all.
    ///
    /// R33's clamp applies to progress lines and *only* to progress lines. Two
    /// emitters `console.log` a payload straight out: the full request URL
    /// under `--full-url` (ts:1195) and a decoded non-market body (ts:1296).
    /// Routing a thousand-character URL through [`Block::Line`] would truncate
    /// it to a `~`, which is exactly the sort of quiet wrongness the port
    /// exists to avoid.
    Raw(String),
}

/// Renders blocks into `out`, each line terminated by `\n`.
///
/// `metric` decides how a cell is measured: [`Metric::Utf16`] is the parity
/// path (it is what `String.prototype.length` counts), and [`Metric::Display`]
/// is the opt-in `EDM_WIDTH=display` fix for CJK and emoji alignment.
pub fn write_blocks(out: &mut String, blocks: &[Block<'_>], width: usize, metric: Metric) {
    for block in blocks {
        match block {
            Block::Heading(title) => push_line(out, &heading(title, width, metric)),
            Block::Note(text) => {
                for line in wrap_note(text, width, metric) {
                    push_line(out, &line);
                }
            }
            Block::Line(line) => {
                push_line(out, &text::clamp(line, width.cast_signed(), metric));
            }
            Block::Raw(text) => push_line(out, text),
            Block::Table {
                title,
                columns,
                rows,
            } => {
                push_line(out, &heading(title, width, metric));
                let rendered = render_table(columns, rows, width, metric);
                for line in &rendered.lines {
                    push_line(out, line);
                }
                if !rendered.fit.omitted.is_empty() {
                    // The width is interpolated raw, so it is never grouped —
                    // `columns hidden to fit 1000 cols`, not `1,000` [R36].
                    let note = format!(
                        "columns hidden to fit {width} cols: {}",
                        rendered.fit.omitted.join(", ")
                    );
                    for line in wrap_note(&note, width, metric) {
                        push_line(out, &line);
                    }
                }
            }
        }
    }
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

/// `heading` (`game-internal-api.ts:467`) [R28].
///
/// `== TITLE ` padded out to the terminal width with `=`. The comparison is
/// `>=`, so a label that exactly fills the terminal gets no padding at all —
/// one unit narrower and it gets a single `=`. Several titles contain U+2014
/// (em dash), which is one UTF-16 unit and so does not perturb this, though it
/// is two display columns wide under [`Metric::Display`].
#[must_use]
pub fn heading(title: &str, width: usize, metric: Metric) -> String {
    let label = format!("== {title} ");
    let measured = metric.of_str(&label);
    if measured >= width {
        return label;
    }
    let mut out = label;
    out.extend(core::iter::repeat_n('=', width - measured));
    out
}

/// The indent `emitNote` prefixes every line with.
const NOTE_INDENT: &str = "   ";

/// `emitNote` (`game-internal-api.ts:473`) [R29], as the lines it would print.
///
/// A greedy wrap with no cleverness whatsoever, and the lack of cleverness is
/// the specification:
///
/// - it splits on a *single space* with no filtering, so an empty token is a
///   real word — a doubled space survives into the output, and a leading space
///   is dropped because the first token is empty and the "line is empty" branch
///   simply overwrites it;
/// - a word longer than the limit is emitted on its own, overflowing;
/// - `wrap_note("")` yields nothing at all, not a blank line;
/// - the limit is `max(20, width - 3)` measured *without* the three-space
///   indent that is then prepended, so a wrapped line reaches `width` exactly
///   at best and a long word can exceed it.
#[must_use]
pub fn wrap_note(text: &str, width: usize, metric: Metric) -> Vec<String> {
    // `Math.max(20, TERMINAL_WIDTH - 3)`: below a width of 3 the TypeScript
    // computes a negative difference and takes 20, which is what saturation
    // gives here.
    let limit = width.saturating_sub(NOTE_INDENT.len()).max(20);

    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if line.is_empty() {
            line.push_str(word);
        } else if metric.of_str(&line) + 1 + metric.of_str(word) <= limit {
            line.push(' ');
            line.push_str(word);
        } else {
            lines.push(format!("{NOTE_INDENT}{line}"));
            word.clone_into(&mut line);
        }
    }
    if !line.is_empty() {
        lines.push(format!("{NOTE_INDENT}{line}"));
    }
    lines
}
