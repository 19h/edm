//! `renderTable` (`game-internal-api.ts:402`) — column fitting and the ASCII frame.
//!
//! The table is drawn to a fixed width. When the natural widths do not fit, the
//! renderer first squeezes columns that declared a floor, and only when nothing
//! can be squeezed any further does it start throwing columns away. Both of
//! those choices are made in a way that looks accidental and is load-bearing;
//! see [`fit`].

use std::borrow::Cow;

use crate::js::text::{self, Align, Metric};

/// One column of a table: what it is called, how it is aligned, and how much it
/// is allowed to be squeezed before it is dropped altogether.
///
/// Built with the chained constructors so that a column set reads in the same
/// order as the object literal it is transcribed from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    /// Identifies the cell within a row. Carried for diagnostics and for
    /// [`Fit::active`] consumers; the renderer itself indexes rows positionally.
    pub key: &'static str,
    /// The text drawn in the header, and the name reported when the column is
    /// dropped — the hidden-columns note lists headers, not keys.
    pub header: &'static str,
    pub align: Align,
    /// Higher is dropped first. Zero means the column is never dropped.
    pub priority: u32,
    /// The floor the column may be squeezed to. **A column without one cannot
    /// be squeezed at all** — see [`fit`].
    pub min_width: Option<usize>,
    /// A ceiling applied to the measured content width, before the floor.
    pub max_width: Option<usize>,
}

impl Column {
    /// A left-aligned, undroppable, unbounded column.
    #[must_use]
    pub const fn new(key: &'static str, header: &'static str) -> Self {
        Self { key, header, align: Align::Left, priority: 0, min_width: None, max_width: None }
    }

    #[must_use]
    pub const fn right(mut self) -> Self {
        self.align = Align::Right;
        self
    }

    #[must_use]
    pub const fn priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    #[must_use]
    pub const fn min_width(mut self, width: usize) -> Self {
        self.min_width = Some(width);
        self
    }

    #[must_use]
    pub const fn max_width(mut self, width: usize) -> Self {
        self.max_width = Some(width);
        self
    }
}

