//! The renderer, held to `market-request.ts` lines 341-516.
//!
//! The snapshots in `snapshots/` are an oracle, not a record of what this code
//! happened to print: every one of them has been diffed against the output of
//! `fixtures/render_oracle.ts` — the TypeScript renderer, verbatim — run under
//! the same Bun build that runs the original (1.2.3) over the same scenario
//! file. That script's header says how to re-bless them.

use std::borrow::Cow;

use edm_core::js::text::{self, Align, Metric};
use edm_core::render::{self, Block, Column, Fit, Row, columns};
use proptest::prelude::*;
use serde_json::Value;

/// The widths every scenario is snapshotted at. 48 is the floor the terminal
/// width is clamped to, so it is the narrowest table that can ever be drawn.
const WIDTHS: [usize; 5] = [48, 60, 80, 100, 200];

// ---------------------------------------------------------------------------
// Scenario fixtures
// ---------------------------------------------------------------------------

struct Scenario {
    name: String,
    columns: &'static [Column],
    title: String,
    rows: Vec<Row<'static>>,
}

/// Reads the scenario fixture that the Bun oracle also reads, so that the two
/// sides cannot drift apart in their input.
fn scenarios() -> Vec<Scenario> {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/render_scenarios.json"
    ))
    .expect("scenario fixture");
    let parsed: Value = serde_json::from_str(&raw).expect("scenario fixture is JSON");

    parsed
        .as_array()
        .expect("scenario fixture is an array")
        .iter()
        .map(|scenario| {
            let name = scenario["name"].as_str().expect("name").to_owned();
            let set = scenario["columns"].as_str().expect("columns");
            let columns = columns::by_name(set).unwrap_or_else(|| panic!("unknown column set {set}"));
            let rows = scenario["rows"]
                .as_array()
                .expect("rows")
                .iter()
                .map(|row| match row["kind"].as_str().expect("kind") {
                    "rule" => Row::Rule,
                    "band" => Row::band(row["text"].as_str().expect("text").to_owned()),
                    _ => Row::data(
                        row["cells"]
                            .as_array()
                            .expect("cells")
                            .iter()
                            .map(|cell| cell.as_str().expect("cell").to_owned()),
                    ),
                })
                .collect();
            Scenario { name, columns, title: scenario["title"].as_str().expect("title").to_owned(), rows }
        })
        .collect()
}

fn emit(scenario: &Scenario, width: usize) -> String {
    let block = Block::Table {
        title: scenario.title.clone(),
        columns: scenario.columns,
        rows: scenario.rows.clone(),
    };
    let mut out = String::new();
    render::write_blocks(&mut out, std::slice::from_ref(&block), width, Metric::Utf16);
    out
}

#[test]
fn scenario_snapshots() {
    for scenario in scenarios() {
        for width in WIDTHS {
            insta::assert_snapshot!(format!("{}_w{width}", scenario.name), emit(&scenario, width));
        }
    }
}

fn scenario(name: &str) -> Scenario {
    scenarios().into_iter().find(|s| s.name == name).expect("known scenario")
}

// ---------------------------------------------------------------------------
// R27 — the fitting loop
// ---------------------------------------------------------------------------

#[test]
fn fit_commodity_48() {
    let scenario = scenario("commodity_wide");
    let fit = render::fit(scenario.columns, &scenario.rows, 48);

    // Every column with a priority is dropped, heaviest first and leftmost
    // within a priority: 4 → ID, Fence; 3 → Mean; 2 → Stk, Dmd; 1 → Stock,
    // Demand, CPRI.
    assert_eq!(
        fit.omitted,
        ["ID", "Fence", "Mean", "Stk", "Dmd", "Stock", "Demand", "CPRI"]
    );
    assert_eq!(
        fit.active.iter().map(|&i| scenario.columns[i].header).collect::<Vec<_>>(),
        ["Commodity", "Buy", "Sell"]
    );
    // Buy and Sell hold `12,345,678` and declare no floor, so they cannot give
    // ground; Commodity absorbs the whole overflow down from its 30-unit
    // ceiling. 18 + 10 + 10 is a frame of exactly 48.
    //
    // Three surviving columns at their natural 30/10/10 would be a frame of 52,
    // which is why Commodity has to be squeezed at all: a column costs its
    // width *plus three*, and the frame carries a closing `|` on top.
    assert_eq!(fit.widths, [18, 10, 10]);
    assert_eq!(render::frame_width(&fit.widths), 48);
}

