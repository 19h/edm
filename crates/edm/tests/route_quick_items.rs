//! `route --quick` over a list: every named commodity is accounted for.
//!
//! A separate test binary from `route_quick.rs` because `Out` redirects
//! process-global stdout, so two tests that read it cannot share a process;
//! see `support/mod.rs`.

mod support;

use support::{FakeHttp, drive, json_reply, sealed};

const SOL: &str =
    r#"{"systemName":"Sol","systemAddress":10477373803,"systemX":0,"systemY":0,"systemZ":0}"#;

const EXPORTS: &str = r#"[
  {"commodityName":"gold","marketId":128016384,"stationName":"Galileo","stationType":"Ocellus","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"buyPrice":9000,"stock":5000}
]"#;
const IMPORTS: &str = r#"[
  {"commodityName":"gold","marketId":128016576,"stationName":"Titan City","stationType":"Coriolis","distanceToArrival":505,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"sellPrice":11500,"demand":7000,"demandBracket":3}
]"#;
const LOCAL: &str = "[]";

/// A lookup over several commodities must not answer for the ones it could and
/// stay silent about the one it could not.
#[test]
fn a_commodity_that_nominated_nothing_is_named_rather_than_dropped_from_the_answer() {
    let http = FakeHttp::default()
        .route("/v2/system/name/Sol", vec![json_reply(SOL)])
        // Ardent indexes all three names, so each one is queried; two of them
        // simply have nothing to offer near Sol.
        .route(
            "/v2/commodities",
            vec![json_reply(
                r#"[{"commodityName":"gold"},{"commodityName":"silver"},{"commodityName":"unobtainium"}]"#,
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
        // Ardent knows no such id, so all three of its pages are empty. This is
        // what a misspelt --item looks like from here. The nearby routes are
        // registered first: `FakeHttp` matches the first needle the URL
        // contains, and the bare path is a prefix of both.
        .route(
            "/commodity/name/unobtainium/nearby/exports",
            vec![json_reply("[]")],
        )
        .route(
            "/commodity/name/unobtainium/nearby/imports",
            vec![json_reply("[]")],
        )
        .route(
            "/v2/system/name/Sol/commodity/name/unobtainium",
            vec![json_reply("[]")],
        )
        // A real commodity whose only nearby seller cannot fill the floor: the
        // index knows it, this run still has nothing to offer for it.
        .route(
            "/commodity/name/silver/nearby/exports",
            vec![json_reply(
                r#"[{"commodityName":"silver","marketId":128016900,"stationName":"Thin Seam","stationType":"Coriolis","distanceToArrival":10,"maxLandingPadSize":3,"systemAddress":10477373803,"systemName":"Sol","systemX":0,"systemY":0,"systemZ":0,"buyPrice":4500,"stock":4}]"#,
            )],
        )
        .route(
            "/commodity/name/silver/nearby/imports",
            vec![json_reply("[]")],
        )
        .route(
            "/v2/system/name/Sol/commodity/name/silver",
            vec![json_reply("[]")],
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
        );

    let argv = &[
        "route",
        "Sol",
        "--quick",
        "1",
        "--item",
        "gold,unobtainium,silver",
        "--qty",
        "100",
        "--shape",
        "one-way",
        "--no-cache",
        "--concurrency",
        "1",
        "--rps",
        "100",
    ];
    let run = drive(argv, &http);
    run.assert_exit(0);
    assert!(
        run.stdout
            .contains("\"unobtainium\": Ardent's price index returned no row at all"),
        "a name Ardent does not index must say so: {}",
        run.stdout
    );
    assert!(
        run.stdout
            .contains("\"silver\": Ardent has price rows for it, but none survived"),
        "an indexed commodity that lost to the floor is a different fact: {}",
        run.stdout
    );
    // The one commodity that did work is still answered in full.
    assert!(run.stdout.contains("BEST SINGLE HOPS"), "{}", run.stdout);
    assert!(run.stdout.contains("Gold"), "{}", run.stdout);
}
