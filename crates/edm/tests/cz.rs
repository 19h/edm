//! `cz` — combat zones from a starsystem payload, including a radius search.

mod support;

use std::path::PathBuf;

use support::{FakeHttp, drive, drive_with_env_and_files, json_reply, sealed};

const SYSTEM: &str = r#"{
  "systemName":"Test","systemAddress":42,"systemX":1,"systemY":2,"systemZ":3
}"#;

const STARSYSTEM: &str = r#"{
  "starsystem": {
    "starsystem": {
      "name": "Test",
      "minorfaction_state": "war",
      "minorFactions": {
        "4099303016811": {"id": 4099303016811, "name": "Scori Alliance"},
        "180145679199357291": {"id": 180145679199357291, "name": "East India Company"}
      }
    },
    "sites": {
      "2": {
        "bodysiteId": 3282400282,
        "scriptName": "Warzone_PointRace_Low_01",
        "distFromSystem": 440449.29453,
        "scriptParameters": {
          "PrimaryFactionID": 4099303016811,
          "SecondaryFactionID": 180145679199357291
        },
        "tags": {"0": "Low"}
      },
      "3": {
        "bodysiteId": 3282400283,
        "scriptName": "Warzone_PointRace_High_01",
        "distFromSystem": 2000,
        "scriptParameters": {
          "PrimaryFactionID": 4099303016811,
          "SecondaryFactionID": 180145679199357291
        },
        "tags": {"0": "High"}
      },
      "4": {
        "id": 3872061696,
        "name": "Mordovski Tourism Lodge +",
        "poiType": "onFootSettlement",
        "bodysiteId": 99,
        "scriptName": "Warzone_Settlement",
        "distFromSystem": 2063.75,
        "scriptParameters": {
          "PrimaryFactionID": 4099303016811,
          "SecondaryFactionID": 180145679199357291
        },
        "tags": {"0": "Hard", "1": "Low"}
      }
    }
  }
}"#;

