//! Every table `market-request.ts` prints, as a value.
//!
//! Each function here is one `emit*` from the TypeScript with the `console.log`
//! taken out: it builds the [`Block`]s and returns them. That is what makes the
//! whole visible output of a command reachable from a `#[test]` — no terminal,
//! no clock, no socket — and it is why the ambient bits the originals read from
//! module scope (`TERMINAL_WIDTH`, `API_ORIGIN`) arrive as parameters instead.
//!
//! Two things a reader should not "clean up":
//!
//! - **The zero placeholders are not uniform.** `formatQuantity` renders `0` as
//!   `-` and `formatInteger` renders it as `0`, and the TypeScript picks
//!   between them cell by cell — the market table's `Stock` is a dash, the
//!   trade log's `Qty` is a zero, the same commodity count is a dash in the
//!   sweep's `Sup` and a `0` in its `Comm`. Every choice below is transcribed,
//!   not derived [R7/R8].
//! - **Only one builder measures text.** The sweep's EDDN cell is clamped to 24
//!   units *while the row is built*, before the column is measured [R35], so it
//!   is the only one of these functions that needs a [`Metric`]. The rest hand
//!   raw strings to the renderer and let it do the fitting.

use std::borrow::Cow;

use crate::domain::read::Read;
use crate::domain::starsystem::{MarketPoint, PointOfInterest, Services, poi_type_rank};
use crate::domain::trade::Kind;
use crate::domain::{Commodity, MarketSnapshot, cargo_used};
use crate::js::json::{JsObject, JsValue};
use crate::js::text::{self, Metric};
use crate::js::time::{milliseconds_display, unix_seconds_display};
use crate::js::{self, collate, format_integer, format_quantity, js_number, to_fixed_1};

use super::{Block, Row, columns};

// ---------------------------------------------------------------------------
// Cell formatters
// ---------------------------------------------------------------------------

/// `formatBracketMeter` (ts:533).
///
/// The clamp is `Math.max(0, Math.min(3, Math.trunc(level)))`, which is a total
/// function only because `level` always came through `readNumber` and is
/// therefore finite; a NaN would make the TypeScript's two `repeat` calls both
/// yield `""` and print an empty cell.
#[must_use]
pub fn bracket_meter(level: f64) -> String {
    let filled = js::js_max(0.0, js::js_min(3.0, level.trunc())) as usize;
    let mut out = String::with_capacity(3);
    out.extend(core::iter::repeat_n('#', filled));
    out.extend(core::iter::repeat_n('.', 3 - filled));
    out
}

/// `formatFlag` (ts:537).
fn push_flag(out: &mut String, enabled: bool, symbol: char) {
    out.push(if enabled { symbol } else { '.' });
}

/// `formatCargo` (ts:2038) — bare when no capacity was given, `used/capacity`
/// when one was.
#[must_use]
pub fn cargo_cell(used: f64, capacity: Option<f64>) -> String {
    match capacity {
        None => format_integer(used),
        Some(capacity) => format!("{}/{}", format_integer(used), format_integer(capacity)),
    }
}

/// `fieldRow` (ts:506) for a two-column table.
fn field_row<'a>(field: &'a str, value: impl Into<Cow<'a, str>>) -> Row<'a> {
    Row::Data(vec![Cow::Borrowed(field), value.into()])
}

/// Groups in first-seen order, which is what a `Map` keyed on the group name
/// enumerates in. The bands are re-sorted afterwards, so this order survives
/// only as the tie-break of a stable sort.
fn group_by<'g, T>(items: &'g [T], key: impl Fn(&'g T) -> &'g str) -> Vec<(&'g str, Vec<&'g T>)> {
    let mut groups: Vec<(&str, Vec<&T>)> = Vec::new();
    for item in items {
        let name = key(item);
        if let Some((_, bucket)) = groups.iter_mut().find(|(existing, _)| *existing == name) {
            bucket.push(item);
        } else {
            groups.push((name, vec![item]));
        }
    }
    groups
}

// ---------------------------------------------------------------------------
// Market listing
// ---------------------------------------------------------------------------

/// The legend under every commodity table (ts:677). Double spaces intentional.
const COMMODITY_LEGEND: &str = "legend: '-' zero | CPRI = Consumer/Producer/Rare/Illegal | Stk,Dmd meters '###' bracket 3 .. '...' bracket 0 | '~' truncated";

/// `commodityRow` (ts:637).
fn commodity_row<'a>(commodity: &Commodity<'a>) -> Row<'a> {
    let mut flags = String::with_capacity(4);
    push_flag(&mut flags, commodity.consumer, 'C');
    push_flag(&mut flags, commodity.producer, 'P');
    push_flag(&mut flags, commodity.rare, 'R');
    push_flag(&mut flags, commodity.illegal, 'I');

    Row::Data(vec![
        // `String(id)`, not `formatInteger`: no grouping separators here.
        Cow::Owned(js_number(commodity.id)),
        Cow::Borrowed(commodity.name),
        Cow::Owned(format_quantity(commodity.stock)),
        Cow::Owned(bracket_meter(commodity.stock_bracket)),
        Cow::Owned(format_quantity(commodity.buy_price)),
        Cow::Owned(format_quantity(commodity.sell_price)),
        Cow::Owned(format_quantity(commodity.fence_price)),
        Cow::Owned(format_quantity(commodity.demand)),
        Cow::Owned(bracket_meter(commodity.demand_bracket)),
        Cow::Owned(format_quantity(commodity.mean_price)),
        Cow::Owned(flags),
    ])
}

