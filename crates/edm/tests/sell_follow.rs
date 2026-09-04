//! `sell --follow`: the plan is redone every round from a fresh read of the
//! buyers, and the loop knows when to stop \[C52\].
//!
//! Its own test binary, and one test in it, because `Out` redirects
//! process-global stdout; see `support/mod.rs`. The runtime's clock is paused,
//! so thirty-second rounds take no time and the trace is exactly what a live
//! session would send.

mod support;

use std::path::PathBuf;

use support::{FakeHttp, drive_with_paused_time, json_reply, sealed};

const SOL: &str =
    r#"{"systemName":"Sol","systemAddress":10477373803,"systemX":0,"systemY":0,"systemZ":0}"#;

/// One buyer, so every market read is for the same id and the scripted
/// replies are consumed in a known order.
const IMPORTS: &str = r#"[
  {"commodityName":"tritium","marketId":3229009410,"stationName":"Takes The Lot","stationType":"Coriolis","distanceToArrival":30,"maxLandingPadSize":3,"systemAddress":7267755828641,"systemName":"Alpha Centauri","systemX":3.03125,"systemY":-0.09375,"systemZ":3.15625,"sellPrice":50000,"demand":9000,"demandBracket":3}
]"#;

const BUYING: &str = r#"{"inventory": [], "commodities": {"128961249": {"id": 128961249, "categoryname": "Chemicals", "name": "Tritium", "stock": 0, "buyPrice": 0, "sellPrice": 50000, "fencePrice": 0, "demand": 9000, "legality": "", "meanPrice": 50000, "demandBracket": 3, "stockBracket": 0, "consumer": 1, "producer": 0, "rare": 0}}, "allowsDumping": true}"#;

/// The same market with its tritium order filled: the row is gone entirely,
/// which is what a filled order looks like on the wire \[C46\].
const NOT_BUYING: &str =
    r#"{"inventory": [], "commodities": {}, "allowsDumping": true}"#;

fn journal() -> Vec<(PathBuf, String)> {
    vec![
        (PathBuf::from("/journals"), String::new()),
        (
            PathBuf::from("/journals/Journal.3309-01-01T000000.01.log"),
            concat!(
                r#"{"timestamp":"3309-01-01T00:00:01Z","event":"LoadGame","Commander":"Test","Ship":"Type9","Credits":1000000,"Loan":0}"#,
                "\n",
                r#"{"timestamp":"3309-01-01T00:00:02Z","event":"Location","StarSystem":"Sol","SystemAddress":10477373803,"StarPos":[0.0,0.0,0.0],"Docked":false}"#,
                "\n",
            )
            .to_owned(),
        ),
        (
            PathBuf::from("/journals/Cargo.json"),
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"Cargo","Vessel":"Ship","Count":1232,"Inventory":[{"Name":"tritium","Count":1232,"Stolen":0}]}"#
                .to_owned(),
        ),
    ]
}

fn env() -> Vec<(String, String)> {
    vec![
        ("EDM_JOURNAL_DIR".to_owned(), "/journals".to_owned()),
        ("EDM_JITTER".to_owned(), "0".to_owned()),
    ]
}

fn ardent(replies: Vec<support::Reply>) -> FakeHttp {
    FakeHttp::default()
        .route("/v2/system/name/Sol", vec![json_reply(SOL)])
        .route("/commodity/name/tritium/nearby/imports", vec![json_reply(IMPORTS)])
        .route("/v2/system/name/Sol/commodity/name/tritium", vec![json_reply("[]")])
        .route("/2.0/elite/market/list", replies)
}

#[test]
fn sell_follow_end_to_end() {
    every_round_re_reads_the_buyers_and_plans_again();
    three_rounds_with_nothing_to_plan_end_the_session();
    follow_with_json_is_refused_before_the_hold_is_read();
}

/// A round is one sweep of the nominated buyers and one plan, and the round
/// cap ends the session cleanly.
fn every_round_re_reads_the_buyers_and_plans_again() {
    let http = ardent(vec![sealed(BUYING), sealed(BUYING), sealed(BUYING)]);
    let run = drive_with_paused_time(
        &["sell", "--from", "Sol", "--radius", "50", "--no-cache", "--follow", "30", "--follow-rounds", "2"],
        &http,
        env(),
        journal(),
    );
    run.assert_exit(0);
    let market_reads = run
        .calls
        .iter()
        .filter(|call| call.ends_with("/2.0/elite/market/list"))
        .count();
    assert_eq!(market_reads, 3, "one sweep, then one per round\n{}", run.stdout);
    assert_eq!(
        run.stdout.matches("== SELL PLAN ==").count(),
        3,
        "a plan under every round\n{}",
        run.stdout
    );
    assert!(run.stdout.contains("round 1: 1 of 1 buyers re-read, 1 requests, 1,232 t of tritium aboard"), "{}", run.stdout);
    assert!(run.stdout.contains("round 2: 1 of 1 buyers re-read"), "{}", run.stdout);
    assert!(run.stdout.contains("--follow-rounds 2 reached"), "{}", run.stdout);
    // Ardent was asked once. A round is a sweep, never a search.
    assert_eq!(
        run.calls.iter().filter(|call| call.contains("ardent")).count(),
        3,
        "{:?}",
        run.calls
    );
}

/// The buyer set is fixed, so once every buyer has stopped buying no round can
/// recover; the loop says so after three empty rounds rather than re-reading
/// dead markets until the ceiling \[C46\].
fn three_rounds_with_nothing_to_plan_end_the_session() {
    let http = ardent(vec![
        sealed(BUYING),
        sealed(NOT_BUYING),
        sealed(NOT_BUYING),
        sealed(NOT_BUYING),
        // Never reached: the loop must stop before asking for this one.
        sealed(BUYING),
    ]);
    let run = drive_with_paused_time(
        &["sell", "--from", "Sol", "--radius", "50", "--no-cache", "--follow", "30", "--follow-rounds", "10"],
        &http,
        env(),
        journal(),
    );
    run.assert_exit(0);
    let market_reads = run
        .calls
        .iter()
        .filter(|call| call.ends_with("/2.0/elite/market/list"))
        .count();
    assert_eq!(market_reads, 4, "{}", run.stdout);
    assert_eq!(run.stdout.matches("== SELL PLAN ==").count(), 1, "{}", run.stdout);
    assert_eq!(run.stdout.matches("round ").count(), 3, "{}", run.stdout);
    assert!(
        run.stdout.contains("nothing could be planned for 3 rounds"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("--follow-rounds 10 reached"), "{}", run.stdout);
}

/// A document is one document or nothing; a loop emits one per round. Refused
/// before anything is read, so the trace is empty.
fn follow_with_json_is_refused_before_the_hold_is_read() {
    let http = ardent(Vec::new());
    let run = drive_with_paused_time(
        &["sell", "--from", "Sol", "--follow", "60", "--json"],
        &http,
        env(),
        journal(),
    );
    run.assert_exit(1);
    assert!(run.stderr.contains("--follow cannot be combined with --json"), "{}", run.stderr);
    assert!(run.calls.is_empty(), "{:?}", run.calls);
}