const EMPTY_SYSTEM: &str = r#"{"starsystem":{"starsystem":{"name":"Neighbor"}}}"#;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the descriptor-capture harness requires every scenario in this binary to share one test"
)]
fn cz_lists_space_zones_and_searches_a_radius() {
    let http = FakeHttp::default()
        .route("/v2/system/name/Test", vec![json_reply(SYSTEM)])
        .route("/2.0/elite/starsystem", vec![sealed(STARSYSTEM)]);
    let run = drive(&["cz", "--system", "Test"], &http);
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        [
            "GET https://api.ardent-insight.com/v2/system/name/Test",
            "GET https://api.orerve.net/2.0/elite/starsystem",
        ]
    );
    assert!(run.stdout.contains("COMBAT ZONES"), "{}", run.stdout);
    assert!(run.stdout.contains("High"), "{}", run.stdout);
    assert!(run.stdout.contains("Low"), "{}", run.stdout);
    assert!(run.stdout.contains("Scori Alliance"), "{}", run.stdout);
    assert!(run.stdout.contains("East India"), "{}", run.stdout);
    assert!(
        run.stdout.find("High").unwrap() < run.stdout.find("Low").unwrap(),
        "high intensity sorts first:\n{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("settlement"),
        "settlements are opt-in:\n{}",
        run.stdout
    );

    let json_http = FakeHttp::default()
        .route("/v2/system/name/Test", vec![json_reply(SYSTEM)])
        .route("/2.0/elite/starsystem", vec![sealed(STARSYSTEM)]);
    let run = drive(
        &["cz", "--system", "Test", "--json", "--settlements", "--detail"],
        &json_http,
    );
    run.assert_exit(0);
    let document: serde_json::Value = serde_json::from_str(&run.stdout).expect("one JSON document");
    assert_eq!(document["summary"]["systems"], 1);
    assert_eq!(document["summary"]["zones"], 3);
    assert_eq!(document["settlements"], true);
    assert_eq!(document["systems"][0]["system"]["distanceLy"], 0);
    let intensities: Vec<&str> = document["systems"][0]["zones"]
        .as_array()
        .unwrap()
        .iter()
        .map(|zone| zone["intensity"].as_str().unwrap())
        .collect();
    assert_eq!(intensities, ["Low", "High", "Low"]);
    assert!(
        document["systems"][0]["zones"]
            .as_array()
            .unwrap()
            .iter()
            .any(|zone| {
                zone["kind"] == "settlement"
                    && zone["difficulty"] == "Hard"
                    && zone["name"] == "Mordovski Tourism Lodge"
            }),
        "{document}"
    );

    let named_http = FakeHttp::default()
        .route("/v2/system/name/Test", vec![json_reply(SYSTEM)])
        .route("/2.0/elite/starsystem", vec![sealed(STARSYSTEM)]);
    let run = drive(
        &["cz", "--system", "Test", "--settlements"],
        &named_http,
    );
    run.assert_exit(0);
    assert!(
        run.stdout.contains("Mordovski Tourism Lodge"),
        "settlement names belong in the table:\n{}",
        run.stdout
    );
    assert!(
        !run.stdout.contains("Mordovski Tourism Lodge +"),
        "payload + markers are not the settlement name:\n{}",
        run.stdout
    );

    let wrong_http = FakeHttp::default()
        .route("/v2/system/name/Test", vec![json_reply(SYSTEM)])
        .route(
            "/2.0/elite/starsystem",
            vec![sealed(r#"{"errors":["not a system"]}"#)],
        );
    let run = drive(&["cz", "--system", "Test", "--json"], &wrong_http);
    run.assert_exit(1);
    let document: serde_json::Value =
        serde_json::from_str(&run.stdout).expect("failure remains one JSON document");
    assert_eq!(document["summary"]["failed"], 1);
    assert!(run.stderr.contains("without starsystem data"));

    let radius_http = FakeHttp::default()
        .route("/v2/system/name/Test", vec![json_reply(SYSTEM)])
        .route(
            "/v2/system/name/Test/nearby?maxDistance=10",
            vec![json_reply(
                r#"[{"systemName":"Neighbor","systemAddress":43,
                     "systemX":4.25,"systemY":2,"systemZ":3,"distance":3}]"#,
            )],
        )
        .route(
            "/2.0/elite/starsystem",
            vec![sealed(STARSYSTEM), sealed(EMPTY_SYSTEM)],
        );
    let run = drive(
        &["cz", "--system", "Test", "--radius", "10", "--json"],
        &radius_http,
    );
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        [
            "GET https://api.ardent-insight.com/v2/system/name/Test",
            "GET https://api.ardent-insight.com/v2/system/name/Test/nearby",
            "GET https://api.orerve.net/2.0/elite/starsystem",
            "GET https://api.orerve.net/2.0/elite/starsystem",
        ],
        "--radius enumerates the centre and nearby systems"
    );
    let document: serde_json::Value =
        serde_json::from_str(&run.stdout).expect("radius output is one JSON document");
    assert_eq!(document["radiusLy"], 10);
    assert_eq!(document["summary"]["systems"], 2);
    assert_eq!(document["systems"][0]["system"]["name"], "Test");
    assert_eq!(document["systems"][1]["system"]["name"], "Neighbor");
    assert_eq!(document["systems"][1]["system"]["distanceLy"], 3.25);
    assert_eq!(document["summary"]["zones"], 2);

    let ceiling_http = FakeHttp::default()
        .route("/v2/system/name/Test", vec![json_reply(SYSTEM)])
        .route(
            "/v2/system/name/Test/nearby?maxDistance=10",
            vec![json_reply(
                r#"[{"systemName":"Neighbor","systemAddress":43,
                     "systemX":4.25,"systemY":2,"systemZ":3,"distance":3}]"#,
            )],
        );
    let run = drive(
        &["cz", "--system", "Test", "--radius", "10", "--max-requests", "1"],
        &ceiling_http,
    );
    run.assert_exit(1);
    assert!(run.stderr.contains("request count (2)"), "{}", run.stderr);
    assert!(run.stderr.contains("Nothing has been sent"), "{}", run.stderr);
    assert!(
        !run.calls.iter().any(|call| call.contains("starsystem")),
        "the ceiling permits only Ardent discovery:\n{:?}",
        run.calls
    );

    let dry_http = FakeHttp::default().route("/v2/system/name/Test", vec![json_reply(SYSTEM)]);
    let run = drive(&["cz", "--system", "Test", "--dry-run"], &dry_http);
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        ["GET https://api.ardent-insight.com/v2/system/name/Test"],
        "single-system dry-run still prints the Frontier request without sending it"
    );
    assert!(run.stdout.contains("REQUEST") || run.stdout.contains("starsystem"));

    let run = drive(&["cz"], &FakeHttp::default());
    run.assert_exit(1);
    assert!(
        run.stderr
            .contains("could not determine the current system")
    );
    assert!(run.calls.is_empty());

    let local_http = FakeHttp::default()
        .route("/v2/system/name/Test", vec![json_reply(SYSTEM)])
        .route("/2.0/elite/starsystem", vec![sealed(STARSYSTEM)]);
    let run = drive_with_env_and_files(
        &["cz"],
        &local_http,
        vec![("EDM_JOURNAL_DIR".to_owned(), "/journals".to_owned())],
        vec![
            (PathBuf::from("/journals"), String::new()),
            (
                PathBuf::from("/journals/Journal.2026-08-07T010101.01.log"),
                concat!(
                    r#"{"timestamp":"2026-08-07T01:01:01Z","event":"LoadGame","Credits":1000}"#,
                    "\n",
                    r#"{"timestamp":"2026-08-07T01:02:00Z","event":"Location","StarSystem":"Test","SystemAddress":42,"StarPos":[1,2,3]}"#,
                    "\n",
                )
                .to_owned(),
            ),
        ],
    );
    run.assert_exit(0);
    assert_eq!(
        run.calls,
        [
            "GET https://api.ardent-insight.com/v2/system/name/Test",
            "GET https://api.orerve.net/2.0/elite/starsystem",
        ],
        "no target uses the journal's current system"
    );

    let run = drive(&["cz", "Sol", "--radius", "501"], &FakeHttp::default());
    run.assert_exit(1);
    assert!(run.stderr.contains("--radius must be at most 500"));
    assert!(run.calls.is_empty());

    let run = drive(&["cz", "--help"], &FakeHttp::default());
    run.assert_exit(0);
    assert!(run.stdout.contains("edm cz — list combat zones near a system"));
    assert!(run.stdout.contains("--radius <ly>"));
    assert!(run.stdout.contains("--settlements"));
    assert!(run.calls.is_empty());
}