/// `emitCommodityTable` (ts:658).
///
/// The band label is uppercased through full Unicode, so a category can get
/// *longer* — `straße` becomes `STRASSE` [R32]. Bands do not influence column
/// widths [R30], so that only ever costs a `~` inside the band itself.
#[must_use]
pub fn commodity_table<'a>(commodities: &'a [Commodity<'_>]) -> Vec<Block<'a>> {
    let mut categories = group_by(commodities, |commodity| commodity.category);
    // `[...categories.keys()].sort(localeCompare)`, stable [R26].
    categories.sort_by(|(left, _), (right, _)| collate::locale_cmp(left, right));

    let mut rows: Vec<Row<'a>> = Vec::new();
    for (name, bucket) in &mut categories {
        bucket.sort_by(|left, right| collate::locale_cmp(left.name, right.name));
        let supplied = bucket.iter().filter(|c| c.stock > 0.0).count();
        let wanted = bucket.iter().filter(|c| c.demand > 0.0).count();
        rows.push(Row::band(format!(
            // ts:672
            "{}  {} items | {} supplied | {} in demand",
            name.to_uppercase(),
            bucket.len(),
            supplied,
            wanted
        )));
        rows.extend(bucket.iter().map(|commodity| commodity_row(commodity)));
    }

    vec![
        Block::Table {
            // ts:676
            title: format!(
                "COMMODITIES  {} entries in {} categories",
                commodities.len(),
                categories.len()
            ),
            columns: columns::COMMODITY_COLUMNS,
            rows,
        },
        Block::Note(COMMODITY_LEGEND.to_owned()),
    ]
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

fn item_num(item: Option<&JsObject>, key: &str) -> f64 {
    item.map_or(0.0, |record| record.num(key))
}

/// `emitInventoryTable` (ts:692).
///
/// An empty hold prints a bare heading and no frame at all (ts:694). A row that
/// is not an object degrades rather than disappearing: `asRecord(entry) ?? {}`
/// gives every cell its default, so a JSON `null` renders as `? | - | - | . |
/// - | 0 | 0 | -`.
#[must_use]
pub fn inventory_table<'a>(inventory: &'a [JsValue]) -> Vec<Block<'a>> {
    if inventory.is_empty() {
        return vec![Block::Heading("INVENTORY  empty".to_owned())];
    }

    let rows: Vec<Row<'a>> = inventory
        .iter()
        .map(|entry| {
            let item = entry.as_record();
            let position = item.and_then(|record| record.record("xyz"));
            let coordinates = match position {
                // Coordinates are the only place the tables print a fraction,
                // and `toFixed(1)` rounds ties away from zero [R9].
                Some(xyz) => Cow::Owned(format!(
                    "{} / {} / {}",
                    to_fixed_1(xyz.num("x")),
                    to_fixed_1(xyz.num("y")),
                    to_fixed_1(xyz.num("z"))
                )),
                None => Cow::Borrowed("-"),
            };

            let mut stolen = String::with_capacity(1);
            push_flag(&mut stolen, item.is_some_and(|record| record.flag("stolen")), 'S');

            Row::Data(vec![
                Cow::Borrowed(match item.map_or("", |record| record.string("commodity")) {
                    "" => "?",
                    name => name,
                }),
                Cow::Owned(format_quantity(item_num(item, "qty"))),
                Cow::Owned(format_quantity(item_num(item, "value"))),
                Cow::Owned(stolen),
                Cow::Owned(format_quantity(item_num(item, "marked"))),
                // `String(...)`, so an owner of zero is `0` where `qty` of zero
                // is `-`. Transcribed, not derived.
                Cow::Owned(js_number(item_num(item, "owner"))),
                Cow::Owned(js_number(item_num(item, "origin"))),
                coordinates,
            ])
        })
        .collect();

    vec![Block::Table {
        // ts:721
        title: format!("INVENTORY  {} items", inventory.len()),
        columns: columns::INVENTORY_COLUMNS,
        rows,
    }]
}

// ---------------------------------------------------------------------------
// Market summary
// ---------------------------------------------------------------------------

/// The title `emitMarketSnapshot` defaults to (ts:781).
pub const DEFAULT_SNAPSHOT_TITLE: &str = "MARKET SUMMARY";