#[test]
fn priority_ties_drop_leftmost() {
    // Two columns at the same maximum priority. The TypeScript folds the
    // droppable list with a strict `>`, which keeps the incumbent, so the
    // leftmost goes first. `Iterator::max_by_key` would drop `second`.
    const SET: &[Column] = &[
        Column::new("keep", "Keep"),
        Column::new("first", "First").priority(2),
        Column::new("second", "Second").priority(2),
        Column::new("light", "Light").priority(1),
    ];
    let rows = [Row::data(["aaaaaaaa", "bbbbbbbb", "cccccccc", "dddddddd"])];

    // Four 8-unit columns are a 45-unit frame; each drop takes 11 off it.
    assert_eq!(render::fit(SET, &rows, 40).omitted, ["First"]);
    assert_eq!(render::fit(SET, &rows, 30).omitted, ["First", "Second"]);
    assert_eq!(render::fit(SET, &rows, 20).omitted, ["First", "Second", "Light"]);
    // Nothing droppable is left, so the frame simply overflows.
    assert_eq!(render::fit(SET, &rows, 5).omitted, ["First", "Second", "Light"]);
}

#[test]
fn a_column_without_a_floor_never_shrinks() {
    // `slack = width - (minWidth ?? width)` is zero without a floor, so the
    // only way this table can fit is by dropping — and `wide` is not
    // droppable, so it keeps its full width and the frame overflows.
    const SET: &[Column] =
        &[Column::new("wide", "Wide"), Column::new("gone", "Gone").priority(1)];
    let rows = [Row::data(["0123456789012345678901234", "x"])];

    let fit = render::fit(SET, &rows, 20);
    assert_eq!(fit.omitted, ["Gone"]);
    assert_eq!(fit.widths, [25]);
    assert!(render::frame_width(&fit.widths) > 20, "the frame is allowed to overflow");
}

#[test]
fn max_width_is_applied_before_the_min_width_floor() {
    // `measureColumns` clips to `maxWidth` and *then* raises to `minWidth`, so
    // a floor above the ceiling wins. Doing it the other way round would give 4.
    const SET: &[Column] = &[Column::new("odd", "Odd").max_width(4).min_width(10)];
    let rows = [Row::data(["0123456789012345"])];

    assert_eq!(render::fit(SET, &rows, 200).widths, [10]);
}

#[test]
fn widths_are_remeasured_from_scratch_after_each_drop() {
    const SET: &[Column] = &[
        Column::new("wide", "Wide").min_width(4),
        Column::new("drop", "Drop").priority(1),
    ];
    let rows = [Row::data(["01234567890123456789", "0123456789"])];
    assert_eq!(render::fit(SET, &rows, 100).widths, [20, 10]);

    // At 20 the pair cannot fit even with `wide` squeezed flat to 4, so `Drop`
    // goes — and the re-measure hands `wide` its natural 20 back. It is then
    // squeezed by the *new* excess of 4 only. Carrying the earlier squeeze
    // over would leave it at its floor of 4.
    let fit = render::fit(SET, &rows, 20);
    assert_eq!(fit.omitted, ["Drop"]);
    assert_eq!(fit.widths, [16]);
}

#[test]
fn the_first_maximum_slack_column_is_squeezed() {
    // Both columns have 16 units of slack. The TypeScript sorts by slack
    // descending — a stable sort — and takes `[0]`, so the tie goes left.
    const SET: &[Column] = &[
        Column::new("left", "Left").min_width(4),
        Column::new("right", "Right").min_width(4),
    ];
    let rows = [Row::data(["01234567890123456789", "01234567890123456789"])];

    // A frame of 47 is 5 too wide, and all 5 come out of the left column.
    assert_eq!(render::fit(SET, &rows, 42).widths, [15, 20]);
}