/// A row of the table.
///
/// `Band` is a full-width label spanning the whole frame (a category heading),
/// `Rule` a horizontal divider. Cells in `Data` are positional: cell *i*
/// belongs to column *i* of the **full** column set, so dropping a column does
/// not renumber the rest. A row shorter than the column set is padded with
/// empty cells, which is what `row.cells[column.key] ?? ""` does in the
/// TypeScript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row<'a> {
    Data(Vec<Cow<'a, str>>),
    Band(Cow<'a, str>),
    Rule,
}

impl<'a> Row<'a> {
    /// A data row from anything string-shaped.
    pub fn data<I>(cells: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Cow<'a, str>>,
    {
        Self::Data(cells.into_iter().map(Into::into).collect())
    }

    /// A band row from anything string-shaped.
    pub fn band(text: impl Into<Cow<'a, str>>) -> Self {
        Self::Band(text.into())
    }

    fn cell(&self, index: usize) -> &str {
        match self {
            Self::Data(cells) => cells.get(index).map_or("", Cow::as_ref),
            _ => "",
        }
    }
}

/// The outcome of fitting a column set to an available width.
///
/// Returned rather than folded into the rendered lines so that the three
/// invariants of [`fit`] can be asserted directly instead of inferred from
/// alignment in the output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fit {
    /// Indices into the original column set, in order, of the columns that
    /// survived.
    pub active: Vec<usize>,
    /// Drawn width of each surviving column, parallel to [`Fit::active`].
    pub widths: Vec<usize>,
    /// Headers of the dropped columns, in the order they were dropped.
    pub omitted: Vec<&'static str>,
    /// Iterations the shrink/drop loop took.
    ///
    /// Not part of the rendered output. The loop is the only unbounded
    /// construct in the renderer, and exposing its step count is what lets a
    /// property test pin the bound instead of trusting an argument about it.
    pub steps: usize,
}

/// Outer width of the frame: each column costs its width plus `"| "` and `" "`,
/// and the frame is closed by a final `"|"` (`game-internal-api.ts:393`).
#[must_use]
pub fn frame_width(widths: &[usize]) -> usize {
    widths.iter().map(|width| width + 3).sum::<usize>() + 1
}

/// `measureColumns` (`game-internal-api.ts:380`).
///
/// The order of the last two steps is observable: `maxWidth` clips the measured
/// content **before** `minWidth` raises the floor, so a column with
/// `max < min` ends up at `min` rather than `max` [R27]. Band rows are skipped,
/// so a long category label never widens a column [R30].
fn measure(columns: &[Column], active: &[usize], rows: &[Row<'_>], metric: Metric) -> Vec<usize> {
    active
        .iter()
        .map(|&index| {
            let column = &columns[index];
            let mut width = metric.of_str(column.header);
            for row in rows {
                if matches!(row, Row::Data(_)) {
                    width = width.max(metric.of_str(row.cell(index)));
                }
            }
            if let Some(ceiling) = column.max_width {
                width = width.min(ceiling);
            }
            width.max(column.min_width.unwrap_or(1))
        })
        .collect()
}

/// Fits `columns` into `available` display units, in the parity metric.
///
/// See [`fit_with`]; this is that function at [`Metric::Utf16`], which is what
/// `String.prototype.length` measures and therefore the parity path [R22].
#[must_use]
pub fn fit(columns: &[Column], rows: &[Row<'_>], available: usize) -> Fit {
    fit_with(columns, rows, available, Metric::Utf16)
}

/// The fitting loop of `renderTable` (`game-internal-api.ts:411`) [R27].
///
/// Three details of this look like bugs and are reproduced deliberately,
/// because every column width in every table depends on them:
///
/// 1. Slack is `width - (minWidth ?? width)`. A column that declared no
///    `minWidth` gets its own width as the floor, so its slack is zero and it
///    can *never* be squeezed — only dropped. Only columns with an explicit
///    floor ever give ground.
/// 2. The column squeezed is the **first** at maximum slack. The TypeScript
///    sorts by slack descending and takes `[0]`, and `Array.prototype.sort` has
///    been stable since ES2019, so ties go to the leftmost.
/// 3. The column dropped is the **first** at maximum priority. The TypeScript
///    folds with a strict `>`, which keeps the incumbent on a tie.
///    `Iterator::max_by_key` returns the *last* maximum and is banned in
///    `clippy.toml` for this reason.
///
/// And one more: after a drop the widths are re-measured from scratch, so every
/// squeeze applied so far is undone and the surviving columns get their natural
/// widths back before the next round of squeezing.
#[must_use]
pub fn fit_with(columns: &[Column], rows: &[Row<'_>], available: usize, metric: Metric) -> Fit {
    let mut active: Vec<usize> = (0..columns.len()).collect();
    let mut widths = measure(columns, &active, rows, metric);
    let mut omitted: Vec<&'static str> = Vec::new();
    let mut steps = 0usize;

    while frame_width(&widths) > available {
        steps += 1;

        // Squeeze before losing a column entirely. Strict `>` keeps the first
        // maximum, matching the stable sort's `[0]`.
        let mut squeeze: Option<(usize, usize)> = None;
        for (position, &index) in active.iter().enumerate() {
            let width = widths[position];
            // `measure` floors at `min_width` and a squeeze never goes below it,
            // so this cannot underflow.
            let slack = width - columns[index].min_width.unwrap_or(width);
            if slack > 0 && squeeze.is_none_or(|(_, best)| slack > best) {
                squeeze = Some((position, slack));
            }
        }
        if let Some((position, slack)) = squeeze {
            let excess = frame_width(&widths) - available;
            widths[position] -= excess.min(slack);
            continue;
        }

        let mut victim: Option<usize> = None;
        for (position, &index) in active.iter().enumerate() {
            let priority = columns[index].priority;
            if priority == 0 {
                continue;
            }
            let better = victim.is_none_or(|held| priority > columns[active[held]].priority);
            if better {
                victim = Some(position);
            }
        }
        let Some(position) = victim else { break };

        omitted.push(columns[active[position]].header);
        active.remove(position);
        widths = measure(columns, &active, rows, metric);
    }

    Fit { active, widths, omitted, steps }
}

/// A drawn table: the frame lines and the fit that produced them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// Frame lines, without trailing newlines. Under [`Metric::Utf16`] every
    /// one of them measures exactly `frame_width(&fit.widths)`; under
    /// [`Metric::Display`] a cell can fall short when a wide character will not
    /// fit in the space left, which is the price of not splitting one.
    pub lines: Vec<String>,
    pub fit: Fit,
}

/// `renderTable` (`game-internal-api.ts:402`) — fit, then draw.
///
/// The row state machine [R30] is the subtle half. `previousWasRule` starts
/// `true` because the header rule has just been drawn, so a leading `Rule` or
/// `Band` adds no divider of its own; consecutive rules collapse to one; a band
/// is always followed by a divider; and the closing divider is drawn only when
/// the last row was data.
#[must_use]
pub fn render_table(
    columns: &[Column],
    rows: &[Row<'_>],
    available: usize,
    metric: Metric,
) -> Rendered {
    let fit = fit_with(columns, rows, available, metric);
    let widths = &fit.widths;

    let dash_rule = rule(widths, '-');
    let header_rule = rule(widths, '=');
    // A band spans the frame minus its own `"| "` and `" |"`. The floor exists
    // because a zero-column frame is 1 unit wide.
    let band_width = frame_width(widths).saturating_sub(4).max(1);

    // Each surviving column contributes its text, its fitted width and its
    // alignment; `row_line` does the padding and draws the rails.
    let layout = || fit.active.iter().enumerate().map(|(position, &index)| (position, index));

    let mut lines = vec![
        dash_rule.clone(),
        row_line(
            layout().map(|(position, index)| {
                (columns[index].header, widths[position], columns[index].align)
            }),
            metric,
        ),
        header_rule,
    ];

    let mut previous_was_rule = true;
    for row in rows {
        match row {
            Row::Rule => {
                if !previous_was_rule {
                    lines.push(dash_rule.clone());
                }
                previous_was_rule = true;
            }
            Row::Band(band) => {
                if !previous_was_rule {
                    lines.push(dash_rule.clone());
                }
                lines.push(format!("| {} |", text::pad(band, band_width, Align::Left, metric)));
                lines.push(dash_rule.clone());
                previous_was_rule = true;
            }
            Row::Data(_) => {
                lines.push(row_line(
                    layout().map(|(position, index)| {
                        (row.cell(index), widths[position], columns[index].align)
                    }),
                    metric,
                ));
                previous_was_rule = false;
            }
        }
    }
    if !previous_was_rule {
        lines.push(dash_rule);
    }

    Rendered { lines, fit }
}

fn row_line<'s>(
    cells: impl Iterator<Item = (&'s str, usize, Align)>,
    metric: Metric,
) -> String {
    let mut line = String::from("| ");
    for (position, (text, width, align)) in cells.enumerate() {
        if position > 0 {
            line.push_str(" | ");
        }
        line.push_str(&text::pad(text, width, align, metric));
    }
    line.push_str(" |");
    line
}

fn rule(widths: &[usize], fill: char) -> String {
    let mut out = String::from("+");
    for &width in widths {
        out.extend(core::iter::repeat_n(fill, width + 2));
        out.push('+');
    }
    out
}