/// `emitMarketSummary` (ts:724).
///
/// `credits`, `debt` and `allowsDumping` are probed with `in`, not tested for
/// null [R18] — a payload carrying `"credits": null` prints `0 cr`, which is
/// exactly the value the trade clamps then use. `lastModified` is the odd one
/// out: the TypeScript runs it through `asRecord`, so a present-but-non-object
/// `lastModified` omits the row entirely.
#[must_use]
pub fn market_summary<'a>(snapshot: &'a MarketSnapshot<'_>, title: &str) -> Vec<Block<'a>> {
    let payload = snapshot.payload;
    let commodities = &snapshot.commodities;
    let count = |predicate: fn(&Commodity<'_>) -> bool| commodities.iter().filter(|c| predicate(c)).count();

    let mut distinct: Vec<&str> = Vec::new();
    for commodity in commodities {
        if !distinct.contains(&commodity.category) {
            distinct.push(commodity.category);
        }
    }

    let mut rows: Vec<Row<'a>> = Vec::new();
    if payload.present("credits") {
        rows.push(field_row("credits", format!("{} cr", format_integer(payload.num("credits")))));
    }
    if payload.present("debt") {
        rows.push(field_row("debt", format!("{} cr", format_integer(payload.num("debt")))));
    }
    if let Some(last_modified) = payload.record("lastModified") {
        // The seconds are interpolated ungrouped and the ISO form follows in
        // parentheses [R21].
        rows.push(field_row("lastModified", unix_seconds_display(last_modified.num("sec"))));
    }

    rows.push(field_row(
        "commodities",
        format!("{} in {} categories", commodities.len(), distinct.len()),
    ));
    rows.push(field_row("supplied (stock > 0)", count(|c| c.stock > 0.0).to_string()));
    rows.push(field_row("in demand", count(|c| c.demand > 0.0).to_string()));
    rows.push(field_row(
        "consumers / producers",
        format!("{} / {}", count(|c| c.consumer), count(|c| c.producer)),
    ));
    rows.push(field_row(
        "rare / illegal",
        format!("{} / {}", count(|c| c.rare), count(|c| c.illegal)),
    ));
    if payload.present("allowsDumping") {
        rows.push(field_row(
            "allowsDumping",
            if payload.flag("allowsDumping") { "yes" } else { "no" },
        ));
    }
    rows.push(field_row("inventory items", snapshot.inventory.len().to_string()));

    vec![Block::Table { title: title.to_owned(), columns: columns::FIELD_COLUMNS, rows }]
}

/// `emitMarketSnapshot` (ts:781) — the summary, the hold, then the listing.
#[must_use]
pub fn market_snapshot<'a>(snapshot: &'a MarketSnapshot<'_>, title: &str) -> Vec<Block<'a>> {
    let mut blocks = market_summary(snapshot, title);
    blocks.extend(inventory_table(snapshot.inventory));
    blocks.extend(commodity_table(&snapshot.commodities));
    blocks
}

// ---------------------------------------------------------------------------
// Request and response
// ---------------------------------------------------------------------------

/// One `name: value` header pair, already combined and lowercased the way
/// `Headers` iteration presents them [R71].
pub type Header = (String, String);

/// Everything `emitRequest` reads off a `PreparedRequest` (ts:1175).
///
/// `origin` is a field rather than [`crate::consts::API_ORIGIN`] because
/// `EDM_ORIGIN_OVERRIDE` can replace it [C24], and this crate reads no
/// environment.
#[derive(Clone, Copy, Debug)]
pub struct RequestView<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub origin: &'a str,
    pub url: &'a str,
    pub headers: &'a [Header],
    /// `(name, display ?? String(value))` for each envelope field, in wire
    /// order.
    pub fields: &'a [(&'a str, Cow<'a, str>)],
    /// `encoder.encode(plaintext).length` — genuinely UTF-8 bytes here, unlike
    /// the `--dump` count [R37].
    pub plaintext_bytes: f64,
    pub nonce: &'a str,
    pub frontier_time: f64,
    pub request_time: f64,
}

/// `headerRows` (ts:509) — sorted by `localeCompare`, so `Fdev-Retry` and
/// `fdev-retry` would not sort the way a byte comparison orders them [R26].
fn header_rows(headers: &[Header]) -> Vec<Row<'_>> {
    let mut sorted: Vec<&Header> = headers.iter().collect();
    sorted.sort_by(|(left, _), (right, _)| collate::locale_cmp(left, right));
    sorted.iter().map(|(name, value)| field_row(name, value.as_str())).collect()
}

/// `emitRequest` (ts:1175).
#[must_use]
pub fn request<'a>(view: &RequestView<'a>, full_url: bool) -> Vec<Block<'a>> {
    // `url.slice(url.indexOf("?") + 1)` — with no `?` at all, `indexOf` is -1
    // and the slice is the whole URL. `?` is ASCII, so the byte offset and the
    // UTF-16 offset agree.
    let query = view.url.split_once('?').map_or(view.url, |(_, rest)| rest);

    let mut rows: Vec<Row<'a>> = vec![
        Row::band("TARGET"),
        field_row("method", view.method),
        field_row("endpoint", format!("{}{}", view.origin, view.path)),
        field_row(
            "query",
            format!(
                // ts:1182
                "{} chars base64 {}",
                format_integer(text::utf16_len(query) as f64),
                text::elide(query, 20, 12)
            ),
        ),
        Row::band("HEADERS"),
    ];
    rows.extend(header_rows(view.headers));
    rows.push(Row::band("ENVELOPE"));
    rows.extend(view.fields.iter().map(|(name, display)| field_row(name, display.clone())));
    rows.push(field_row("plaintext", format!("{} bytes", format_integer(view.plaintext_bytes))));
    rows.push(field_row("nonce", view.nonce));
    rows.push(field_row("fTime", unix_seconds_display(view.frontier_time)));
    rows.push(field_row("requestTime", milliseconds_display(view.request_time)));

    let mut blocks = vec![Block::Table {
        // ts:1178
        title: format!("REQUEST  {} {}", view.method, view.path),
        columns: columns::FIELD_COLUMNS,
        rows,
    }];

    if full_url {
        blocks.push(Block::Heading("REQUEST URL".to_owned()));
        // Verbatim: the encrypted query is a kilobyte of base64 and clamping it
        // would defeat the whole point of the flag.
        blocks.push(Block::Raw(view.url.to_owned()));
        return blocks;
    }
    // ts:1197
    blocks.push(Block::Note("pass --full-url to print the encrypted query in full".to_owned()));
    blocks
}

