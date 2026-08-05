//! `market --market-id` — one poll, and the four ways it can go wrong.
//!
//! One `#[test]`; see `support/mod.rs` for why.

mod support;

use support::{
    CAPI, FakeHttp, NOT_A_LISTING, RESPONSE_NONCE, Reply, drive, drive_with_env, listing, reply,
    sealed,
};

fn http(replies: Vec<Reply>) -> FakeHttp {
    FakeHttp::default().route("/2.0/elite/market/list", replies)
}

#[test]
fn one_market_by_id_end_to_end() {
    // -- success ------------------------------------------------------------
    let run = drive(&["market", "--market-id", "4306502403"], &http(vec![sealed(&listing(10))]));
    run.assert_exit(0);
    assert!(run.stderr.is_empty());
    insta::assert_snapshot!("success", run.stdout);

    // R77: on this path `--json` is passed as `quiet`, so both tables really
    // are suppressed and the document is the only thing on stdout.
    let run = drive(
        &["market", "--market-id", "4306502403", "--json"],
        &http(vec![sealed(&listing(10))]),
    );
    run.assert_exit(0);
    assert!(run.stdout.starts_with('{'), "no table precedes the document");
    insta::assert_snapshot!("success_json", run.stdout);

    // -- 405, with a diagnosis ---------------------------------------------
    // R73: the `Allow` header is echoed raw, and the suggested verb is
    // `verbs[0]`.
    let run = drive(
        &["market", "--market-id", "4306502403"],
        &http(vec![reply(405, &[("allow", "PUT, OPTIONS")], "")]),
    );
    run.assert_exit(1);
    insta::assert_snapshot!("http_405_stdout", run.stdout);
    insta::assert_snapshot!("http_405_stderr", run.stderr);

    // R73 again: an empty `Allow` is falsy, so a 405 carrying one produces no
    // diagnosis line at all rather than an empty sentence.
    let run = drive(
        &["market", "--market-id", "4306502403"],
        &http(vec![reply(405, &[("allow", "")], "")]),
    );
    run.assert_exit(1);
    assert_eq!(
        run.stderr,
        "GET /2.0/elite/market/list failed: HTTP 405 Method Not Allowed\n"
    );

    // -- 500 ----------------------------------------------------------------
    let run = drive(
        &["market", "--market-id", "4306502403"],
        &http(vec![reply(500, &[], "upstream exploded")]),
    );
    run.assert_exit(1);
    // R76: the failure line went to stderr, the body to **stdout**.
    assert!(run.stdout.ends_with("upstream exploded\n"));
    insta::assert_snapshot!("http_500_stderr", run.stderr);

    // -- a 2xx that will not open ------------------------------------------
    let run = drive(
        &["market", "--market-id", "4306502403"],
        &http(vec![reply(
            200,
            &[("nonce", RESPONSE_NONCE), ("uncompressedsize", "64")],
            "AAAAAAAAAAAAAAAA",
        )]),
    );
    run.assert_exit(1);
    assert_eq!(
        run.stderr,
        "Could not decrypt response: Decrypted response lacks the EDDE compression header\n"
    );
    assert!(run.stdout.ends_with("AAAAAAAAAAAAAAAA\n"));

    // R72: an absent nonce header renders as an unquoted `null`, because the
    // message goes through `JSON.stringify`.
    let run = drive(
        &["market", "--market-id", "4306502403"],
        &http(vec![reply(200, &[("uncompressedsize", "64")], "")]),
    );
    run.assert_exit(1);
    assert_eq!(run.stderr, "Missing or invalid response Nonce header: null\n");

    // -- a body that opens but is not a listing ----------------------------
    let run = drive(&["market", "--market-id", "4306502403"], &http(vec![sealed(NOT_A_LISTING)]));
    // Nothing failed: the market simply had nothing to say.
    run.assert_exit(0);
    assert!(run.stdout.contains("== PAYLOAD "));
    assert!(run.stdout.contains("\"Market not found\""));

    // -- --dry-run ----------------------------------------------------------
    // R74: the REQUEST table prints *before* the bail, so a dry run still shows
    // exactly what would have been sent — and sends nothing.
    let run = drive(&["market", "--market-id", "4306502403", "--dry-run"], &http(vec![]));
    run.assert_exit(0);
    assert!(run.stdout.contains("== REQUEST  GET /2.0/elite/market/list "));
    assert!(!run.stdout.contains("== RESPONSE"));
    assert!(run.calls.is_empty());

    // -- C24: the origin override moves the request and the table -----------
    let run = drive_with_env(
        &["market", "--market-id", "4306502403"],
        &FakeHttp::default()
            .route("http://localhost:9/2.0/elite/market/list", vec![sealed(&listing(10))]),
        vec![("EDM_ORIGIN_OVERRIDE".to_owned(), "http://localhost:9".to_owned())],
    );
    run.assert_exit(0);
    assert_eq!(run.calls, ["GET http://localhost:9/2.0/elite/market/list"]);
    assert!(run.stdout.contains("http://localhost:9/2.0/elite/market/list"));
    assert!(!run.stdout.contains(CAPI));
}
