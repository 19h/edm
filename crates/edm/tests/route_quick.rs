//! `route --quick`: a small Ardent index prefix, then live-only ranking.
//!
//! Kept as its own test binary because `Out` redirects process-global stdout;
//! see `support/mod.rs`.

mod support;

use support::{FakeHttp, drive, json_reply, sealed};

const SOL: &str =
    r#"{"systemName":"Sol","systemAddress":10477373803,"systemX":0,"systemY":0,"systemZ":0}"#;

// The first export is deliberately cheaper but below `--qty`; a client that
// trusts the page order without rechecking the advertised quantity would poll
// it instead of Galileo.
const EXPORTS: &str = r#"[
  {"commodityName":"gold","marketId":999,"stationName":"Too Thin","stationType":"Coriolis","distanceToArrival":10,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"buyPrice":1,"stock":99},
  {"commodityName":"gold","marketId":128016384,"stationName":"Galileo","stationType":"Ocellus","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"buyPrice":9000,"stock":5000}
]"#;
const IMPORTS: &str = r#"[
  {"commodityName":"gold","marketId":128016576,"stationName":"Titan City","stationType":"Coriolis","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"sellPrice":11500,"demand":0,"demandBracket":1}
]"#;

// The nearby endpoint omits its centre system. Its direct commodity sibling
// puts these two zero-Ly stations back into the per-side price selection.
const LOCAL: &str = r#"[
  {"commodityName":"gold","marketId":128016384,"stationName":"Galileo","stationType":"Ocellus","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"buyPrice":9000,"stock":5000,"sellPrice":0,"demand":0,"demandBracket":0},
  {"commodityName":"gold","marketId":128016576,"stationName":"Titan City","stationType":"Coriolis","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"buyPrice":0,"stock":0,"sellPrice":11500,"demand":7000,"demandBracket":3}
]"#;

fn quick_http() -> FakeHttp {
    FakeHttp::default()
        .route("/v2/system/name/Sol", vec![json_reply(SOL)])
        // Every --item is resolved against this before a query is spent on it.
        .route(
            "/v2/commodities",
            vec![json_reply(
                r#"[{"commodityName":"gold"},{"commodityName":"silver"}]"#,
            )],
        )
        .route(
            "/commodity/name/gold/nearby/exports",
            vec![json_reply(EXPORTS)],
        )
        .route(
            "/commodity/name/gold/nearby/imports",
            vec![json_reply(IMPORTS)],
        )
        .route(
            "/v2/system/name/Sol/commodity/name/gold",
            vec![json_reply(LOCAL)],
        )
        .route(
            "/2.0/elite/market/list",
            vec![
                sealed(include_str!(
                    "../../../xtask/scenarios/payloads/market-gold-source.json"
                )),
                sealed(include_str!(
                    "../../../xtask/scenarios/payloads/market-gold-sink.json"
                )),
            ],
        )
        .route("/upload/", vec![json_reply("OK"), json_reply("OK")])
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end scenario deliberately checks selection, live verification, relay and JSON provenance"
)]
fn a_quick_lookup_polls_only_qualifying_candidates_and_relays_live_listings() {
    let http = quick_http();
    let run = drive(
        &[
            "route",
            "Sol",
            "--quick",
            "1",
            "--item",
            "gold",
            "--qty",
            "100",
            "--cargo",
            "784",
            "--shape",
            "one-way",
            "--no-cache",
            "--concurrency",
            "1",
            "--rps",
            "100",
            "--eddn-test",
        ],
        &http,
    );

    run.assert_exit(0);
    assert!(run.stdout.contains("BEST SINGLE HOPS"), "{}", run.stdout);
    // Three price pages for gold, plus the commodity catalogue.
    assert!(run.stdout.contains("4 Ardent queries"), "{}", run.stdout);
    assert!(
        run.stdout
            .contains("not enumerated (commodity price index)"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("Gold"), "{}", run.stdout);
    assert!(run.stdout.contains("unreported"), "{}", run.stdout);
    // The question the mode was asked, answered from the live payloads rather
    // than from the index rows that nominated the markets.
    assert!(run.stdout.contains("BEST LIVE PRICES"), "{}", run.stdout);
    // `market-gold-*` also advertise Silver. It is not allowed to become the
    // answer merely because a chosen live market happens to carry it.
    assert!(!run.stdout.contains("Silver"), "{}", run.stdout);
    assert_eq!(
        run.calls
            .iter()
            .filter(|call| call.contains("/2.0/elite/market/list"))
            .count(),
        2,
        "only the one qualifying seller and buyer are live-polled: {:#?}",
        run.calls
    );
    assert_eq!(
        run.calls
            .iter()
            .filter(|call| call.contains("/upload/"))
            .count(),
        2,
        "each fresh live listing is relayed once: {:#?}",
        run.calls
    );
    assert!(
        run.calls
            .iter()
            .any(|call| call.ends_with("/v2/system/name/Sol/commodity/name/gold")),
        "the reference system is included too: {:?}",
        run.calls
    );
    assert!(
        !run.calls
            .iter()
            .any(|call| call.contains("/v2/system/name/Sol/nearby?")),
        "quick lookup must not fall back to regional enumeration: {:?}",
        run.calls
    );

    // A JSON consumer cannot infer that this was a bounded commodity prefix
    // from ordinary route counts, so the document carries its own provenance.
    let json_http = quick_http();
    let json = drive(
        &[
            "route",
            "Sol",
            "--quick",
            "1",
            "--item",
            "gold",
            "--qty",
            "100",
            "--cargo",
            "784",
            "--shape",
            "one-way",
            "--no-cache",
            "--concurrency",
            "1",
            "--rps",
            "100",
            "--json",
        ],
        &json_http,
    );
    json.assert_exit(0);
    let document: serde_json::Value =
        serde_json::from_str(&json.stdout).expect("quick JSON is one document");
    let quick = &document["coverage"]["quickLookup"];
    assert_eq!(quick["completeRegionalSurvey"], false, "{}", json.stdout);
    assert_eq!(quick["commodities"], serde_json::json!(["gold"]));
    assert_eq!(quick["marketIds"].as_array().map(Vec::len), Some(2));
    assert!(!json.stdout.contains("QUICK LOOKUP"), "{}", json.stdout);
    let best = quick["bestLive"].as_array().expect("live bests");
    assert_eq!(
        best.len(),
        2,
        "one seller and one buyer for gold: {}",
        json.stdout
    );
    assert_eq!(best[0]["side"], "sells");
    assert_eq!(best[0]["stationName"], "Galileo");
    assert_eq!(best[0]["price"], 9000.0);
    assert_eq!(best[0]["indexPrice"], 9000.0);
    assert_eq!(best[1]["side"], "buys");
    assert_eq!(best[1]["stationName"], "Titan City");
    assert_eq!(best[1]["price"], 11500.0);
    // Ardent's index row for this buyer published a bracket and no tonnage —
    // the candidate table above prints it as "unreported". The live read has
    // the number, and this table reports the live read. That difference is the
    // entire argument for polling the candidates rather than ranking the index.
    assert_eq!(best[1]["quantity"], 7000.0);
    assert_eq!(best[1]["quantityUnpublished"], false);

    // The regular spend gate still stops after free candidate discovery under
    // `--dry-run`; no game API request or EDDN message may leak past it.
    let dry_http = quick_http();
    let dry = drive(
        &[
            "route",
            "Sol",
            "--quick",
            "1",
            "--item",
            "gold",
            "--qty",
            "100",
            "--shape",
            "one-way",
            "--dry-run",
        ],
        &dry_http,
    );
    dry.assert_exit(0);
    assert!(
        !dry.calls
            .iter()
            .any(|call| call.contains("/2.0/elite/") || call.contains("/upload/")),
        "dry run leaked a paid or relay request: {:?}",
        dry.calls
    );
}
