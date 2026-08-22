//! `market <system>` — the sweep, its filters, its requeues and R78.
//!
//! Every sweep here runs one worker, so the reply queue is handed out in
//! listing order and the mix is scriptable. One `#[test]`; see
//! `support/mod.rs` for why.

mod support;

use support::{
    ARDENT_COLONIA, FakeHttp, NOT_A_LISTING, Reply, drive, json_reply, listing, reply, sealed,
    starsystem,
};

/// Five of the seven markets survive the filters: one is a fleet carrier and
/// one has no commodity market, so both "skipped" rows appear.
fn sweep(market_replies: Vec<Reply>) -> FakeHttp {
    FakeHttp::default()
        .route("/v2/system/name/", vec![json_reply(ARDENT_COLONIA)])
        .route("/2.0/elite/starsystem", vec![sealed(&starsystem())])
        .route("/2.0/elite/market/list", market_replies)
}

/// A 500 that is requeued and then succeeds, a 404 that is not, and a 200 whose
/// body is not a listing — which is `HTTP 200`, and so **not** transient
/// \[R84\].
fn mixed() -> Vec<Reply> {
    vec![
        sealed(&listing(10)),
        reply(500, &[], ""),
        reply(404, &[], ""),
        sealed(NOT_A_LISTING),
        sealed(&listing(0)),
        sealed(&listing(10)),
    ]
}

/// Every market answers 2xx, and two of them with something that is not a
/// listing — so nothing inside `send` ever sets the exit code and the only
/// candidate is the sweep's own failure tally.
fn opaque_only() -> Vec<Reply> {
    vec![
        sealed(&listing(10)),
        sealed(NOT_A_LISTING),
        sealed(&listing(0)),
        sealed(NOT_A_LISTING),
        sealed(&listing(10)),
    ]
}

#[test]
fn a_system_sweep_end_to_end() {
    let flags = ["market", "Colonia", "--concurrency", "1", "--requeue", "1"];

    // -- the mix ------------------------------------------------------------
    let run = drive(&flags, &sweep(mixed()));
    // Two markets produced nothing usable.
    run.assert_exit(1);
    assert!(
        run.stdout
            .contains("[requeue 1/1] Ohm City (3229009409): HTTP 500")
    );
    insta::assert_snapshot!("mixed_stdout", run.stdout);
    insta::assert_snapshot!("mixed_stderr", run.stderr);

    // R78: a `--json` sweep returns before the failure tally. Two markets still
    // produced no usable data, and the exit code stays **0** because no
    // non-2xx ever reached `send`.
    let mut json_flags = flags.to_vec();
    json_flags.push("--json");
    let run = drive(&json_flags, &sweep(opaque_only()));
    run.assert_exit(0);
    insta::assert_snapshot!("opaque_json", run.stdout);

    // The same replies without `--json` do set it, through the "no usable
    // data" path.
    let run = drive(&flags, &sweep(opaque_only()));
    run.assert_exit(1);
    assert!(run.stdout.contains("2 markets returned no usable data"));

    // -- filters ------------------------------------------------------------
    // `--carriers` and `--all-markets` each restore one market, and the
    // corresponding "skipped" row disappears.
    let run = drive(
        &[
            "market",
            "Colonia",
            "--concurrency",
            "1",
            "--carriers",
            "--all-markets",
        ],
        &sweep((0..7).map(|_| sealed(&listing(10))).collect()),
    );
    run.assert_exit(0);
    assert!(run.stdout.contains("| markets  | 7 of 7"));
    assert!(!run.stdout.contains("carriers skipped"));
    assert!(!run.stdout.contains("no-market skipped"));

    // `--detail` re-emits each market's full snapshot.
    //
    // DEVIATION: the TypeScript prints these from inside the worker (ts:1550),
    // interleaved with the progress lines and in completion order;
    // `crate::sweep::sweep` offers no hook for that, so `cmd::market` emits
    // them after the pool drains. Asserted only for presence and count, so the
    // test does not pin the wrong ordering.
    let run = drive(
        &["market", "Colonia", "--concurrency", "1", "--detail"],
        &sweep((0..5).map(|_| sealed(&listing(10))).collect()),
    );
    run.assert_exit(0);
    assert_eq!(
        run.stdout
            .matches("== MARKET  Jaques Station (3229009408) ")
            .count(),
        1
    );
    assert_eq!(
        run.stdout
            .matches("== COMMODITIES  3 entries in 2 categories ")
            .count(),
        5
    );

    // -- R87 ----------------------------------------------------------------
    // Under `--dry-run` a sweep leaves `failure` null, so nothing is requeued,
    // every row reads `no data`, and the run still exits 1.
    let run = drive(
        &["market", "Colonia", "--concurrency", "1", "--dry-run"],
        &sweep(vec![]),
    );
    run.assert_exit(1);
    assert!(!run.stdout.contains("[requeue"));
    assert_eq!(
        run.stdout.matches("HTTP -  no data").count(),
        5,
        "one row per surviving market"
    );
    // R74: the starsystem read carries `ignoreDryRun`, so it happened anyway.
    assert_eq!(
        run.calls,
        [
            "GET https://api.ardent-insight.com/v2/system/name/Colonia",
            "GET https://api.orerve.net/2.0/elite/starsystem",
        ]
    );

    // -- the two ways a sweep refuses to start ------------------------------
    let empty = FakeHttp::default()
        .route("/v2/system/name/", vec![json_reply(ARDENT_COLONIA)])
        .route("/2.0/elite/starsystem", vec![sealed("{}")]);
    let run = drive(&["market", "Colonia"], &empty);
    run.assert_exit(1);
    assert_eq!(
        run.stderr,
        "No markets found in that system; run `markets --dump <file>` to inspect the payload\n"
    );

    let unreadable = FakeHttp::default()
        .route("/v2/system/name/", vec![json_reply(ARDENT_COLONIA)])
        .route("/2.0/elite/starsystem", vec![reply(503, &[], "")]);
    let run = drive(&["market", "Colonia"], &unreadable);
    run.assert_exit(1);
    assert!(
        run.stderr.ends_with(
            "Could not read the star system; try `markets` first to see what is there\n"
        )
    );
}
