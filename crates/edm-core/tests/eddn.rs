//! The EDDN `commodity/3` message, validated against the real schema.
//!
//! `tests/fixtures/eddn-commodity-v3.0.json` is a verbatim copy of
//! `schemas/commodity-v3.0.json` from the EDCD/EDDN repository (BSD 3-Clause),
//! vendored so the suite does not depend on a sibling checkout.
//!
//! Schema validity is a *signal*, not the acceptance gate — byte-identity with
//! the TypeScript is. But it is a sharp signal: the schema's `disallowed`
//! definition matches no JSON value at all, and its prices are declared
//! `integer` in a draft-04 document that the gateway validates with CPython,
//! where `123.0` is not an integer.

use edm_core::domain::Commodity;
use edm_core::domain::eddn::{EddnOptions, EddnStation, build_message};
use edm_core::js::json::JsValue;

fn schema() -> serde_json::Value {
    let raw = include_str!("fixtures/eddn-commodity-v3.0.json");
    serde_json::from_str(raw).expect("the vendored EDDN schema parses")
}

/// Round-trips our own serializer's bytes back through an independent parser,
/// so what gets validated is exactly what would go on the wire.
fn as_sent(payload: &JsValue) -> serde_json::Value {
    serde_json::from_str(&payload.stringify_compact()).expect("our output is valid JSON")
}

fn commodity(name: &'static str, category: &'static str) -> Commodity<'static> {
    Commodity {
        id: 128_049_204.0,
        name,
        category,
        stock: 1234.0,
        stock_bracket: 2.0,
        buy_price: 517.0,
        sell_price: 505.0,
        fence_price: 0.0,
        demand: 0.0,
        demand_bracket: 0.0,
        mean_price: 500.0,
        consumer: false,
        producer: true,
        rare: false,
        illegal: false,
    }
}

fn station() -> EddnStation {
    EddnStation {
        system_name: "Colonia".to_owned(),
        station_name: "Jaques Station".to_owned(),
        station_type: Some("Coriolis".to_owned()),
        economies: None,
    }
}

fn options() -> EddnOptions {
    EddnOptions { uploader_id: "F1234567".to_owned(), ..EddnOptions::default() }
}

#[test]
fn a_built_message_validates_against_the_published_schema() {
    let schema = schema();
    let validator = jsonschema::validator_for(&schema).expect("draft-04 schema compiles");

    let commodities = [commodity("Silver", "Metals"), commodity("Gold", "Metals")];
    let message =
        build_message(&station(), 128_667_761.0, &commodities, "2026-08-05T12:00:00.000Z", &options());

    let instance = as_sent(&message.payload);
    if let Err(error) = validator.validate(&instance) {
        panic!("{error}\n\npayload was:\n{}", message.payload.stringify(2));
    }
    assert_eq!(message.count, 2);
}