// ---------------------------------------------------------------------------
// R30 — the band / rule state machine
// ---------------------------------------------------------------------------

#[test]
fn bands_do_not_widen_columns() {
    const SET: &[Column] = &[Column::new("a", "A"), Column::new("b", "B")];
    let with_band = [
        Row::band("a band far wider than any cell in this table".to_owned()),
        Row::data(["x", "y"]),
    ];
    let without = [Row::data(["x", "y"])];

    assert_eq!(render::fit(SET, &with_band, 200).widths, render::fit(SET, &without, 200).widths);
}

#[test]
fn band_width_is_the_frame_less_four() {
    const SET: &[Column] = &[Column::new("a", "A"), Column::new("b", "B")];
    let rows = [Row::band("x".to_owned())];
    let rendered = render::render_table(SET, &rows, 200, Metric::Utf16);
    let frame = render::frame_width(&rendered.fit.widths);

    let band = rendered.lines.iter().find(|line| line.contains('x')).expect("band line");
    // `bandWidth = frameWidth - 4`, and the `"| "` and `" |"` that bracket it
    // put the line back at exactly the frame width.
    assert_eq!(text::utf16_len(band), frame);
    assert_eq!(band, "| x     |");
}

#[test]
fn rules_and_bands_collapse() {
    let rendered = render::render_table(
        columns::PLAN_COLUMNS,
        &scenario("band_state_machine").rows,
        48,
        Metric::Utf16,
    );
    let dashes = rendered.lines.iter().filter(|line| line.starts_with("+--")).count();
    // One opening rule, one after each of the two leading bands, one before
    // "after rules" (from the collapsed run of three), one before and one
    // after the trailing band. The leading pair of rules emits nothing at all
    // because the header rule has just been drawn.
    assert_eq!(dashes, 6);
    assert!(!rendered.lines.last().expect("lines").starts_with("+=="));
}