/// `emitResponse` (ts:1200).
///
/// `status` is a JavaScript number interpolated into the title, so it goes
/// through `String(n)` and not `formatInteger` — no grouping separator at
/// three digits either way, but the distinction is the rule everywhere else.
#[must_use]
pub fn response<'a>(status: f64, status_text: &str, headers: &'a [Header]) -> Vec<Block<'a>> {
    let mut rows: Vec<Row<'a>> = vec![Row::band("HEADERS")];
    rows.extend(header_rows(headers));
    vec![Block::Table {
        // ts:1202
        title: format!("RESPONSE  HTTP {} {status_text}", js_number(status)),
        columns: columns::FIELD_COLUMNS,
        rows,
    }]
}

/// `emitOpaquePayload` (ts:1293) — a decoded body that is not a market
/// listing.
///
/// Pretty-printed when it parses and echoed verbatim when it does not; a body
/// `serde_json` rejects but JavaScript accepts takes the second path, which is
/// how [C15] degrades identically.
#[must_use]
pub fn opaque_payload(decrypted: &str) -> Vec<Block<'static>> {
    let rendered = match JsValue::parse(decrypted) {
        Ok(value) => value.stringify(2),
        Err(_) => decrypted.to_owned(),
    };
    vec![Block::Heading("PAYLOAD".to_owned()), Block::Raw(rendered)]
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

/// What the EDDN gateway said about one market.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EddnOutcome<'a> {
    pub ok: bool,
    /// Already `clampText(body, 120)` by the time it gets here [R79].
    pub detail: &'a str,
}

/// One row of `SWEEP RESULTS` — `MarketVisit` (ts:1347) as the table reads it.
///
/// `failure` is deliberately absent: the TypeScript populates it and never
/// renders it [R89].
#[derive(Clone, Copy, Debug)]
pub struct Visit<'a> {
    pub market_id: f64,
    pub name: &'a str,
    /// `null` when the request never produced a status at all.
    pub status: Option<f64>,
    pub snapshot: Option<&'a MarketSnapshot<'a>>,
    pub eddn: Option<EddnOutcome<'a>>,
    /// `attempts ?? 1`.
    pub attempts: Option<f64>,
}

/// `emitSweepSummary` (ts:1369).
///
/// The `EDDN` cell is clamped to 24 units *before* the column is measured
/// [R35], which is why this is the one builder that needs a [`Metric`]: a
/// failure detail can arrive already carrying a `~` and then be clamped again
/// by a column narrower than 24.
///
/// Note the asymmetry inside one row: `Comm` is `formatInteger`, so a market
/// with no commodities shows `0`, while `Sup` and `Dem` are `formatQuantity`
/// and show `-`.
#[must_use]
pub fn sweep_summary<'a>(visits: &'a [Visit<'a>], title: &str, metric: Metric) -> Vec<Block<'a>> {
    let rows: Vec<Row<'a>> = visits
        .iter()
        .map(|visit| {
            let commodities = visit.snapshot.map(|snapshot| &snapshot.commodities);
            let counted = |predicate: fn(&Commodity<'_>) -> bool| match commodities {
                None => Cow::Borrowed("-"),
                Some(list) => Cow::Owned(format_quantity(
                    list.iter().filter(|c| predicate(c)).count() as f64,
                )),
            };

            Row::Data(vec![
                Cow::Owned(js_number(visit.market_id)),
                Cow::Borrowed(visit.name),
                visit.status.map_or(Cow::Borrowed("-"), |status| Cow::Owned(js_number(status))),
                commodities.map_or(Cow::Borrowed("-"), |list| {
                    Cow::Owned(format_integer(list.len() as f64))
                }),
                counted(|c| c.stock > 0.0),
                counted(|c| c.demand > 0.0),
                match visit.eddn {
                    None => Cow::Borrowed("-"),
                    Some(eddn) if eddn.ok => Cow::Borrowed("sent"),
                    Some(eddn) => text::clamp(eddn.detail, 24, metric),
                },
                Cow::Owned(js_number(visit.attempts.unwrap_or(1.0))),
            ])
        })
        .collect();

    vec![Block::Table { title: title.to_owned(), columns: columns::SWEEP_COLUMNS, rows }]
}

// ---------------------------------------------------------------------------
// Trade
// ---------------------------------------------------------------------------

/// `PlanField` (ts:1725) — one resolved value and where it came from.
#[derive(Clone, Debug)]
pub struct PlanField<'a> {
    pub label: &'a str,
    pub value: Cow<'a, str>,
    /// `"flag" | "market" | "default"` (ts:1723).
    pub source: &'a str,
}

/// `emitTradePlan` (ts:1919).
///
/// `qty` reaches the title through `formatInteger`, which truncates — a
/// fractional quantity is displayed whole and sent fractional [R95].
#[must_use]
pub fn trade_plan<'a>(
    kind: Kind,
    qty: f64,
    commodity_name: &str,
    fields: &'a [PlanField<'a>],
    notes: &'a [String],
) -> Vec<Block<'a>> {
    let rows: Vec<Row<'a>> = fields
        .iter()
        .map(|field| {
            Row::Data(vec![
                Cow::Borrowed(field.label),
                field.value.clone(),
                Cow::Borrowed(field.source),
            ])
        })
        .collect();

    let mut blocks = vec![Block::Table {
        // ts:1921
        title: format!(
            "TRADE PLAN  {} {} x {commodity_name}",
            kind.as_str(),
            format_integer(qty)
        ),
        columns: columns::PLAN_COLUMNS,
        rows,
    }];
    blocks.extend(notes.iter().map(|note| Block::Note(note.clone())));
    blocks
}

