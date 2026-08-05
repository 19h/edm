//! `markets` — the address cross-check, the listing, and `--dump`.
//!
//! One `#[test]`; see `support/mod.rs` for why.

mod support;

use std::path::PathBuf;

use support::{ARDENT_COLONIA, FakeHttp, drive, json_reply, sealed, starsystem};

fn http() -> FakeHttp {
    FakeHttp::default()
        .route("/v2/system/name/", vec![json_reply(ARDENT_COLONIA)])
        .route("/2.0/elite/starsystem", vec![sealed(&starsystem())])
}

#[test]
fn markets_end_to_end() {
    // -- the whole listing --------------------------------------------------
    let run = drive(&["markets", "Colonia"], &http());
    run.assert_exit(0);
    assert!(run.stdout.contains("(round-trips)"), "Colonia's address must re-pack");
    insta::assert_snapshot!("listing", run.stdout);

    // `--trading` hides the market with nothing imported or exported, and says
    // so; `--carriers` restores the fleet carrier.
    let run = drive(&["markets", "Colonia", "--trading"], &http());
    run.assert_exit(0);
    assert!(run.stdout.contains("1 without a commodity market hidden by --trading"));
    let run = drive(&["markets", "Colonia", "--carriers"], &http());
    run.assert_exit(0);
    assert!(!run.stdout.contains("fleet carriers hidden"));
    assert!(run.stdout.contains("K3M-B4G"));

    // -- --dump -------------------------------------------------------------
    // Written *before* `JSON.parse`, so a payload that will not parse is still
    // dumped \[C16\].
    let run = drive(&["markets", "Colonia", "--dump", "/tmp/starsystem.json"], &http());
    run.assert_exit(0);
    assert_eq!(run.files.len(), 1);
    assert_eq!(run.files[0].0, PathBuf::from("/tmp/starsystem.json"));
    assert_eq!(run.files[0].1, starsystem());
    assert!(run.stdout.contains("of starsystem payload to /tmp/starsystem.json"));

    // C16: the `--json` branch returns before the dump, so `--json --dump f`
    // writes nothing at all.
    let run = drive(&["markets", "Colonia", "--dump", "/tmp/starsystem.json", "--json"], &http());
    run.assert_exit(0);
    assert!(run.files.is_empty());
    insta::assert_snapshot!("json", run.stdout);

    // -- --address ----------------------------------------------------------
    // R52: `--address` is read before the missing-name check, so no Ardent
    // lookup happens and the system row reads `address <id64>`.
    let run = drive(
        &["markets", "--address", "3238296097059"],
        &FakeHttp::default().route("/2.0/elite/starsystem", vec![sealed(&starsystem())]),
    );
    run.assert_exit(0);
    assert_eq!(run.calls, ["GET https://api.orerve.net/2.0/elite/starsystem"]);
    assert!(run.stdout.contains("address 3238296097059"));
    // With no coordinates there is nothing to cross-check against.
    assert!(!run.stdout.contains("repacked address"));

    // -- the structural fallback -------------------------------------------
    // A payload with no `starsystem.polities` falls back to sniffing the tree.
    let odd = FakeHttp::default().route(
        "/2.0/elite/starsystem",
        vec![sealed(r#"{"stations":[{"name":"Somewhere","marketId":42,"type":"Outpost"}]}"#)],
    );
    let run = drive(&["markets", "--address", "3238296097059"], &odd);
    run.assert_exit(0);
    assert!(run.stdout.contains("falling back to a structural scan"));
    assert!(run.stdout.contains("== POINTS OF INTEREST  1 found by scan "));

    // And a payload with nothing station-like at all says so and stops.
    let bare = FakeHttp::default().route("/2.0/elite/starsystem", vec![sealed(r#"{"a":1}"#)]);
    let run = drive(&["markets", "--address", "3238296097059"], &bare);
    run.assert_exit(0);
    assert!(run.stdout.contains("no markets found under starsystem.polities"));

    // -- a name that resolves to nothing ------------------------------------
    let run = drive(&["markets"], &FakeHttp::default());
    run.assert_exit(1);
    assert_eq!(run.stderr, "markets needs a system or station name (or --address <id64>)\n");
}
