//! `trade` — one request, a batch fill, and R76's leak.
//!
//! One `#[test]`; see `support/mod.rs` for why.

mod support;

use support::{FakeHttp, NOT_A_LISTING, Reply, drive, drive_with_env, listing, sealed};

fn http(trade_replies: Vec<Reply>) -> FakeHttp {
    FakeHttp::default()
        .route("/2.0/elite/market/trade", trade_replies)
        .route("/2.0/elite/market/list", vec![sealed(&listing(10))])
}

const BUY_GOLD: [&str; 9] =
    ["trade", "--market-id", "4306502403", "--type", "buy", "--item", "gold", "--qty", "5"];

const FILL: [&str; 10] = [
    "trade",
    "--market-id",
    "4306502403",
    "--type",
    "buy",
    "--item",
    "gold,silver",
    "--cargo",
    "100",
    "--fill",
];

#[test]
fn trading_end_to_end() {
    // -- one trade ----------------------------------------------------------
    let run = drive(&BUY_GOLD, &http(vec![sealed(&listing(15))]));
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        [
            "GET https://api.orerve.net/2.0/elite/market/list",
            "PUT https://api.orerve.net/2.0/elite/market/trade",
        ],
        "the listing is read first, then the trade is sent"
    );
    insta::assert_snapshot!("single_stdout", run.stdout);

    let mut json = BUY_GOLD.to_vec();
    json.push("--json");
    let run = drive(&json, &http(vec![sealed(&listing(15))]));
    run.assert_exit(0);
    insta::assert_snapshot!("single_json", run.stdout);

    // R91/R94: the zero-quantity ladder ends the run before anything is sent,
    // and `derivePrice` gets there before the stock clamp does.
    let run = drive(
        &["trade", "--market-id", "4306502403", "--type", "buy", "--item", "biowaste", "--qty", "5"],
        &http(vec![]),
    );
    run.assert_exit(1);
    assert_eq!(run.calls, ["GET https://api.orerve.net/2.0/elite/market/list"]);
    assert_eq!(run.stderr, "Biowaste is not sold at this market (buyPrice 0)\n");

    // R74: the price lookup carries `ignoreDryRun`, so `--dry-run` still reads
    // the listing — and then prints the request without sending it.
    let mut dry = BUY_GOLD.to_vec();
    dry.push("--dry-run");
    let run = drive(&dry, &http(vec![]));
    run.assert_exit(0);
    assert_eq!(run.calls, ["GET https://api.orerve.net/2.0/elite/market/list"]);
    assert!(run.stdout.contains("== REQUEST  PUT /2.0/elite/market/trade "));

    // -- a batch fill -------------------------------------------------------
    // The first purchase fills the hold, so the second item is never reached
    // and the round ends on `hold is full` \[R90\].
    let batch = || {
        FakeHttp::default()
            .route("/2.0/elite/market/trade", vec![sealed(&listing(100))])
            .route("/2.0/elite/market/list", vec![sealed(&listing(10))])
    };
    let run = drive(&FILL, &batch());
    run.assert_exit(0);
    assert!(run.stdout.contains("hold is full"));
    insta::assert_snapshot!("batch_stdout", run.stdout);

    let mut json = FILL.to_vec();
    json.push("--json");
    let run = drive(&json, &batch());
    run.assert_exit(0);
    insta::assert_snapshot!("batch_json", run.stdout);

    // R90: the stamp is drawn before the dry-run branch, so a dry run consumes
    // the entropy stream identically — and sends nothing.
    let mut dry = FILL.to_vec();
    dry.push("--dry-run");
    let run = drive(
        &dry,
        &FakeHttp::default().route("/2.0/elite/market/list", vec![sealed(&listing(10))]),
    );
    run.assert_exit(0);
    assert_eq!(run.calls, ["GET https://api.orerve.net/2.0/elite/market/list"]);
    assert!(run.stdout.contains("[1] would buy "));
    insta::assert_snapshot!("batch_dry_run", run.stdout);

    // Two guards `loadBatchSettings` applies before any request.
    let run = drive(
        &["trade", "--market-id", "1", "--type", "sell", "--item", "a,b", "--qty", "1", "--fill"],
        &FakeHttp::default(),
    );
    run.assert_exit(1);
    assert_eq!(run.stderr, "--fill only applies to --type buy\n");
    assert!(run.calls.is_empty());

    // -- R76 and its opt-out ------------------------------------------------
    // A listing that is not a listing prints a PAYLOAD block to **stdout** even
    // under `--json`, which corrupts the document that was about to follow.
    let opaque =
        || FakeHttp::default().route("/2.0/elite/market/list", vec![sealed(NOT_A_LISTING)]);
    let mut json = BUY_GOLD.to_vec();
    json.push("--json");

    let leaked = drive(&json, &opaque());
    leaked.assert_exit(1);
    assert!(leaked.stdout.contains("== PAYLOAD "), "the leak is the behaviour under test");
    assert_eq!(leaked.stderr, "Market listing did not contain commodity data\n");

    let strict = drive_with_env(
        &json,
        &opaque(),
        vec![("EDM_STRICT_JSON".to_owned(), "1".to_owned())],
    );
    strict.assert_exit(1);
    assert!(strict.stdout.is_empty(), "nothing may reach the JSON stream");
    assert!(strict.stderr.contains("== PAYLOAD "));
}