#[test]
fn no_closing_rule_when_the_last_row_is_not_data() {
    const SET: &[Column] = &[Column::new("a", "A")];
    let ends_with_data = [Row::data(["x"])];
    let ends_with_rule = [Row::data(["x"]), Row::Rule];

    let a = render::render_table(SET, &ends_with_data, 40, Metric::Utf16).lines;
    let b = render::render_table(SET, &ends_with_rule, 40, Metric::Utf16).lines;
    // The explicit trailing rule is drawn *instead of* the closing one, not as
    // well as it.
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------
// R28 — headings
// ---------------------------------------------------------------------------

#[test]
fn heading_pads_with_equals() {
    assert_eq!(render::heading("SWEEP", 20, Metric::Utf16), "== SWEEP ===========");
}

#[test]
fn heading_exactly_as_wide_as_the_terminal_gets_no_padding() {
    // `label.length >= TERMINAL_WIDTH` — the label is 9 units, so at a width of
    // 9 it is returned as-is, trailing space and all, and at 10 it gets one `=`.
    assert_eq!(render::heading("SWEEP", 9, Metric::Utf16), "== SWEEP ");
    assert_eq!(render::heading("SWEEP", 10, Metric::Utf16), "== SWEEP =");
    assert_eq!(render::heading("SWEEP", 1, Metric::Utf16), "== SWEEP ");
}

#[test]
fn an_em_dash_in_a_title_is_one_unit() {
    // U+2014 is a single UTF-16 code unit but three UTF-8 bytes; measuring
    // bytes would lose two `=` here.
    let padded = render::heading("TRADES — done", 30, Metric::Utf16);
    assert_eq!(text::utf16_len(&padded), 30);
    assert!(padded.len() > 30, "the em dash is three UTF-8 bytes");
    // Two from the `== ` prefix and thirteen of padding.
    assert_eq!(padded.matches('=').count(), 2 + 13);
}

// ---------------------------------------------------------------------------
// R29 — emitNote
// ---------------------------------------------------------------------------

#[test]
fn an_empty_note_prints_nothing() {
    assert!(render::wrap_note("", 80, Metric::Utf16).is_empty());
}

#[test]
fn a_leading_space_is_dropped_and_a_double_space_survives() {
    // The first token of `" a"` is empty, and the "line is empty" branch
    // overwrites rather than appends, so the space vanishes. In the middle of
    // a line the same empty token is appended with its separator, so the pair
    // of spaces is preserved.
    assert_eq!(render::wrap_note(" a", 80, Metric::Utf16), ["   a"]);
    assert_eq!(render::wrap_note("a  b", 80, Metric::Utf16), ["   a  b"]);
    assert_eq!(render::wrap_note("a b ", 80, Metric::Utf16), ["   a b "]);
}

#[test]
fn an_over_long_word_overflows_unbroken() {
    let word = "x".repeat(60);
    let lines = render::wrap_note(&format!("hi {word}"), 30, Metric::Utf16);
    assert_eq!(lines, ["   hi".to_owned(), format!("   {word}")]);
}

#[test]
fn the_wrap_limit_excludes_the_indent_and_has_a_floor_of_twenty() {
    let text = "aaaa bbbb cccc dddd eeee";
    // The limit is measured without the indent, so a 20-unit line becomes a
    // 22-unit one once indented — the wrap does not actually respect the width
    // it was given, it undershoots by one and can then overshoot by three.
    assert_eq!(render::wrap_note(text, 23, Metric::Utf16), [
        "   aaaa bbbb cccc dddd",
        "   eeee"
    ]);
    // `Math.max(20, width - 3)`: every width of 23 or less wraps identically.
    assert_eq!(
        render::wrap_note(text, 4, Metric::Utf16),
        render::wrap_note(text, 23, Metric::Utf16)
    );
    // The whole 24-unit text fits on one line only once the limit reaches 24,
    // which takes a width of 27.
    assert_eq!(render::wrap_note(text, 26, Metric::Utf16).len(), 2);
    assert_eq!(render::wrap_note(text, 27, Metric::Utf16), ["   aaaa bbbb cccc dddd eeee"]);
}

// ---------------------------------------------------------------------------
// R31 / C11 — width discovery
// ---------------------------------------------------------------------------

#[test]
fn columns_override_wins_and_is_js_trimmed() {
    assert_eq!(render::terminal_width(Some("  120\u{feff}"), Some(80)), 120);
    // Not digits after trimming, so the override is ignored entirely.
    assert_eq!(render::terminal_width(Some("120x"), Some(80)), 80);
    assert_eq!(render::terminal_width(Some(""), Some(80)), 80);
    // `/^\d+$/` is ASCII: a fullwidth digit is not a digit.
    assert_eq!(render::terminal_width(Some("１２０"), Some(80)), 80);
    // No sign, no decimal point, no exponent.
    assert_eq!(render::terminal_width(Some("+120"), Some(80)), 80);
}

#[test]
fn width_is_floored_at_48_and_defaults_to_100() {
    assert_eq!(render::terminal_width(Some("10"), None), 48);
    assert_eq!(render::terminal_width(None, Some(10)), 48);
    assert_eq!(render::terminal_width(None, None), 100);
    // A terminal reporting zero columns is treated as no terminal at all.
    assert_eq!(render::terminal_width(None, Some(0)), 100);
}

#[test]
fn an_absurd_columns_override_is_clamped() {
    // C11: the TypeScript would attempt `"=".repeat(1e20)` at module init.
    assert_eq!(render::terminal_width(Some("99999999999999999999"), None), render::MAX_WIDTH);
    assert_eq!(render::terminal_width(Some(&"9".repeat(400)), None), render::MAX_WIDTH);
    assert_eq!(render::terminal_width(Some("10000"), None), render::MAX_WIDTH);
    assert_eq!(render::terminal_width(Some("9999"), None), 9999);
}

// ---------------------------------------------------------------------------
// R33 / R36 — progress lines and the hidden-columns note
// ---------------------------------------------------------------------------

#[test]
fn progress_lines_are_clamped_to_the_terminal() {
    let mut out = String::new();
    render::write_blocks(
        &mut out,
        &[Block::Line("0123456789".repeat(3))],
        20,
        Metric::Utf16,
    );
    assert_eq!(out, "0123456789012345678~\n");
}

#[test]
fn the_hidden_columns_note_is_ungrouped_and_comma_joined() {
    // R36: `${TERMINAL_WIDTH}` interpolates the raw number, so a four-digit
    // terminal is `1000 cols`, never `1,000 cols`.
    let cell = "x".repeat(400);
    let rows = vec![Row::data(std::iter::repeat_n(cell, columns::COMMODITY_COLUMNS.len()))];
    let mut out = String::new();
    render::write_blocks(
        &mut out,
        &[Block::Table { title: "T".to_owned(), columns: columns::COMMODITY_COLUMNS, rows }],
        1000,
        Metric::Utf16,
    );
    assert!(out.contains("columns hidden to fit 1000 cols: ID, Fence, Mean, Stk"), "{out}");
}

// ---------------------------------------------------------------------------
// R22 — the metric, and block assembly
// ---------------------------------------------------------------------------

#[test]
fn the_display_metric_measures_a_cjk_name_as_two_cells_per_character() {
    // The parity path counts UTF-16 code units, which is why a station name in
    // Japanese punches through the right-hand frame rail on a real terminal.
    // `EDM_WIDTH=display` selects the other metric; this is the whole
    // difference between them.
    const SET: &[Column] = &[Column::new("name", "Name")];
    let rows = [Row::data(["東京駅"])];

    assert_eq!(render::fit_with(SET, &rows, 200, Metric::Utf16).widths, [4]);
    assert_eq!(render::fit_with(SET, &rows, 200, Metric::Display).widths, [6]);
}

#[test]
fn blocks_emit_in_order() {
    let mut out = String::new();
    render::write_blocks(
        &mut out,
        &[
            Block::Heading("REQUEST URL".to_owned()),
            Block::Line("https://example.invalid/market".to_owned()),
            Block::Note("pass --full-url to print the encrypted query in full".to_owned()),
        ],
        48,
        Metric::Utf16,
    );
    assert_eq!(
        out,
        concat!(
            "== REQUEST URL =================================\n",
            "https://example.invalid/market\n",
            "   pass --full-url to print the encrypted query\n",
            "   in full\n",
        )
    );
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

fn cell() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::string::string_regex("[a-zA-Z0-9 ,.:/~-]{0,24}").expect("regex"),
        prop::string::string_regex("[é—東🚀a ]{0,12}").expect("regex"),
    ]
}

