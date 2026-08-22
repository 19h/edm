//! `route --quick` accepts `--category` as a commodity source.
//!
//! Its own test binary because `Out` redirects process-global stdout, so two
//! tests that read it cannot share a process; see `support/mod.rs`.

mod support;

use support::{FakeHttp, drive, json_reply};

const SOL: &str =
    r#"{"systemName":"Sol","systemAddress":10477373803,"systemX":0,"systemY":0,"systemZ":0}"#;

/// A class is a list of commodities. `--quick` used to refuse this argv for
/// want of `--item` even though `--category metals` had already named the
/// cargo. The unindexed name in the catalogue must not be queried just because
/// it sat next to gold.
#[test]
fn a_category_looks_up_the_commodities_ardent_indexes_in_it() {
    let http = FakeHttp::default()
        .route("/v2/system/name/Sol", vec![json_reply(SOL)])
        .route(
            "/v2/commodities",
            vec![json_reply(
                r#"[{"commodityName":"gold"},{"commodityName":"unobtainium"},{"commodityName":"silver"}]"#,
            )],
        )
        .route("/commodity/name/gold/nearby/exports", vec![json_reply("[]")])
        .route("/commodity/name/gold/nearby/imports", vec![json_reply("[]")])
        .route("/v2/system/name/Sol/commodity/name/gold", vec![json_reply("[]")])
        .route("/commodity/name/silver/nearby/exports", vec![json_reply("[]")])
        .route("/commodity/name/silver/nearby/imports", vec![json_reply("[]")])
        .route(
            "/v2/system/name/Sol/commodity/name/silver",
            vec![json_reply("[]")],
        );

    let run = drive(
        &[
            "route",
            "Sol",
            "--quick",
            "1",
            "--category",
            "metals",
            "--qty",
            "100",
            "--shape",
            "one-way",
            "--dry-run",
        ],
        &http,
    );

    run.assert_exit(0);
    assert!(
        run.stdout
            .contains("--category \"Metals\" is 2 commodities"),
        "the expansion has to be said: {}",
        run.stdout
    );
    let queried: Vec<_> = run
        .calls
        .iter()
        .filter(|call| call.contains("/commodity/name/"))
        .cloned()
        .collect();
    assert!(
        queried
            .iter()
            .any(|call| call.contains("/commodity/name/gold")),
        "gold is a metal: {queried:?}"
    );
    assert!(
        queried
            .iter()
            .any(|call| call.contains("/commodity/name/silver")),
        "silver is a metal: {queried:?}"
    );
    assert!(
        !queried.iter().any(|call| call.contains("unobtainium")),
        "an uncategorised id is not a metal: {queried:?}"
    );
    assert!(
        !run.stderr.contains("--quick needs --item"),
        "a class is a commodity source: {}",
        run.stderr
    );
}
