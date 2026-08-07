//! `vendor` — direct-market and system-wide Pioneer Supplies lookup.
//!
//! One `#[test]`; see `support/mod.rs` for why descriptor capture requires it.

mod support;

use support::{FakeHttp, drive, json_reply, sealed};

const MARKET: &str = r#"{
  "marketId":4370953219,
  "stationName":"Jaques Station",
  "stationType":"Orbis",
  "systemName":"Colonia"
}"#;

const STOCK: &str = r#"{
  "premiumstock":{
    "personaweapon":[
      {"name":"Wpn_S_Pistol_Laser_SAuto","id":"128937288","class":3,
       "quantity":1,"credits_basevalue":50000,"credits_withmods_value":1250000,
       "mods":{"Mod1":null,"Mod2":"weapon_mod_stability"}},
      {"name":"Wpn_S_Pistol_Plasma_Charged","id":"128937281","class":2,
       "quantity":0,"credits_basevalue":50000,"credits_withmods_value":250000,
       "mods":{"Mod1":null}}
    ],
    "personasuit":[]
  },
  "premiumStockKey":1785999600,
  "outfitting":{
    "personaweapon":{
      "128937271":{"id":"128937271","name":"Wpn_M_AssaultRifle_Kinetic_FAuto",
                   "class":1,"credits_basevalue":125000}
    },
    "personasuit":{}
  },
  "microresources":{"Consumable":{}}
}"#;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the descriptor-capture harness requires every scenario in this binary to share one test"
)]
fn vendor_dispatches_and_aggregates_market_scoped_stock() {
    let direct_http = FakeHttp::default()
        .route("/v2/market/4370953219", vec![json_reply(MARKET)])
        .route("/2.0/elite/vendors/items", vec![sealed(STOCK)]);
    let run = drive(&["vendor", "--market-id", "4370953219"], &direct_http);
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        [
            "GET https://api.ardent-insight.com/v2/market/4370953219",
            "GET https://api.orerve.net/2.0/elite/vendors/items",
        ]
    );
    assert!(run.stdout.contains("Takada Zenith"), "{}", run.stdout);
    assert!(run.stdout.contains("Karma AR-50"), "{}", run.stdout);
    assert!(
        !run.stdout.contains("Manticore Tormentor"),
        "sold-out premium stock is hidden"
    );

    let json_http = FakeHttp::default()
        .route("/v2/market/4370953219", vec![json_reply(MARKET)])
        .route("/2.0/elite/vendors/items", vec![sealed(STOCK)]);
    let run = drive(
        &["vendor", "--market-id", "4370953219", "--json", "--detail"],
        &json_http,
    );
    run.assert_exit(0);
    let document: serde_json::Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    assert_eq!(document["summary"]["markets"], 1);
    assert_eq!(document["summary"]["items"], 3);
    assert_eq!(
        document["markets"][0]["payload"]["premiumStockKey"],
        1_785_999_600u64
    );
    assert_eq!(
        document["markets"][0]["items"][0]["symbol"],
        "Wpn_S_Pistol_Laser_SAuto"
    );

    let wrong_shape_http = FakeHttp::default()
        .route("/v2/market/4370953219", vec![json_reply(MARKET)])
        .route(
            "/2.0/elite/vendors/items",
            vec![sealed(r#"{"errors":["not inventory"]}"#)],
        );
    let run = drive(
        &["vendor", "--market-id", "4370953219", "--json"],
        &wrong_shape_http,
    );
    run.assert_exit(1);
    let document: serde_json::Value =
        serde_json::from_str(&run.stdout).expect("failure remains one JSON document");
    assert_eq!(document["summary"]["failed"], 1);
    assert_eq!(
        document["markets"][0]["payload"]["errors"][0],
        "not inventory"
    );
    assert!(run.stderr.contains("without vendor inventory data"));

    let station_http = FakeHttp::default()
        .route(
            "/v2/search/station/name/Jaques%20Station",
            vec![json_reply(
                r#"[{"marketId":4370953219,"stationName":"Jaques Station",
                    "stationType":"Orbis","systemName":"Colonia","maxLandingPadSize":3}]"#,
            )],
        )
        .route(
            "/v2/system/name/Colonia",
            vec![json_reply(
                r#"{"systemName":"Colonia","systemAddress":3238296097059,
                    "systemX":-9530.5,"systemY":-910.28125,"systemZ":19808.125}"#,
            )],
        )
        .route("/2.0/elite/vendors/items", vec![sealed(STOCK)]);
    let run = drive(&["vendor", "--station", "Jaques Station"], &station_http);
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        [
            "GET https://api.ardent-insight.com/v2/search/station/name/Jaques%20Station",
            "GET https://api.ardent-insight.com/v2/system/name/Colonia",
            "GET https://api.orerve.net/2.0/elite/vendors/items",
        ],
        "Ardent's station marketId selects one Frontier market without a system sweep"
    );

    let system_http = FakeHttp::default()
        .route(
            "/v2/system/name/Test",
            vec![json_reply(
                r#"{"systemName":"Test","systemAddress":42,"systemX":1,"systemY":2,"systemZ":3}"#,
            )],
        )
        .route(
            "/v2/system/name/Test/markets",
            vec![json_reply(
                r#"[
                  {"marketId":4370953219,"stationName":"Jaques Station","systemName":"Test",
                   "stationType":"Orbis","maxLandingPadSize":3,"distanceToArrival":10},
                  {"marketId":4370953220,"stationName":"ABC-123","systemName":"Test",
                   "stationType":"FleetCarrier","maxLandingPadSize":3,"distanceToArrival":20}
                ]"#,
            )],
        )
        .route("/2.0/elite/vendors/items", vec![sealed(STOCK)]);
    let run = drive(&["vendor", "--system", "Test"], &system_http);
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        [
            "GET https://api.ardent-insight.com/v2/system/name/Test",
            "GET https://api.ardent-insight.com/v2/system/name/Test/markets",
            "GET https://api.orerve.net/2.0/elite/vendors/items",
        ],
        "fleet carriers are excluded before Frontier is queried"
    );

    let dry_http = FakeHttp::default()
        .route(
            "/v2/system/name/Test",
            vec![json_reply(
                r#"{"systemName":"Test","systemAddress":42,"systemX":1,"systemY":2,"systemZ":3}"#,
            )],
        )
        .route(
            "/v2/system/name/Test/markets",
            vec![json_reply(
                r#"[{"marketId":4370953219,"stationName":"Jaques Station",
                     "systemName":"Test","stationType":"Orbis"}]"#,
            )],
        );
    let run = drive(&["vendor", "--system", "Test", "--dry-run"], &dry_http);
    run.assert_exit(0);
    assert_eq!(
        run.calls.len(),
        2,
        "system dry-run sends no Frontier request"
    );
    assert!(run.stdout.contains("VENDOR SEARCH PLAN") || run.stdout.contains("REQUEST"));

    let run = drive(&["vendor", "--help"], &FakeHttp::default());
    run.assert_exit(0);
    assert!(
        run.stdout
            .contains("edm vendor — find live Pioneer Supplies stock")
    );
    assert!(run.calls.is_empty());
}
