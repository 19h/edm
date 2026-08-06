//! The ID64 system-address codec, measured against the original algorithm.
//!
//! The fixtures are produced by executing `decodeSystemAddress`,
//! `encodeSystemAddress` and `containsCoordinates` **sliced verbatim out of
//! `game-internal-api.ts`** — not by re-transcribing them. A fixture therefore
//! cannot agree with a mistake that the generator and this implementation
//! happened to share.

use edm_core::domain::id64::{self, Coordinates};
use edm_core::js;
use serde::Deserialize;

fn fixture(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    std::fs::read_to_string(format!("{path}{name}"))
        .unwrap_or_else(|e| panic!("{name}: {e} — run `bun xtask/oracle/bless-id64.ts`"))
}

fn rows(body: &str) -> impl Iterator<Item = (usize, Vec<&str>)> {
    body.lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .map(|(i, line)| (i + 1, line.split('\t').collect()))
}

#[derive(Deserialize)]
struct Coord {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Parts {
    mass_code: u32,
    mass_code_letter: String,
    boxel_size: f64,
    sector: Coord,
    boxel: Coord,
    index: f64,
    origin: Coord,
}

/// Renders our own decode into the same shape the fixture holds, so a
/// disagreement points at the field rather than at the whole struct.
fn render(parts: &id64::AddressParts) -> String {
    let c = |v: &Coordinates| {
        format!("{}/{}/{}", js::js_number(v.x), js::js_number(v.y), js::js_number(v.z))
    };
    format!(
        "mass={} letter={} size={} sector={} boxel={} index={} origin={}",
        parts.mass_code,
        parts.mass_code_letter,
        js::js_number(parts.boxel_size),
        c(&parts.sector),
        c(&parts.boxel),
        js::js_number(parts.index),
        c(&parts.origin),
    )
}

fn render_fixture(p: &Parts) -> String {
    let c = |v: &Coord| format!("{}/{}/{}", js::js_number(v.x), js::js_number(v.y), js::js_number(v.z));
    format!(
        "mass={} letter={} size={} sector={} boxel={} index={} origin={}",
        p.mass_code,
        p.mass_code_letter,
        js::js_number(p.boxel_size),
        c(&p.sector),
        c(&p.boxel),
        js::js_number(p.index),
        c(&p.origin),
    )
}

#[test]
fn decode_matches_the_original() {
    let body = fixture("id64_decode.tsv");
    let mut mismatches = Vec::new();
    let mut checked = 0;

    for (line, cols) in rows(&body) {
        let address = js::to_number(cols[0]);
        let expected = cols[1];

        let actual = match id64::decode(address) {
            Ok(parts) => render(&parts),
            Err(message) => format!("ERR:{message}"),
        };
        let expected = if let Some(message) = expected.strip_prefix("ERR:") {
            format!("ERR:{message}")
        } else {
            render_fixture(&serde_json::from_str::<Parts>(expected).expect("parts JSON"))
        };

        if actual != expected && mismatches.len() < 10 {
            mismatches.push(format!(
                "  line {line}: address {}\n    ts:   {expected}\n    rust: {actual}",
                cols[0]
            ));
        }
        checked += 1;
    }

    assert!(mismatches.is_empty(), "{checked} rows checked:\n{}", mismatches.join("\n"));
}

#[test]
fn encode_and_containment_match_the_original() {
    let body = fixture("id64_encode.tsv");
    let mut mismatches = Vec::new();
    let mut checked = 0;

    for (line, cols) in rows(&body) {
        let coordinates = Coordinates {
            x: js::to_number(cols[0]),
            y: js::to_number(cols[1]),
            z: js::to_number(cols[2]),
        };
        let mass_code = js::to_number(cols[3]);
        let index = js::to_number(cols[4]);

        let actual = match id64::encode(coordinates, mass_code, index) {
            Ok(address) => js::js_number(address),
            Err(message) => format!("ERR:{message}"),
        };
        if actual != cols[5] && mismatches.len() < 10 {
            mismatches.push(format!(
                "  line {line}: encode({}, {}, {}, {})\n    ts:   {}\n    rust: {actual}",
                cols[0], cols[1], cols[2], cols[3], cols[5]
            ));
        }

        // Containment is only meaningful where the address decoded.
        if let Ok(address) = id64::encode(coordinates, mass_code, index)
            && let Ok(parts) = id64::decode(address)
        {
            let inside = id64::contains(&parts, coordinates);
            let want = cols[6] == "true";
            if inside != want && mismatches.len() < 10 {
                mismatches.push(format!(
                    "  line {line}: contains disagrees — ts {want}, rust {inside}"
                ));
            }
        }
        checked += 1;
    }

    assert!(mismatches.is_empty(), "{checked} rows checked:\n{}", mismatches.join("\n"));
}

/// The documented anchor, stated outright: `systemAddr=5378909424384` is
/// *Hyades Sector NI-X a16-0*, which is what the bit layout in the module docs
/// was derived from.
#[test]
fn hyades_sector_anchor() {
    let parts = id64::decode(5_378_909_424_384.0).expect("a valid address");
    assert_eq!(parts.mass_code, 0);
    assert_eq!(parts.mass_code_letter, 'a');
    assert_eq!(parts.boxel_size, 10.0);
    assert_eq!(parts.index, 0.0);
    assert_eq!((parts.sector.x, parts.sector.y, parts.sector.z), (39.0, 31.0, 18.0));
    assert_eq!((parts.boxel.x, parts.boxel.y, parts.boxel.z), (17.0, 126.0, 96.0));
    assert_eq!((parts.origin.x, parts.origin.y, parts.origin.z), (105.0, -45.0, -105.0));
}

/// Every address that decodes must re-encode to itself from its own boxel
/// origin. This is the check `runMarkets` prints as "repacked address".
#[test]
fn round_trip_from_the_boxel_origin() {
    let mut checked = 0;
    // A deterministic sweep across the whole safe-integer range and every mass
    // code, rather than a proptest, so failures are reproducible by address.
    let mut state = 0x243f_6a88_85a3_08d3u64;
    for _ in 0..20_000 {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        let address = ((state >> 11) as f64).floor();
        if !js::safe_int(address) {
            continue;
        }
        let parts = id64::decode(address).expect("non-negative safe integer");
        let packed = id64::encode(parts.origin, f64::from(parts.mass_code), parts.index)
            .expect("an address decodes into coordinates that re-encode");
        assert_eq!(
            packed, address,
            "address {} did not round-trip (mass code {})",
            js::js_number(address),
            parts.mass_code
        );
        assert!(id64::contains(&parts, parts.origin), "the origin is inside its own boxel");
        checked += 1;
    }
    assert!(checked > 15_000, "expected most samples to be usable, got {checked}");
}