/// `TradeRecord` (ts:2013) as the log table reads it.
#[derive(Clone, Copy, Debug)]
pub struct TradeRecord<'a> {
    pub round: f64,
    pub commodity: &'a str,
    pub qty: f64,
    pub unit_price: f64,
    pub status: Option<f64>,
    pub cargo_used: Option<f64>,
}

/// The `TRADES` log (ts:2290), its credit note and the closing hold listing.
///
/// The totals are folded left to right in row order, as the TypeScript's
/// `reduce` is, because `f64` addition is not associative and the printed total
/// depends on the order.
///
/// Every quantity here is `formatInteger`, so a zero-unit request shows `0` and
/// not the `-` the market table would print for the same number.
#[must_use]
pub fn trade_log<'a>(
    records: &'a [TradeRecord<'a>],
    rounds: f64,
    outcome: &str,
    capacity: Option<f64>,
    inventory: &'a [JsValue],
    credits: Option<f64>,
) -> Vec<Block<'a>> {
    let total_units = records.iter().fold(0.0, |sum, record| sum + record.qty);
    let total_value = records.iter().fold(0.0, |sum, record| sum + record.qty * record.unit_price);

    let mut rows: Vec<Row<'a>> = records
        .iter()
        .map(|record| {
            Row::Data(vec![
                Cow::Owned(js_number(record.round)),
                Cow::Borrowed(record.commodity),
                Cow::Owned(format_integer(record.qty)),
                Cow::Owned(format_integer(record.unit_price)),
                Cow::Owned(format_integer(record.qty * record.unit_price)),
                record.status.map_or(Cow::Borrowed("-"), |status| Cow::Owned(js_number(status))),
                record
                    .cargo_used
                    .map_or(Cow::Borrowed("-"), |used| Cow::Owned(cargo_cell(used, capacity))),
            ])
        })
        .collect();
    rows.push(Row::Rule);
    rows.push(Row::Data(vec![
        Cow::Borrowed(""),
        Cow::Borrowed("TOTAL"),
        Cow::Owned(format_integer(total_units)),
        Cow::Borrowed(""),
        Cow::Owned(format_integer(total_value)),
        Cow::Borrowed(""),
        Cow::Owned(cargo_cell(cargo_used(inventory), capacity)),
    ]));

    let mut blocks = vec![Block::Table {
        // ts:2290 — the dash is U+2014, one UTF-16 unit and two display columns.
        title: format!(
            "TRADES  {} requests over {} round{} — {outcome}",
            records.len(),
            js_number(rounds),
            if rounds == 1.0 { "" } else { "s" }
        ),
        columns: columns::TRADE_LOG_COLUMNS,
        rows,
    }];
    if let Some(credits) = credits {
        // ts:2317
        blocks.push(Block::Note(format!("credits now {}", format_integer(credits))));
    }
    blocks.extend(inventory_table(inventory));
    blocks
}

// ---------------------------------------------------------------------------
// Star system
// ---------------------------------------------------------------------------

/// The legend under every market-point table (ts:2812).
const MARKET_POINT_LEGEND: &str = "CBOYF = Commodities/Blackmarket/Outfitting/shipYard/reFuel service available | Imp,Exp = commodities traded";