fn rows(width: usize) -> impl Strategy<Value = Vec<Row<'static>>> {
    let data = prop::collection::vec(cell(), width)
        .prop_map(|cells| Row::Data(cells.into_iter().map(Cow::Owned).collect()));
    let row = prop_oneof![
        8 => data,
        1 => cell().prop_map(|text| Row::Band(Cow::Owned(text))),
        1 => Just(Row::Rule),
    ];
    prop::collection::vec(row, 0..6)
}

/// One of the eight real column sets, with a row set shaped to fit it.
///
/// Synthetic column sets would be a weaker test: the `2 * columns` step bound
/// below holds because the real sets declare at most two floors between them,
/// and an arbitrary set of eleven shrinkable columns would need eleven squeezes
/// after every drop.
fn table_case() -> impl Strategy<Value = (&'static [Column], Vec<Row<'static>>)> {
    prop::sample::select(columns::ALL.iter().map(|(_, set)| *set).collect::<Vec<_>>())
        .prop_flat_map(|set| (Just(set), rows(set.len())))
}

/// The order columns are dropped in, independent of the fitting loop: priority
/// descending, and index ascending within a priority.
fn expected_drop_order(set: &[Column]) -> Vec<&'static str> {
    let mut candidates: Vec<&Column> = set.iter().filter(|column| column.priority > 0).collect();
    // A stable sort, so equal priorities stay in column order — which is the
    // whole point of the property.
    candidates.sort_by_key(|column| std::cmp::Reverse(column.priority));
    candidates.into_iter().map(|column| column.header).collect()
}

