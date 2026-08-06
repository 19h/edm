//! Every table the program prints, held to `market-request.ts`.
//!
//! The snapshots are taken at three widths because the interesting failures are
//! width-dependent: 48 is the floor the terminal width is clamped to and the
//! only place columns are dropped, 100 is what a non-terminal stdout gets, and
//! 200 is wide enough that nothing is squeezed and the raw cell text is visible.
//!
//! The unit tests beside them exist because a snapshot proves a table *changed*
//! but not *why*. The orderings and the two zero placeholders are stated
//! directly so a reviewer can see the rule rather than infer it from a frame.

use std::borrow::Cow;

use edm_core::domain::starsystem::{collect_points_of_interest, read_market_points};
use edm_core::domain::trade::Kind;
use edm_core::domain::{self, Commodity};
use edm_core::js::json::JsValue;
use edm_core::js::text::Metric;
use edm_core::render::views::{
    self, EddnOutcome, Header, PlanField, RequestView, TradeRecord, Visit,
};
use edm_core::render::{Block, Row, write_blocks};

/// 48 is the clamp floor, 100 the non-terminal default, 200 a width nothing is
/// squeezed at.
const WIDTHS: [usize; 3] = [48, 100, 200];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn emit(blocks: &[Block<'_>], width: usize) -> String {
    let mut out = String::new();
    write_blocks(&mut out, blocks, width, Metric::Utf16);
    out
}



fn snapshot_at_widths(name: &str, blocks: &[Block<'_>]) {
    for width in WIDTHS {
        insta::assert_snapshot!(format!("{name}_w{width}"), emit(blocks, width));
    }
}

/// Band labels in the order the table would draw them.
fn bands(blocks: &[Block<'_>]) -> Vec<String> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Table { rows, .. } => Some(rows),
            _ => None,
        })
        .flatten()
        .filter_map(|row| match row {
            Row::Band(text) => Some(text.to_string()),
            _ => None,
        })
        .collect()
}

/// Data rows in draw order, as raw cell text — before any fitting.
fn cells(blocks: &[Block<'_>]) -> Vec<Vec<String>> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Table { rows, .. } => Some(rows),
            _ => None,
        })
        .flatten()
        .filter_map(|row| match row {
            Row::Data(cells) => Some(cells.iter().map(ToString::to_string).collect()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A trade-endpoint listing: five categories including `NonMarketable` and an
/// illegal good, an inventory row with `xyz` and one without, and a third entry
/// that is not an object at all.
const MARKET: &str = r#"{
  "credits": 12345678,
  "debt": 0,
  "lastModified": { "sec": 1700000000 },
  "allowsDumping": true,
  "commodities": {
    "128049152": { "id": 128049152, "name": "Explosives", "categoryname": "Chemicals",
      "stock": 41231, "stockBracket": 3, "buyPrice": 312, "sellPrice": 296,
      "meanPrice": 305, "demand": 0, "demandBracket": 0, "producer": 1, "legality": "" },
    "128049202": { "id": 128049202, "name": "Gold", "categoryname": " Metals ",
      "stock": 0, "stockBracket": 0, "buyPrice": 0, "sellPrice": 9301, "fencePrice": 9280,
      "meanPrice": 9401, "demand": 812, "demandBracket": 2, "consumer": 1, "legality": "" },
    "128049204": { "id": 128049204, "name": "Silver", "categoryname": "Metals",
      "stock": 2, "stockBracket": 1, "buyPrice": 4820, "sellPrice": 4791,
      "meanPrice": 4775, "demand": 0, "demandBracket": 0, "producer": 1, "legality": "" },
    "128049669": { "id": 128049669, "name": "Basic Narcotics", "categoryname": "Narcotics",
      "stock": 0, "stockBracket": 0, "buyPrice": 0, "sellPrice": 1301, "fencePrice": 1120,
      "meanPrice": 1108, "demand": 5240, "demandBracket": 3, "consumer": 1, "rare": 1,
      "legality": "Illegal" },
    "128049672": { "id": 128049672, "name": "Hydrogen Fuel", "categoryname": "Chemicals",
      "stock": 964532, "stockBracket": 3, "buyPrice": 110, "sellPrice": 104,
      "meanPrice": 108, "demand": 0, "demandBracket": 0, "producer": 1, "legality": "" },
    "128673855": { "id": 128673855, "name": "Limpet", "categoryname": "NonMarketable",
      "stock": 0, "stockBracket": 0, "buyPrice": 101, "sellPrice": 0,
      "meanPrice": 100, "demand": 0, "demandBracket": 0, "legality": "" },
    "128924331": { "id": 128924331, "name": "Unclassified Relic", "categoryname": "   ",
      "stock": 1, "stockBracket": 0, "buyPrice": 0, "sellPrice": 82640,
      "meanPrice": 80000, "demand": 1, "demandBracket": 1, "rare": 1, "legality": "" }
  },
  "inventory": [
    { "commodity": "Gold", "qty": 117, "value": 1088217, "stolen": false, "marked": 0,
      "owner": 0, "origin": 3223343616,
      "xyz": { "x": -9530.6875, "y": -910.25, "z": 19808.125 } },
    { "commodity": "Basic Narcotics", "qty": 4, "value": 4404, "stolen": true,
      "marked": 1, "owner": 128000000, "origin": 0 },
    null
  ]
}"#;

/// A system with a starport, an outpost that trades nothing, two fleet carriers
/// at unknown distances and a `poiType` the normaliser does not know.
const STARSYSTEM: &str = r#"{
  "starsystem": {
    "starsystem": { "minorFactions": {
      "72060832334024995": { "name": "Colonia Council" }
    } },
    "polities": {
      "0": {
        "controllingMinorFaction": 72060832334024995,
        "markets": {
          "3223343616": { "id": 3223343616, "name": "Jaques Station",
            "poiType": "starport", "distFromSystem": 712.5, "bodyName": "Colonia 2 a",
            "economies": { "0": { "name": "Tourism", "proportion": 0.7 },
                           "1": { "name": "Refinery", "proportion": 0.3 } },
            "imported": { "gold": 1, "silver": 1, "explosives": 1 },
            "exported": { "hydrogenfuel": 1 },
            "services": { "commodities": "ok", "blackmarket": "ok", "outfitting": "ok",
                          "shipyard": "ok", "refuel": "ok" } },
          "3223343617": { "id": 3223343617, "name": "Brady Terminal",
            "poiType": "outpost", "distFromSystem": 240.75, "bodyName": "Colonia 4",
            "economies": { "0": { "name": "Industrial", "proportion": 1 } },
            "services": { "refuel": "ok", "outfitting": "closed" } },
          "3705689344": { "id": 3705689344, "name": "K7Q-B0X",
            "poiType": "fleetcarrier",
            "imported": { "tritium": 1 },
            "services": { "commodities": "ok", "refuel": "ok" } },
          "3705689345": { "id": 3705689345, "name": "A1B-C2D",
            "poiType": "FleetCarrier",
            "services": { "commodities": "ok" } },
          "3806349056": { "id": 3806349056, "name": "Colonia Hub",
            "poiType": "surfaceStation", "distFromSystem": 12,
            "economies": { "0": { "name": "Extraction", "proportion": 1 } },
            "exported": { "gold": 1 },
            "services": { "commodities": "ok", "blackmarket": "ok" } }
        }
      }
    }
  }
}"#;

/// A payload that carries no `starsystem.polities`, so only the structural scan
/// finds anything.
const DRIFTED: &str = r#"{
  "docks": [
    { "marketId": 3223343616, "stationName": "Jaques Station", "stationType": "Coriolis",
      "economy": "Tourism", "controllingFaction": "Colonia Council" },
    { "name": "Brady Terminal", "type": "outpost", "services": { "refuel": "ok" } },
    { "name": "Not A Port" }
  ]
}"#;

fn headers() -> Vec<Header> {
    // Combined and lowercased the way `Headers` iteration presents them [R71].
    [
        ("encrypted", "1"),
        ("fdev-retry", "0/2"),
        ("fdev-season", "4"),
        ("fdev-semver", "4.4.0.3"),
        ("nonce", "a1b2c3d4e5f6"),
        ("request-time", "1700000000000"),
        ("user-agent", "EDGame/11.0/Win64"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value.to_owned()))
    .collect()
}

fn commodity(name: &'static str, category: &'static str) -> Commodity<'static> {
    Commodity {
        id: 1.0,
        name,
        category,
        stock: 0.0,
        stock_bracket: 0.0,
        buy_price: 0.0,
        sell_price: 0.0,
        fence_price: 0.0,
        demand: 0.0,
        demand_bracket: 0.0,
        mean_price: 0.0,
        consumer: false,
        producer: false,
        rare: false,
        illegal: false,
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

#[test]
fn market_snapshot_tables() {
    let document = JsValue::parse(MARKET).expect("fixture parses");
    let snapshot = domain::parse_market_snapshot(&document).expect("fixture is a market");
    snapshot_at_widths(
        "market_snapshot",
        &views::market_snapshot(&snapshot, views::DEFAULT_SNAPSHOT_TITLE),
    );
}

#[test]
fn an_empty_hold_prints_only_a_heading() {
    let blocks = views::inventory_table(&[]);
    assert_eq!(blocks, vec![Block::Heading("INVENTORY  empty".to_owned())]);
    snapshot_at_widths("inventory_empty", &blocks);
}

#[test]
fn sweep_results() {
    let document = JsValue::parse(MARKET).expect("fixture parses");
    let snapshot = domain::parse_market_snapshot(&document).expect("fixture is a market");
    let empty_document = JsValue::parse(
        r#"{"commodities":{"1":{"id":1,"name":"Tritium","categoryname":"Minerals"}}}"#,
    )
    .expect("fixture parses");
    let empty = domain::parse_market_snapshot(&empty_document).expect("fixture is a market");

    let visits = [
        Visit {
            market_id: 3_223_343_616.0,
            name: "Jaques Station",
            status: Some(200.0),
            snapshot: Some(&snapshot),
            eddn: Some(EddnOutcome { ok: true, detail: "OK" }),
            attempts: Some(1.0),
        },
        Visit {
            market_id: 3_223_343_617.0,
            name: "Brady Terminal",
            status: Some(200.0),
            snapshot: Some(&empty),
            eddn: Some(EddnOutcome {
                ok: false,
                detail: "HTTP 400 Bad Request: Failed to validate to schema",
            }),
            attempts: Some(2.0),
        },
        Visit {
            market_id: 3_705_689_344.0,
            name: "K7Q-B0X",
            status: Some(500.0),
            snapshot: None,
            eddn: None,
            attempts: Some(4.0),
        },
        Visit {
            market_id: 3_806_349_056.0,
            name: "Colonia Hub",
            status: None,
            snapshot: None,
            eddn: None,
            attempts: None,
        },
    ];

    snapshot_at_widths(
        "sweep",
        &views::sweep_summary(&visits, "SWEEP RESULTS  4 markets", Metric::Utf16),
    );
}

#[test]
fn request_and_response() {
    let fields: Vec<(&str, Cow<'_, str>)> = vec![
        ("marketID", Cow::Borrowed("3223343616")),
        ("machineToken", Cow::Borrowed("<80 chars>")),
        ("fTime", Cow::Borrowed("1700000000")),
    ];
    let header_list = headers();
    let url = format!(
        "https://api.orerve.net/2.0/elite/market/list?{}",
        "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVphYmNkZWZnaGlqa2xtbm9wcXJzdHV2d3h5ejAxMjM0NTY3ODk=".repeat(3)
    );
    let view = RequestView {
        method: "GET",
        path: "/2.0/elite/market/list",
        origin: "https://api.orerve.net",
        url: &url,
        headers: &header_list,
        fields: &fields,
        plaintext_bytes: 212.0,
        nonce: "a1b2c3d4e5f6",
        frontier_time: 1_700_000_000.0,
        request_time: 1_700_000_000_000.0,
    };

    for width in WIDTHS {
        insta::assert_snapshot!(
            format!("request_w{width}"),
            emit(&views::request(&view, false), width)
        );
        insta::assert_snapshot!(
            format!("request_full_url_w{width}"),
            emit(&views::request(&view, true), width)
        );
    }
    snapshot_at_widths("response", &views::response(200.0, "OK", &header_list));
}

#[test]
fn opaque_payload_is_pretty_printed_when_it_parses() {
    let parsed = views::opaque_payload(r#"{"errors":[{"code":401,"message":"unauthorized"}]}"#);
    let raw = views::opaque_payload("not json at all {");
    for width in WIDTHS {
        insta::assert_snapshot!(format!("opaque_json_w{width}"), emit(&parsed, width));
        insta::assert_snapshot!(format!("opaque_raw_w{width}"), emit(&raw, width));
    }
}

#[test]
fn trade_plan_and_log() {
    let fields = [
        PlanField { label: "marketId", value: Cow::Borrowed("3223343616"), source: "flag" },
        PlanField { label: "commodity", value: Cow::Borrowed("Gold (128049202)"), source: "market" },
        PlanField { label: "unitPrice", value: Cow::Borrowed("9,301 cr"), source: "market" },
        PlanField { label: "qty", value: Cow::Borrowed("13"), source: "flag" },
        PlanField { label: "total", value: Cow::Borrowed("120,913 cr"), source: "default" },
    ];
    let notes = [
        "Gold: stock - | demand 812 | buy - | sell 9,301 | fence 9,280 | held 117".to_owned(),
    ];
    snapshot_at_widths("trade_plan", &views::trade_plan(Kind::Sell, 13.0, "Gold", &fields, &notes));

    let document = JsValue::parse(MARKET).expect("fixture parses");
    let snapshot = domain::parse_market_snapshot(&document).expect("fixture is a market");
    let records = [
        TradeRecord {
            round: 1.0,
            commodity: "Gold",
            qty: 13.0,
            unit_price: 9301.0,
            status: Some(200.0),
            cargo_used: Some(121.0),
        },
        TradeRecord {
            round: 1.0,
            commodity: "Basic Narcotics",
            qty: 0.0,
            unit_price: 1120.0,
            status: None,
            cargo_used: None,
        },
        TradeRecord {
            round: 2.0,
            commodity: "Hydrogen Fuel",
            qty: 8.0,
            unit_price: 110.0,
            status: Some(402.0),
            cargo_used: Some(121.0),
        },
    ];
    snapshot_at_widths(
        "trade_log",
        &views::trade_log(
            &records,
            2.0,
            "hold is full",
            Some(256.0),
            snapshot.inventory,
            Some(12_345_678.0),
        ),
    );
}

#[test]
fn system_markets_and_the_structural_fallback() {
    let document = JsValue::parse(STARSYSTEM).expect("fixture parses");
    let payload = document.as_record().expect("fixture is an object");
    let points = read_market_points(payload);
    snapshot_at_widths("market_points", &views::market_points(&points, "MARKETS  Colonia"));

    let drifted = JsValue::parse(DRIFTED).expect("fixture parses");
    let found = collect_points_of_interest(&drifted);
    snapshot_at_widths("points_of_interest", &views::points_of_interest(&found));
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// Bands are sorted by `localeCompare` and rows within a band by name, both
/// stably [R26]. `categoryname` is trimmed and an all-blank one falls back to
/// `Uncategorised`, which is why the relic sorts between Narcotics and the rest.
#[test]
fn commodity_bands_and_rows_are_locale_sorted() {
    let document = JsValue::parse(MARKET).expect("fixture parses");
    let snapshot = domain::parse_market_snapshot(&document).expect("fixture is a market");
    let blocks = views::commodity_table(&snapshot.commodities);

    let labels: Vec<String> =
        bands(&blocks).iter().map(|band| band.split("  ").next().unwrap().to_owned()).collect();
    assert_eq!(
        labels,
        ["CHEMICALS", "METALS", "NARCOTICS", "NONMARKETABLE", "UNCATEGORISED"],
        "band order is localeCompare over the category names"
    );

    let names: Vec<String> = cells(&blocks).iter().map(|row| row[1].clone()).collect();
    assert_eq!(
        names,
        [
            "Explosives",
            "Hydrogen Fuel",
            "Gold",
            "Silver",
            "Basic Narcotics",
            "Limpet",
            "Unclassified Relic",
        ],
        "within a band, rows sort by commodity name"
    );
}

/// `toUpperCase` is full Unicode, so a band can come out *longer* than the
/// category it names [R32]. Bands do not influence column widths [R30], so the
/// only consequence is a `~` inside the band itself.
#[test]
fn r32_a_band_can_lengthen_when_uppercased() {
    let commodities = [commodity("Kompressor", "straße")];
    let blocks = views::commodity_table(&commodities);
    assert_eq!(bands(&blocks), ["STRASSE  1 items | 0 supplied | 0 in demand"]);
}

/// Type bands follow `POI_TYPE_ORDER`; anything unrecognised shares the last
/// rank and breaks its tie by `localeCompare`.
#[test]
fn market_point_bands_follow_the_type_order() {
    let document = JsValue::parse(STARSYSTEM).expect("fixture parses");
    let payload = document.as_record().expect("fixture is an object");
    let points = read_market_points(payload);
    let blocks = views::market_points(&points, "MARKETS");

    let labels: Vec<String> =
        bands(&blocks).iter().map(|band| band.split("  ").next().unwrap().to_owned()).collect();
    assert_eq!(
        labels,
        ["STARPORT", "OUTPOST", "CARRIER", "SURFACESTATION"],
        "known types in POI_TYPE_ORDER, then the unknown one"
    );
}

/// Within a band the sort is by distance with `null` as `Infinity`, then by
/// name — and the TypeScript's first test is `left.distance !== right.distance`,
/// so two distanceless markets compare equal and fall through to the name.
#[test]
fn distanceless_markets_sort_last_then_by_name() {
    let document = JsValue::parse(STARSYSTEM).expect("fixture parses");
    let payload = document.as_record().expect("fixture is an object");
    let points = read_market_points(payload);
    let blocks = views::market_points(&points, "MARKETS");

    let carriers: Vec<String> = cells(&blocks)
        .iter()
        .filter(|row| row[0].starts_with("37056893"))
        .map(|row| row[1].clone())
        .collect();
    assert_eq!(carriers, ["A1B-C2D", "K7Q-B0X"], "both are distanceless, so name decides");
}

// ---------------------------------------------------------------------------
// The two zero placeholders
// ---------------------------------------------------------------------------

/// `formatQuantity` renders `0` as `-` and `formatInteger` renders it as `0`
/// [R7/R8], and which cell uses which is transcribed from the TypeScript. This
/// pins the three places the two meet inside one row.
#[test]
fn zero_renders_as_a_dash_or_a_zero_depending_on_the_cell() {
    let document = JsValue::parse(MARKET).expect("fixture parses");
    let snapshot = domain::parse_market_snapshot(&document).expect("fixture is a market");

    // Inventory: `qty`, `value` and `marked` are quantities, but `owner` and
    // `origin` go through `String(n)` (ts:713).
    let hold = cells(&views::inventory_table(snapshot.inventory));
    assert_eq!(hold[0][4], "-", "marked 0 is a dash");
    assert_eq!(hold[0][5], "0", "owner 0 is a zero");
    assert_eq!(hold[2], ["?", "-", "-", ".", "-", "0", "0", "-"], "a null entry degrades whole");

    // The market listing: a stocked-out commodity shows a dash, its id does not.
    let listing = cells(&views::commodity_table(&snapshot.commodities));
    let gold = listing.iter().find(|row| row[1] == "Gold").expect("Gold is listed");
    assert_eq!(gold[0], "128049202", "the id is String(n), never grouped");
    assert_eq!(gold[2], "-", "stock 0 is a dash");
    assert_eq!(gold[4], "-", "buyPrice 0 is a dash");

    // The sweep: `Comm` is formatInteger and `Sup`/`Dem` are formatQuantity, so
    // one row can carry both spellings of the same count.
    let empty_document =
        JsValue::parse(r#"{"commodities":{"1":{"id":1,"name":"Tritium"}}}"#).expect("parses");
    let empty = domain::parse_market_snapshot(&empty_document).expect("is a market");
    let visits = [Visit {
        market_id: 1.0,
        name: "Nowhere",
        status: Some(200.0),
        snapshot: Some(&empty),
        eddn: None,
        attempts: None,
    }];
    let row = cells(&views::sweep_summary(&visits, "SWEEP", Metric::Utf16)).remove(0);
    assert_eq!(row[3], "1", "Comm counts through formatInteger");
    assert_eq!(row[4], "-", "Sup counts through formatQuantity");
    assert_eq!(row[5], "-", "Dem counts through formatQuantity");

    // The trade log is formatInteger throughout, so a skipped request shows 0.
    let records = [TradeRecord {
        round: 1.0,
        commodity: "Gold",
        qty: 0.0,
        unit_price: 0.0,
        status: None,
        cargo_used: None,
    }];
    let log = cells(&views::trade_log(&records, 1.0, "done", None, &[], None));
    assert_eq!(log[0][2], "0", "a zero-unit request is a zero, not a dash");
    assert_eq!(log[0][4], "0");
}

/// `formatBracketMeter` clamps to 0..3 before it draws, so a payload carrying a
/// bracket of 4 — which the Companion API does emit — renders `###` rather than
/// widening the column.
#[test]
fn the_bracket_meter_clamps() {
    assert_eq!(views::bracket_meter(0.0), "...");
    assert_eq!(views::bracket_meter(2.7), "##.");
    assert_eq!(views::bracket_meter(4.0), "###");
    assert_eq!(views::bracket_meter(-3.0), "...");
}

/// `formatCargo` (ts:2038) drops the denominator when no `--cargo` was given.
#[test]
fn the_cargo_cell_omits_a_capacity_it_does_not_have() {
    assert_eq!(views::cargo_cell(1234.0, None), "1,234");
    assert_eq!(views::cargo_cell(1234.0, Some(256.0)), "1,234/256");
}

// ---------------------------------------------------------------------------
// R35
// ---------------------------------------------------------------------------

/// The sweep's EDDN cell is `clampText(detail, 24)` *before* the column is
/// measured [R35]. The visible consequence: the cell already carries a `~` at
/// 24 units, and a narrower column then clamps the clamped text again — so at
/// width 48 the frame shows a truncation of a truncation.
#[test]
fn r35_the_eddn_cell_is_clamped_before_it_is_measured() {
    let detail = "HTTP 400 Bad Request: Failed to validate to schema";
    let visits = [Visit {
        market_id: 1.0,
        name: "Nowhere",
        status: Some(200.0),
        snapshot: None,
        eddn: Some(EddnOutcome { ok: false, detail }),
        attempts: None,
    }];
    let blocks = views::sweep_summary(&visits, "SWEEP", Metric::Utf16);

    let cell = cells(&blocks).remove(0).remove(6);
    assert_eq!(cell.chars().count(), 24, "clamped to 24 while the row is built");
    assert_eq!(cell, "HTTP 400 Bad Request: F~");

    // And the column is measured from the already-clamped text, so the header
    // `EDDN` cannot be widened past 24 by an arbitrarily long failure message.
    let rendered = emit(&blocks, 200);
    assert!(rendered.contains("HTTP 400 Bad Request: F~"), "{rendered}");
    assert!(!rendered.contains("validate to schema"), "{rendered}");
}

/// `lastModified` is the one summary row the TypeScript gates on `asRecord`
/// rather than `in`, so a present-but-non-object value omits the row while a
/// present-but-null `credits` still prints `0 cr` [R18].
#[test]
fn r18_the_summary_probes_for_keys_not_for_values() {
    let document = JsValue::parse(
        r#"{"credits":null,"debt":null,"lastModified":1700000000,"allowsDumping":null,
            "commodities":{"1":{"id":1,"name":"Tritium"}}}"#,
    )
    .expect("fixture parses");
    let snapshot = domain::parse_market_snapshot(&document).expect("fixture is a market");
    let rows = cells(&views::market_summary(&snapshot, "MARKET SUMMARY"));
    let labels: Vec<&str> = rows.iter().map(|row| row[0].as_str()).collect();

    assert!(labels.contains(&"credits"), "a null credits still prints");
    assert!(labels.contains(&"debt"));
    assert!(labels.contains(&"allowsDumping"));
    assert!(!labels.contains(&"lastModified"), "a non-object lastModified is dropped");
    assert_eq!(rows[0], ["credits", "0 cr"]);
    assert_eq!(
        rows.iter().find(|row| row[0] == "allowsDumping").expect("row").as_slice(),
        ["allowsDumping", "no"],
        "readBoolean is `=== true || === 1`, so null is no"
    );
}

// ---------------------------------------------------------------------------
// route
// ---------------------------------------------------------------------------

fn sample_estimate() -> edm_core::spend::Estimate {
    use edm_core::spend::{Counts, Estimate, Exclusion, SizePrior};
    Estimate::build(
        Counts {
            systems: 412,
            systems_to_read: 118,
            stations_known: 1_230,
            markets_to_poll: 157,
            cached_fresh: 22,
        },
        vec![
            Exclusion { label: "Odyssey settlements", removed: 771, keep_with: "--settlements" },
            Exclusion { label: "fleet carriers", removed: 194, keep_with: "--include-carriers" },
            Exclusion { label: "outposts (pad L)", removed: 86, keep_with: "--pad M" },
            Exclusion { label: "beyond 2,000 Ls", removed: 22, keep_with: "--max-star-distance" },
        ],
        4.0,
        &SizePrior::default(),
    )
}

fn plan_view(estimate: &edm_core::spend::Estimate) -> views::PlanView<'_> {
    views::PlanView {
        reference: "Sol",
        radius_ly: 20.0,
        complete_to_ly: 20.0,
        ardent_requests: 1,
        estimate,
        rate_per_second: 4.0,
        max_requests: edm_core::spend::DEFAULT_MAX_REQUESTS,
        prior: edm_core::spend::SizePrior::default(),
    }
}

fn route_plan_blocks(estimate: &edm_core::spend::Estimate) -> Vec<Block<'static>> {
    views::route_plan(&plan_view(estimate))
}

#[test]
fn route_plan_snapshots() {
    let estimate = sample_estimate();
    snapshot_at_widths("route_plan", &route_plan_blocks(&estimate));
}

/// The plan must show what each filter removed, not just what survived.
///
/// Near Sol the defaults drop 63% of everything Ardent calls a station. A user
/// shown only "157 markets" cannot tell a deliberate filter from a tool that
/// quietly missed most of the region, so every exclusion appears on its own
/// line naming the flag that would keep it.
#[test]
fn the_plan_names_every_exclusion_and_how_to_undo_it() {
    let estimate = sample_estimate();
    // Raw cells rather than rendered text: at 48 columns the frame elides the
    // very words under test, and the claim here is about content, not fitting.
    let text = cells(&route_plan_blocks(&estimate)).concat().join("\n");
    for (label, flag) in [
        ("Odyssey settlements", "--settlements"),
        ("fleet carriers", "--include-carriers"),
        ("outposts (pad L)", "--pad M"),
    ] {
        assert!(text.contains(label), "missing exclusion {label}\n{text}");
        assert!(text.contains(flag), "missing the flag that keeps {label}\n{text}");
    }
    assert!(text.contains("-771"), "the settlement count itself\n{text}");
}

/// The request count is split by kind and adds up, because the two kinds have
/// very different sizes and a single total hides which one dominates.
#[test]
fn the_plan_shows_the_request_split() {
    let estimate = sample_estimate();
    let text = cells(&route_plan_blocks(&estimate)).concat().join("\n");
    // 135 markets, not the 157 found: the 22 still-fresh cached ones cost
    // nothing, and a plan that priced them would overstate what it will spend.
    assert!(text.contains("253  = 118 official batch + 135 market"), "{text}");
}

/// An enumeration that could not close its frontier says so in the radius row
/// itself, where the number it qualifies is.
#[test]
fn an_incomplete_enumeration_is_stated_next_to_the_radius() {
    let estimate = sample_estimate();
    let blocks = views::route_plan(&views::PlanView {
        reference: "Shinrarta Dezhra",
        radius_ly: 60.0,
        complete_to_ly: 41.0,
        ardent_requests: 7,
        ..plan_view(&estimate)
    });
    let text = cells(&blocks).concat().join("\n");
    assert!(text.contains("60 Ly asked, complete to 41 Ly"), "{text}");
    assert!(!text.contains("complete,"), "must not also claim completeness\n{text}");
}

fn sample_coverage() -> views::RouteCoverage {
    views::RouteCoverage {
        ranked: true,
        eddn_refusal: None,
        systems_total: 118,
        systems_read: 116,
        systems_failed: 2,
        markets_found: 157,
        markets_polled: 151,
        markets_priced: 149,
        markets_failed: 6,
        markets_absent: 3,
        eddn: Some(views::EddnCoverage {
            sent: 140,
            failed: 1,
            recent: 6,
            cached: 22,
            unnamed: 0,
            abandoned: 0,
        }),
        cache_hits: 22,
        requests_sent: 267,
        throttled: 3,
        elapsed_seconds: 71.0,
        oldest_observed_ms: None,
        newest_observed_ms: None,
        observation_time_unknown: 0,
        measured_at_ms: 0.0,
        truncated_to_ly: None,
        breaker_tripped: false,
    }
}

#[test]
fn coverage_reports_underlying_observation_span_and_unknown_times() {
    let mut coverage = sample_coverage();
    coverage.oldest_observed_ms = Some(1_700_000_000_000.0);
    coverage.newest_observed_ms = Some(1_700_000_120_000.0);
    coverage.measured_at_ms = 1_700_000_600_000.0;
    coverage.observation_time_unknown = 2;
    let notes = coverage.notes().join("
");
    assert!(notes.contains("observations span"), "{notes}");
    assert!(notes.contains("10 minutes old"), "{notes}");
    assert!(notes.contains("2 priced listings have no underlying market timestamp"), "{notes}");
    assert!(!notes.contains("read at one instant"), "{notes}");
}

#[test]
fn route_coverage_snapshots() {
    snapshot_at_widths("route_coverage", &views::route_coverage(&sample_coverage()));
}

/// The coverage note that matters most: a market that could not be read is
/// absent from the ranking, and absence must never read as "unprofitable".
#[test]
fn unreached_markets_are_named_as_unreached() {
    let notes = sample_coverage().notes();
    let joined = notes.join("\n");
    assert!(joined.contains("6 markets in radius were not reached"), "{joined}");
    assert!(joined.contains("not ranked low"), "{joined}");
    assert!(joined.contains("2 systems failed"), "{joined}");
}

/// "1 markets were not reached" is the sentence a reader stops trusting.
#[test]
fn the_coverage_notes_agree_with_themselves_in_number() {
    let one = views::RouteCoverage {
        markets_failed: 1,
        systems_failed: 1,
        ..views::RouteCoverage::default()
    };
    let joined = one.notes().join("\n");
    assert!(joined.contains("1 market in radius was not reached and is absent"), "{joined}");
    assert!(joined.contains("1 system failed, so any market in it is unknown"), "{joined}");
}

/// A clean sweep says only the one thing that is true of it.
#[test]
fn a_complete_sweep_carries_no_warnings() {
    let coverage = views::RouteCoverage {
        systems_total: 4,
        systems_read: 4,
        markets_found: 9,
        markets_polled: 9,
        markets_priced: 9,
        requests_sent: 13,
        elapsed_seconds: 4.0,
        ..views::RouteCoverage::default()
    };
    let notes = coverage.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("read live from the Companion API"), "{notes:?}");
}

/// A sweep that found nothing says nothing, rather than claiming live prices it
/// never read.
#[test]
fn an_empty_region_makes_no_claim_about_prices() {
    assert!(views::RouteCoverage::default().notes().is_empty());
}

/// A sweep that reads no starsystem payloads — the default — must not print a
/// row about how many of them it read. "0 of 0" is a row about a decision, not
/// a measurement, and it invites the reader to hunt for a failure.
#[test]
fn coverage_omits_the_system_row_when_no_system_was_read() {
    let coverage = views::RouteCoverage {
        markets_found: 2,
        markets_polled: 2,
        markets_priced: 2,
        requests_sent: 2,
        ..views::RouteCoverage::default()
    };
    let text = emit(&views::route_coverage(&coverage), 200);
    assert!(!text.contains("systems read"), "{text}");
    assert!(text.contains("markets polled | 2 of 2"), "{text}");
}

/// A forty-kilobyte sweep must not be priced as "0-0 MB". The small end of the
/// range is exactly where a user decides whether to run at all.
#[test]
fn a_small_sweep_is_priced_in_kilobytes() {
    use edm_core::spend::{Counts, Estimate, SizePrior};
    let estimate = Estimate::build(
        Counts {
            systems: 3,
            systems_to_read: 0,
            stations_known: 4,
            markets_to_poll: 2,
            cached_fresh: 0,
        },
        Vec::new(),
        4.0,
        &SizePrior::default(),
    );
    assert_eq!(edm_core::spend::transfer_range(&estimate), "28-52 KB");
}

/// And the default plan says nothing about starsystem reads it will not make.
#[test]
fn the_default_plan_does_not_mention_starsystem_reads() {
    use edm_core::spend::{Counts, Estimate, SizePrior};
    let estimate = Estimate::build(
        Counts {
            systems: 412,
            systems_to_read: 0,
            stations_known: 1_230,
            markets_to_poll: 157,
            cached_fresh: 0,
        },
        Vec::new(),
        4.0,
        &SizePrior::default(),
    );
    let text = cells(&route_plan_blocks(&estimate)).concat().join("\n");
    assert!(text.contains("157  (one per market)"), "{text}");
    assert!(text.contains("412 in radius"), "{text}");
    assert!(!text.contains("starsystem"), "{text}");
    assert!(!text.contains("worth reading"), "{text}");
}

// ---------------------------------------------------------------------------
// progress
// ---------------------------------------------------------------------------

fn line(overrides: impl Fn(&mut views::SweepLine<'_>)) -> String {
    let mut line = views::SweepLine {
        completed: 3,
        total: 22,
        station: "Galileo",
        system: "Sol",
        status: Some(200),
        tradable: Some(110),
        from_cache: false,
        attempts: 1,
    };
    overrides(&mut line);
    views::sweep_line(&line)
}

/// The count that means something. Every Companion API market returns the same
/// 391-entry commodity map, most of it priced but idle, so a commodity count is
/// identical for every market in the galaxy; what varies is how many rows have
/// stock or demand behind them.
#[test]
fn a_progress_line_counts_tradable_rows_not_commodities() {
    assert_eq!(line(|_| {}), "[3/22] Galileo (Sol)  HTTP 200  110 tradable");
}

/// A cached market makes no request, so it has no status. Printing `HTTP 200`
/// for a file read would put a number on the wire that never went over it.
#[test]
fn a_cached_market_reports_no_status() {
    let text = line(|l| {
        l.from_cache = true;
        l.status = None;
        l.attempts = 0;
    });
    assert_eq!(text, "[3/22] Galileo (Sol)  cached  110 tradable");
    assert!(!text.contains("HTTP"));
}

/// A market that answered without a usable listing is named as such, not given
/// a zero — zero rows and no listing are different facts.
#[test]
fn a_market_with_no_listing_says_so_rather_than_zero() {
    let text = line(|l| {
        l.tradable = None;
        l.status = Some(500);
    });
    assert_eq!(text, "[3/22] Galileo (Sol)  HTTP 500  no listing");
}

#[test]
fn a_retried_market_says_how_many_attempts_it_took() {
    assert!(line(|l| l.attempts = 3).ends_with("after 3 attempts"));
    assert!(!line(|l| l.attempts = 1).contains("attempts"), "not for the first");
}

/// The pacing lines exist because a slow sweep and a throttled one look the
/// same from outside. Each must name what happened *and* what it changed.
#[test]
fn every_pacing_line_names_what_changed() {
    let throttled = views::pace_line(&views::PaceEvent::Throttled {
        status: 429,
        retry_after: Some("30"),
        new_rate: 2.0,
    });
    assert_eq!(
        throttled,
        "  pace  HTTP 429 (Retry-After: 30); rate now 2 req/s for every worker"
    );
    // "for every worker" is the load-bearing half: a per-job backoff would
    // leave the other fifteen hammering a server that just said stop.
    assert!(throttled.contains("every worker"));

    assert_eq!(
        views::pace_line(&views::PaceEvent::Throttled {
            status: 429,
            retry_after: None,
            new_rate: 2.0,
        }),
        "  pace  HTTP 429 (no Retry-After); rate now 2 req/s for every worker"
    );

    assert_eq!(
        views::pace_line(&views::PaceEvent::Retrying {
            station: "Titan City",
            attempt: 1,
            delay_ms: 1_000.0,
            status: Some(503),
        }),
        "  pace  Titan City HTTP 503 — attempt 2 in 1,000 ms"
    );

    assert_eq!(
        views::pace_line(&views::PaceEvent::GaveUp {
            station: "Sisyphus Dock",
            attempts: 8,
            reason: "AttemptCap",
        }),
        "  pace  Sisyphus Dock given up after 8 attempts (AttemptCap)"
    );

    assert_eq!(
        views::pace_line(&views::PaceEvent::Recovered { new_rate: 4.0 }),
        "  pace  rate recovered to 4 req/s"
    );
    assert_eq!(
        views::pace_line(&views::PaceEvent::BreakerTripped { reason: "FailureRate" }),
        "  pace  stopping early: FailureRate"
    );
}

/// A transport failure has no status at all, and must not be rendered as one.
#[test]
fn a_retry_with_no_status_prints_a_dash() {
    let text = views::pace_line(&views::PaceEvent::Retrying {
        station: "Nowhere",
        attempt: 2,
        delay_ms: 500.0,
        status: None,
    });
    assert!(text.contains("HTTP -"), "{text}");
}

/// HTTP 410 — "commodities not currently available at this market" — is a
/// correct, permanent answer to a well-formed request. The station is real, it
/// was reached, and it has nothing to trade.
///
/// A live radius-100 sweep found 40 of 5,089 answering this, and counting them
/// as failures told the user forty markets had been "not reached and absent
/// from the ranking" when every one of them had answered. It is neither a
/// success to rank nor a failure to chase, so it is counted as neither.
#[test]
fn a_station_with_no_market_is_not_a_failure() {
    let coverage = views::RouteCoverage {
        markets_found: 10,
        markets_polled: 9,
        markets_priced: 9,
        markets_absent: 1,
        requests_sent: 10,
        ..views::RouteCoverage::default()
    };

    let text = emit(&views::route_coverage(&coverage), 200);
    assert!(text.contains("no market at station"), "{text}");
    assert!(text.contains("(HTTP 410)"), "{text}");
    assert!(!text.contains("markets failed"), "not a failure\n{text}");

    let notes = coverage.notes().join("\n");
    assert!(notes.contains("1 station has no commodity market"), "{notes}");
    assert!(notes.contains("it answered, it are") || notes.contains("not trading"), "{notes}");
    assert!(!notes.contains("not reached"), "they were reached\n{notes}");
}

/// And plurals agree, because these notes exist to be believed.
#[test]
fn the_absent_note_agrees_in_number() {
    let many = views::RouteCoverage {
        markets_priced: 5_049,
        markets_absent: 40,
        ..views::RouteCoverage::default()
    };
    let notes = many.notes().join("\n");
    assert!(notes.contains("40 stations have no commodity market"), "{notes}");
}

/// The coverage notes are written for a reader about to look at a ranking —
/// "every price *below*", "not missing from the ranking". `edm eddn` has no
/// table below and no ranking to be missing from, so the same sentences would
/// describe something that is not there.
#[test]
fn an_unranked_run_does_not_talk_about_a_ranking() {
    let coverage = views::RouteCoverage {
        ranked: false,
        markets_found: 9,
        markets_polled: 7,
        markets_priced: 7,
        markets_absent: 2,
        requests_sent: 9,
        ..views::RouteCoverage::default()
    };
    let notes = coverage.notes().join("\n");
    assert!(notes.contains("every price was read live"), "{notes}");
    assert!(!notes.contains("below"), "{notes}");
    assert!(!notes.contains("ranking"), "{notes}");

    // And the heading is the caller's.
    let text = emit(&views::coverage_titled("EDDN IMPORT", &coverage), 200);
    assert!(text.contains("== EDDN IMPORT"), "{text}");
    assert!(!text.contains("ROUTE COVERAGE"), "{text}");
}