/// `emitMarketPoints` (ts:2770).
///
/// Two orderings, both transcribed. The bands go in [`POI_TYPE_ORDER`], with
/// anything unrecognised sharing the last rank and breaking its tie by
/// `localeCompare`. Within a band the sort is by distance with `null` treated
/// as `Infinity`, then by name — and the TypeScript's first test is
/// `left.distance !== right.distance`, so two markets that are both distanceless
/// compare equal and fall through to the name.
///
/// [`POI_TYPE_ORDER`]: crate::domain::starsystem::POI_TYPE_ORDER
#[must_use]
pub fn market_points<'a>(points: &'a [MarketPoint<'_>], title: &str) -> Vec<Block<'a>> {
    let mut groups = group_by(points, |point| point.kind.as_ref());
    groups.sort_by(|(left, _), (right, _)| {
        poi_type_rank(left)
            .cmp(&poi_type_rank(right))
            .then_with(|| collate::locale_cmp(left, right))
    });

    let mut rows: Vec<Row<'a>> = Vec::new();
    for (kind, bucket) in &mut groups {
        bucket.sort_by(|left, right| {
            if left.distance == right.distance {
                return collate::locale_cmp(&left.name, &right.name);
            }
            let near = left.distance.unwrap_or(f64::INFINITY);
            let far = right.distance.unwrap_or(f64::INFINITY);
            // Distances are `Some` only when strictly positive, so neither side
            // can be NaN and the comparison is total.
            near.partial_cmp(&far).unwrap_or(core::cmp::Ordering::Equal)
        });
        let trading = bucket.iter().filter(|point| point.trades()).count();
        rows.push(Row::band(format!(
            // ts:2792
            "{}  {} | {} with a commodity market",
            kind.to_uppercase(),
            bucket.len(),
            trading
        )));

        for point in bucket.iter() {
            let mut services = String::with_capacity(Services::COLUMNS.len());
            for (service, symbol) in Services::COLUMNS {
                push_flag(&mut services, point.services.has(service), symbol);
            }
            rows.push(Row::Data(vec![
                Cow::Owned(js_number(point.market_id)),
                Cow::Borrowed(point.name.as_ref()),
                Cow::Owned(services),
                Cow::Owned(format_quantity(point.imports as f64)),
                Cow::Owned(format_quantity(point.exports as f64)),
                point.distance.map_or(Cow::Borrowed("-"), |distance| {
                    // `Math.round` is half toward +infinity, not away from zero
                    // [R12].
                    Cow::Owned(format_integer(js::js_round(distance)))
                }),
                Cow::Borrowed(point.economy.unwrap_or("-")),
                Cow::Borrowed(point.faction.unwrap_or("-")),
                Cow::Borrowed(point.body_name.unwrap_or("-")),
            ]));
        }
    }

    vec![
        Block::Table { title: title.to_owned(), columns: columns::MARKET_POINT_COLUMNS, rows },
        Block::Note(MARKET_POINT_LEGEND.to_owned()),
    ]
}

/// The structural-scan fallback listing (ts:3082).
///
/// Reached only when `starsystem.polities` yielded nothing, so the caller emits
/// its own explanatory note first; this builds the table alone.
#[must_use]
pub fn points_of_interest<'a>(points: &'a [PointOfInterest<'_>]) -> Vec<Block<'a>> {
    let rows: Vec<Row<'a>> = points
        .iter()
        .map(|point| {
            Row::Data(vec![
                point.market_id.map_or(Cow::Borrowed("-"), |id| Cow::Owned(js_number(id))),
                Cow::Borrowed(point.name),
                Cow::Borrowed(point.kind.unwrap_or("-")),
                Cow::Borrowed(point.economy.unwrap_or("-")),
                Cow::Borrowed(point.faction.unwrap_or("-")),
                Cow::Borrowed(point.path.as_str()),
            ])
        })
        .collect();

    vec![Block::Table {
        // ts:3083
        title: format!("POINTS OF INTEREST  {} found by scan", points.len()),
        columns: columns::POI_COLUMNS,
        rows,
    }]
}

// ---------------------------------------------------------------------------
// route
// ---------------------------------------------------------------------------

/// Everything the plan table states, gathered so the caller cannot supply the
/// numbers in the wrong order.
#[derive(Clone, Copy, Debug)]
pub struct PlanView<'a> {
    pub reference: &'a str,
    /// What was asked for.
    pub radius_ly: f64,
    /// What the enumeration actually established. Equal to `radius_ly` when the
    /// frontier closed; smaller when Ardent's row cap stopped it short.
    pub complete_to_ly: f64,
    pub ardent_requests: u32,
    pub estimate: &'a crate::spend::Estimate,
    pub rate_per_second: f64,
    pub max_requests: f64,
    pub prior: crate::spend::SizePrior,
}

/// The plan table: what a sweep will cost, before any of it is spent.
///
/// The exclusion lines are the important part and are not decoration. Near Sol
/// the defaults remove 63% of what Ardent calls a station, and a user who sees
/// only the surviving count has no way to tell a deliberate filter from a tool
/// that quietly missed most of the region. Each line names the flag that would
/// keep them.
#[must_use]
pub fn route_plan(view: &PlanView<'_>) -> Vec<Block<'static>> {
    use crate::js::format_integer as int;
    use crate::spend;

    let &PlanView {
        reference,
        radius_ly,
        complete_to_ly,
        ardent_requests,
        estimate,
        rate_per_second,
        max_requests,
        prior,
    } = view;

    let coverage = if complete_to_ly >= radius_ly {
        format!("{} Ly (complete, {} Ardent {})", js_number(radius_ly), int(f64::from(ardent_requests)),
                if ardent_requests == 1 { "query" } else { "queries" })
    } else {
        // Say what was actually established rather than what was asked for.
        format!(
            "{} Ly asked, complete to {} Ly ({} Ardent queries)",
            js_number(radius_ly),
            js_number(complete_to_ly),
            int(f64::from(ardent_requests))
        )
    };

    let mut rows: Vec<Row<'static>> = vec![
        field_row("reference", reference.to_owned()),
        field_row("radius", coverage),
        field_row(
            "systems",
            if estimate.systems_to_read == 0 {
                // Not "0 worth reading": by default no system is read at all,
                // and a zero here would read as "the filter emptied them".
                format!("{} in radius", int(estimate.systems as f64))
            } else {
                format!(
                    "{} in radius, {} to verify",
                    int(estimate.systems as f64),
                    int(estimate.systems_to_read as f64)
                )
            },
        ),
        field_row("stations known", format!("{} (Ardent)", int(estimate.stations_known as f64))),
    ];

    for exclusion in &estimate.exclusions {
        // Built directly rather than through `field_row`, whose label borrows.
        rows.push(Row::Data(vec![
            Cow::Owned(format!("  - {}", exclusion.label)),
            Cow::Owned(format!("-{}   ({} to keep)", int(exclusion.removed as f64), exclusion.keep_with)),
        ]));
    }

    rows.push(field_row("markets to poll", int(estimate.markets_to_poll as f64)));
    if estimate.cached_fresh > 0 {
        rows.push(field_row("cached and still fresh", int(estimate.cached_fresh as f64)));
    }
    // The split is only worth showing when there are two kinds. By default a
    // sweep reads no starsystem payloads at all, and "= 0 starsystem + 135
    // market" would invite the reader to look for a decision that was not
    // taken here.
    rows.push(field_row(
        "CAPI requests",
        if estimate.systems_to_read == 0 {
            format!("{}  (one per market)", int(estimate.requests))
        } else {
            format!(
                "{}  = {} starsystem + {} market",
                int(estimate.requests),
                int(estimate.systems_to_read as f64),
                int(estimate.markets_to_poll as f64)
            )
        },
    ));
    rows.push(field_row(
        "estimated transfer",
        format!(
            "{}  (prior: {} KB/system, {} KB/market)",
            spend::transfer_range(estimate),
            int(prior.system_bytes / 1024.0),
            int(prior.market_bytes / 1024.0)
        ),
    ));
    rows.push(field_row("pacing", format!("{} req/s", js_number(rate_per_second))));
    rows.push(field_row("estimated wall clock", spend::duration_estimate(estimate.seconds)));
    rows.push(field_row(
        "ceiling",
        format!("{} of {}   (--max-requests to raise)", int(estimate.requests), int(max_requests)),
    ));

    vec![Block::Table {
        title: "ROUTE PLAN".to_owned(),
        columns: columns::ROUTE_FIELD_COLUMNS,
        rows,
    }]
}

/// What the sweep actually reached, stated so that a market missing because it
/// failed is never mistaken for one missing because it was unprofitable.
#[must_use]
pub fn route_coverage(coverage: &RouteCoverage) -> Vec<Block<'static>> {
    use crate::js::format_integer as int;

    let mut rows = Vec::new();
    // Only when systems were read at all: `--verify-systems` is off by
    // default, and "0 of 0" is a row about a decision, not a measurement.
    if coverage.systems_total > 0 {
        rows.push(field_row(
            "systems read",
            format!(
                "{} of {}",
                int(coverage.systems_read as f64),
                int(coverage.systems_total as f64)
            ),
        ));
    }
    rows.push(field_row(
        "markets polled",
        format!(
            "{} of {}",
            int(coverage.markets_polled as f64),
            int(coverage.markets_found as f64)
        ),
    ));
    rows.push(field_row("markets priced", int(coverage.markets_priced as f64)));
    if coverage.systems_failed > 0 {
        rows.push(field_row("systems failed", int(coverage.systems_failed as f64)));
    }
    if coverage.markets_absent > 0 {
        rows.push(field_row(
            "no market at station",
            format!("{} (HTTP 410)", int(coverage.markets_absent as f64)),
        ));
    }
    if coverage.markets_failed > 0 {
        rows.push(field_row("markets failed", int(coverage.markets_failed as f64)));
    }
    if coverage.cache_hits > 0 {
        rows.push(field_row("from cache", int(coverage.cache_hits as f64)));
    }
    rows.push(field_row("requests sent", int(coverage.requests_sent as f64)));
    if coverage.throttled > 0 {
        rows.push(field_row("throttled", format!("{} (429 or 503)", int(coverage.throttled as f64))));
    }
    rows.push(field_row("elapsed", crate::spend::duration_estimate(coverage.elapsed_seconds)));

    let mut blocks =
        vec![Block::Table {
            title: "ROUTE COVERAGE".to_owned(),
            columns: columns::ROUTE_FIELD_COLUMNS,
            rows,
        }];
    for note in coverage.notes() {
        blocks.push(Block::Note(note));
    }
    blocks
}

