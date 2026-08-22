//! `route --quick` refuses a commodity Ardent does not index.
//!
//! Its own test binary because `Out` redirects process-global stdout, so two
//! tests that read it cannot share a process; see `support/mod.rs`.

mod support;

use support::{FakeHttp, drive, json_reply};

const SOL: &str =
    r#"{"systemName":"Sol","systemAddress":10477373803,"systemX":0,"systemY":0,"systemZ":0}"#;

/// Ardent answers an unknown commodity id with `200` and an empty page, which
/// downstream is indistinguishable from a region with no stock. Left unchecked,
/// a misspelt `--item` is a successful run that reports nothing.
#[test]
fn an_unindexed_commodity_is_refused_before_a_single_query_is_spent() {
    let http = FakeHttp::default()
        .route("/v2/system/name/Sol", vec![json_reply(SOL)])
        .route(
            "/v2/commodities",
            vec![json_reply(
                r#"[{"commodityName":"gold"},{"commodityName":"lowtemperaturediamond"}]"#,
            )],
        );

    let run = drive(
        &[
            "route",
            "Sol",
            "--quick",
            "1",
            "--item",
            "Gild",
            "--dry-run",
        ],
        &http,
    );

    run.assert_exit(1);
    assert!(
        run.stderr.contains("is not a commodity Ardent indexes"),
        "{}",
        run.stderr
    );
    // Quoted as typed, so the correction is in the words the user used.
    assert!(run.stderr.contains("\"Gild\""), "{}", run.stderr);
    assert!(
        run.stderr.contains("Did you mean \"gold\"?"),
        "{}",
        run.stderr
    );
    // The refusal happens before any price page is asked for, so a typo costs
    // one cached catalogue read and nothing else.
    assert!(
        !run.calls
            .iter()
            .any(|call| call.contains("/commodity/name/")),
        "a rejected item was queried anyway: {:?}",
        run.calls
    );
    assert!(
        !run.calls.iter().any(|call| call.contains("/2.0/elite/")),
        "a rejected item reached the game API: {:?}",
        run.calls
    );
}