/// The finding that would have earned an HTTP 400 on every upload: an integral
/// price must not carry a decimal point. F2 / R3.
#[test]
fn integral_prices_serialize_as_integers() {
    let commodities = [commodity("Silver", "Metals")];
    let message =
        build_message(&station(), 1.0, &commodities, "2026-08-05T12:00:00.000Z", &options());
    let text = message.payload.stringify_compact();
    assert!(text.contains(r#""meanPrice":500"#), "got: {text}");
    assert!(text.contains(r#""buyPrice":517"#), "got: {text}");

    // The assertion that matters is the one the gateway makes: after parsing,
    // every quantity must be an *integer*, because draft-04's `type: integer`
    // is checked in CPython where `isinstance(123.0, int)` is False. Round-trip
    // through an independent parser and ask it the same question.
    let sent = as_sent(&message.payload);
    let row = &sent["message"]["commodities"][0];
    for field in
        ["meanPrice", "buyPrice", "stock", "stockBracket", "sellPrice", "demand", "demandBracket"]
    {
        assert!(
            row[field].is_i64(),
            "{field} came back as {} — a float here is an unretryable HTTP 400",
            row[field]
        );
    }
    assert!(sent["message"]["marketId"].is_i64());
}

/// `commodity-README.md:48` — limpets and anything with a legality string are
/// not market data and must not be sent.
#[test]
fn non_marketable_and_illegal_goods_are_skipped() {
    let mut illegal = commodity("BasicNarcotics", "Narcotics");
    illegal.illegal = true;
    let commodities =
        [commodity("Silver", "Metals"), commodity("Limpet", "NonMarketable"), illegal];

    let message =
        build_message(&station(), 1.0, &commodities, "2026-08-05T12:00:00.000Z", &options());
    assert_eq!(message.count, 1);

    let text = message.payload.stringify_compact();
    assert!(text.contains(r#""name":"silver""#), "names are lowercased to the symbol form");
    assert!(!text.contains("limpet"));
    assert!(!text.contains("basicnarcotics"));
}

/// "You MUST NOT send empty lists", and an unknown expansion flag is a missing
/// key rather than a `false`.
#[test]
fn absent_is_not_the_same_as_empty_or_false() {
    let commodities = [commodity("Silver", "Metals")];

    let mut bare = station();
    bare.station_type = None;
    bare.economies = Some(Vec::new());
    let message =
        build_message(&bare, 1.0, &commodities, "2026-08-05T12:00:00.000Z", &options());
    let text = message.payload.stringify_compact();
    assert!(!text.contains("economies"), "an empty list is omitted, not sent: {text}");
    assert!(!text.contains("stationType"));
    assert!(!text.contains("horizons"), "an unknown flag is omitted: {text}");

    let known = EddnOptions { horizons: Some(false), ..options() };
    let message =
        build_message(&station(), 1.0, &commodities, "2026-08-05T12:00:00.000Z", &known);
    assert!(message.payload.stringify_compact().contains(r#""horizons":false"#));
}

/// Message key order is insertion order and is diffed byte-for-byte against the
/// TypeScript, so it is asserted rather than left to chance.
#[test]
fn message_key_order_is_stable() {
    let commodities = [commodity("Silver", "Metals")];
    let full = EddnStation {
        economies: Some(vec![("Industrial".to_owned(), 1.0)]),
        ..station()
    };
    let options = EddnOptions { horizons: Some(true), odyssey: Some(true), ..options() };
    let message =
        build_message(&full, 1.0, &commodities, "2026-08-05T12:00:00.000Z", &options);

    let payload = message.payload.as_object().expect("an object");
    assert_eq!(
        payload.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        ["$schemaRef", "header", "message"]
    );
    let inner = payload.get("message").and_then(JsValue::as_object).expect("message");
    assert_eq!(
        inner.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        [
            "systemName",
            "stationName",
            "marketId",
            "timestamp",
            "commodities",
            "stationType",
            "economies",
            "horizons",
            "odyssey",
        ]
    );
}

/// The `/test` schema is accepted by the gateway but not relayed onward.
#[test]
fn the_test_schema_is_a_suffix() {
    let commodities = [commodity("Silver", "Metals")];
    let options = EddnOptions { test: true, ..options() };
    let message =
        build_message(&station(), 1.0, &commodities, "2026-08-05T12:00:00.000Z", &options);
    assert_eq!(
        message.payload.as_object().unwrap().get("$schemaRef").unwrap().as_str().unwrap(),
        "https://eddn.edcd.io/schemas/commodity/3/test"
    );
}

/// The game-internal API sends fractional quantities and EDDN's schema does not
/// accept them.
///
/// `Water` with a demand of `113.47560000000001` is a real row from a real
/// market. 29,370 such values appear across 29,152 markets scanned on
/// 2026-08-06, and **29.7% of markets carry at least one** — so nearly a third
/// of all uploads were answered with HTTP 400 and `FAIL: Schema Validation`.
///
/// Truncation matches `EDMarketConnector/plugins/eddn.py:624-629`, which is
/// what keeps this program's rows for a market in step with every other
/// uploader's.
#[test]
fn a_fractional_quantity_is_truncated_the_way_every_other_uploader_truncates_it() {
    let payload = JsValue::parse(
        r#"{"commodities":{"128049166":{
            "id":128049166,"name":"Water","categoryname":"Foods",
            "stock":0,"stockBracket":0,
            "buyPrice":0,"sellPrice":711,"meanPrice":260.9,
            "demand":113.47560000000001,"demandBracket":3
        }}}"#,
    )
    .expect("a document");
    let snapshot = edm_core::domain::parse_market_snapshot(&payload).expect("a market");

    let message = build_message(
        &station(),
        4_306_502_403.0,
        &snapshot.commodities,
        "2026-08-06T00:00:00.000Z",
        &EddnOptions::default(),
    );

    let text = message.payload.stringify_compact();
    assert!(text.contains(r#""demand":113"#), "{text}");
    assert!(!text.contains("113.4"), "no fraction survives to the wire\n{text}");
    // And the price is coerced the same way, for the same reason.
    assert!(text.contains(r#""meanPrice":260"#), "{text}");
}

/// A bracket is not a quantity. The schema's `levelType` is the enum
/// `[0, 1, 2, 3, ""]`, and every value the game-internal API has been observed to
/// send is already in it — 29,152 markets scanned, not one outside. Truncating
/// an unexpected value would turn it into a plausible wrong one instead of a
/// loud failure.
#[test]
fn brackets_are_passed_through_rather_than_coerced() {
    let payload = JsValue::parse(
        r#"{"commodities":{"1":{
            "id":1,"name":"Water","categoryname":"Foods",
            "stock":0,"stockBracket":0,
            "buyPrice":0,"sellPrice":711,"meanPrice":260,
            "demand":5,"demandBracket":3
        }}}"#,
    )
    .expect("a document");
    let snapshot = edm_core::domain::parse_market_snapshot(&payload).expect("a market");
    let message = build_message(
        &station(),
        1.0,
        &snapshot.commodities,
        "2026-08-06T00:00:00.000Z",
        &EddnOptions::default(),
    );
    assert!(message.payload.stringify_compact().contains(r#""demandBracket":3"#));
}