/// `1 market` / `2 markets`, with the verb to match.
///
/// Worth the four lines: these notes exist to make a partial answer legible,
/// and "1 markets were not reached" is the sentence a reader stops trusting.
fn plural(count: usize, noun: &str, one: &'static str, many: &'static str) -> (String, &'static str) {
    let word = if count == 1 { noun.to_owned() } else { format!("{noun}s") };
    (format!("{} {word}", crate::js::format_integer(count as f64)), if count == 1 { one } else { many })
}

/// What a sweep reached, and what it did not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RouteCoverage {
    pub systems_total: usize,
    pub systems_read: usize,
    pub systems_failed: usize,
    pub markets_found: usize,
    pub markets_polled: usize,
    pub markets_priced: usize,
    pub markets_failed: usize,
    /// Reached, and answered that they have no commodity market — HTTP 410.
    ///
    /// Counted apart from both the priced and the failed, because it is neither.
    /// A live radius-100 sweep found 40 of 5,089, and reporting them as failures
    /// told the user forty markets had been missed when every one had answered.
    pub markets_absent: usize,
    pub cache_hits: usize,
    pub requests_sent: usize,
    pub throttled: usize,
    pub elapsed_seconds: f64,
    /// Set when the enumeration could not close the frontier.
    pub truncated_to_ly: Option<f64>,
    pub breaker_tripped: bool,
}

impl RouteCoverage {
    /// The sentences that must appear when they are true.
    ///
    /// Each one exists because its absence would let a partial answer read as a
    /// complete one.
    #[must_use]
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();

        // What was actually read, not what a sweep usually does. A run served
        // entirely from the cache printing "read live during this run" is the
        // exact class of untruth the rest of this table exists to prevent —
        // and it is what the second live run of this command produced.
        let fresh = self.markets_priced.saturating_sub(self.cache_hits);
        if self.markets_priced > 0 {
            notes.push(if self.cache_hits == 0 {
                "every price below was read live from the Companion API during this run".to_owned()
            } else if fresh == 0 {
                "every price below came from the cache, not from this run; --refresh re-reads them"
                    .to_owned()
            } else {
                format!(
                    "{} of {} prices came from the cache rather than this run; --refresh re-reads them",
                    crate::js::format_integer(self.cache_hits as f64),
                    crate::js::format_integer(self.markets_priced as f64),
                )
            });
        }