proptest! {
    #[test]
    fn every_line_measures_exactly_the_frame_width(
        (set, rows) in table_case(),
        available in 1usize..200,
    ) {
        let rendered = render::render_table(set, &rows, available, Metric::Utf16);
        let frame = render::frame_width(&rendered.fit.widths);
        for line in &rendered.lines {
            prop_assert_eq!(text::utf16_len(line), frame, "{}", line);
        }
    }

    #[test]
    fn fitting_terminates_within_two_steps_per_column(
        (set, rows) in table_case(),
        available in 1usize..300,
    ) {
        let fit = render::fit(set, &rows, available);
        prop_assert!(fit.steps <= 2 * set.len(), "{} steps for {} columns", fit.steps, set.len());
    }

    #[test]
    fn omitted_is_a_prefix_of_the_priority_order(
        (set, rows) in table_case(),
        available in 1usize..300,
    ) {
        let fit: Fit = render::fit(set, &rows, available);
        let order = expected_drop_order(set);
        prop_assert!(fit.omitted.len() <= order.len());
        prop_assert_eq!(&fit.omitted[..], &order[..fit.omitted.len()]);
    }

    #[test]
    fn widening_the_terminal_only_shrinks_the_omitted_prefix(
        (set, rows) in table_case(),
        narrow in 1usize..150,
        extra in 0usize..150,
    ) {
        let tight = render::fit(set, &rows, narrow);
        let loose = render::fit(set, &rows, narrow + extra);
        prop_assert!(loose.omitted.len() <= tight.omitted.len());
        prop_assert_eq!(&loose.omitted[..], &tight.omitted[..loose.omitted.len()]);
    }

    #[test]
    fn clamp_measures_the_smaller_of_the_two(text in cell(), width in 0usize..30) {
        let clamped = text::clamp(&text, width.cast_signed(), Metric::Utf16);
        prop_assert_eq!(
            text::utf16_len(&clamped),
            text::utf16_len(&text).min(width)
        );
    }

    #[test]
    fn padding_always_reaches_the_width(text in cell(), width in 0usize..30) {
        for align in [Align::Left, Align::Right] {
            let padded = text::pad(&text, width, align, Metric::Utf16);
            prop_assert_eq!(text::utf16_len(&padded), width);
        }
    }

    #[test]
    fn wrapping_a_note_preserves_every_word(
        text in prop::string::string_regex("[a-z ]{0,80}").expect("regex"),
        width in 4usize..60,
    ) {
        let wrapped = render::wrap_note(&text, width, Metric::Utf16);
        let rejoined = wrapped
            .iter()
            .map(|line| line.strip_prefix("   ").expect("indent"))
            .collect::<Vec<_>>()
            .join(" ");
        let words = |s: &str| {
            s.split(' ').filter(|word| !word.is_empty()).map(str::to_owned).collect::<Vec<_>>()
        };
        prop_assert_eq!(words(&rejoined), words(&text));
    }
}

/// A rate may not be printed without the claim that qualifies it.
///
/// `Route::rate` enforces this in Rust by handing back the number and the
/// guarantee together. The table is where it is easy to lose: the first live
/// run of `edm route` dropped `Claim` to fit an ordinary 100-column terminal
/// and printed twenty rates with nothing said about what was proved of any of
/// them. Both columns declare priority zero, and this asserts it holds at every
/// width down to the clamp floor.
#[test]
fn a_route_rate_is_never_printed_without_its_claim() {
    for available in 20..=200 {
        let fit = render::fit(columns::ROUTE_COLUMNS, &[], available);
        let kept: Vec<&str> =
            fit.active.iter().map(|index| columns::ROUTE_COLUMNS[*index].key).collect();
        assert!(kept.contains(&"rate"), "rate dropped at {available}: {kept:?}");
        assert!(kept.contains(&"claim"), "claim dropped at {available}: {kept:?}");
    }
}