        if self.markets_absent > 0 {
            let (count, verb) = plural(self.markets_absent, "station", "has", "have");
            let they = if self.markets_absent == 1 { "it" } else { "they" };
            notes.push(format!(
                "{count} {verb} no commodity market right now — {they} answered, {they} are \
                 simply not trading, and {they} are not missing from the ranking",
            ));
        }
        if self.markets_failed > 0 {
            let (count, verb) = plural(self.markets_failed, "market", "was", "were");
            notes.push(format!(
                "{count} in radius {verb} not reached and {} absent from the ranking, not ranked low",
                if self.markets_failed == 1 { "is" } else { "are" },
            ));
        }
        if self.systems_failed > 0 {
            let (count, verb) = plural(self.systems_failed, "system", "failed", "failed");
            notes.push(format!(
                "{count} {verb}, so any market in {} is unknown to this run",
                if self.systems_failed == 1 { "it" } else { "them" },
            ));
        }
        if let Some(limit) = self.truncated_to_ly {
            notes.push(format!(
                "enumeration is complete only to {} Ly; beyond that this run does not know what exists",
                js_number(limit)
            ));
        }
        if self.breaker_tripped {
            notes.push(
                "the run stopped early after too many failures, so the region was not fully swept"
                    .to_owned(),
            );
        }
        notes
    }
}

/// One market, as the sweep finishes it.
///
/// Shaped after the ported sweep's `[k/N] Name (id)  HTTP s  outcome`
/// (`market-request.ts:1540`, R83) rather than invented, because a commander
/// who has watched `edm market Colonia` should not have to learn a second
/// format — but it is not that function and is not held to it: the id is
/// dropped in favour of the system, which is what locates a station in a
/// region sweep, and the outcome says how many rows are worth trading rather
/// than how many rows there are.
///
/// **Every Companion API market returns the same 391-entry commodity map**,
/// most of it priced but idle, so a raw commodity count is the same number for
/// every market in the galaxy and tells the reader nothing. Measured
/// 2026-08-05.
#[derive(Clone, Copy, Debug)]
pub struct SweepLine<'a> {
    pub completed: usize,
    pub total: usize,
    pub station: &'a str,
    pub system: &'a str,
    pub status: Option<u16>,
    /// Rows this market actually sells or buys, after the quantity floors.
    pub tradable: Option<usize>,
    pub from_cache: bool,
    pub attempts: u32,
}

#[must_use]
pub fn sweep_line(line: &SweepLine<'_>) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "[{}/{}] {} ({})",
        js::format_integer(line.completed as f64),
        js::format_integer(line.total as f64),
        line.station,
        line.system,
    );
    if line.from_cache {
        // No status, because no request was made. Saying `HTTP 200` for a file
        // read would put a number on the wire that never went over it.
        out.push_str("  cached");
    } else {
        let _ = write!(
            out,
            "  HTTP {}",
            line.status.map_or_else(|| "-".to_owned(), |code| js::format_integer(f64::from(code)))
        );
    }
    match line.tradable {
        Some(rows) => {
            let _ = write!(out, "  {} tradable", js::format_integer(rows as f64));
        }
        None => out.push_str("  no listing"),
    }
    if line.attempts > 1 {
        let _ = write!(out, "  after {} attempts", js::format_integer(f64::from(line.attempts)));
    }
    out
}

/// A pacing decision, under `--verbose`.
///
/// These exist because a sweep that is merely slow and a sweep that is being
/// throttled look identical from the outside: both are just quiet. Each line
/// names what happened and what it changed, so a reader can tell one from the
/// other without a packet capture.
#[derive(Clone, Copy, Debug)]
pub enum PaceEvent<'a> {
    /// The server asked us to slow down.
    Throttled { status: u16, retry_after: Option<&'a str>, new_rate: f64 },
    /// A failed job is going back into the queue.
    Retrying { station: &'a str, attempt: u32, delay_ms: f64, status: Option<u16> },
    /// A job has been given up on.
    GaveUp { station: &'a str, attempts: u32, reason: &'a str },
    /// The rate recovered after a run of clean responses.
    Recovered { new_rate: f64 },
    /// The run is stopping early.
    BreakerTripped { reason: &'a str },
}

#[must_use]
pub fn pace_line(event: &PaceEvent<'_>) -> String {
    match *event {
        PaceEvent::Throttled { status, retry_after, new_rate } => {
            let held = retry_after.map_or_else(
                || "no Retry-After".to_owned(),
                |value| format!("Retry-After: {value}"),
            );
            format!(
                "  pace  HTTP {} ({held}); rate now {} req/s for every worker",
                js::format_integer(f64::from(status)),
                js_number(new_rate),
            )
        }
        PaceEvent::Retrying { station, attempt, delay_ms, status } => format!(
            "  pace  {station} HTTP {} — attempt {} in {} ms",
            status.map_or_else(|| "-".to_owned(), |code| js::format_integer(f64::from(code))),
            js::format_integer(f64::from(attempt) + 1.0),
            js::format_integer(js::js_round(delay_ms)),
        ),
        PaceEvent::GaveUp { station, attempts, reason } => format!(
            "  pace  {station} given up after {} attempts ({reason})",
            js::format_integer(f64::from(attempts)),
        ),
        PaceEvent::Recovered { new_rate } => {
            format!("  pace  rate recovered to {} req/s", js_number(new_rate))
        }
        PaceEvent::BreakerTripped { reason } => {
            format!("  pace  stopping early: {reason}")
        }
    }
}
