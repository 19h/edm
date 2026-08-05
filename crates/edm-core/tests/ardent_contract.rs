//! The Ardent client, held to the TypeScript module it replaced.
//!
//! `market-request.ts` reaches Ardent by `import()`ing
//! `/models/dev/edtrade/src/ardent.ts` at runtime and duck-typing four of its
//! exports. Divergence C1 compiles those four in instead, because Rust cannot
//! import TypeScript. This test is what stops that from being a leap of faith:
//! `xtask/oracle/bless-ardent.ts` executes the *real* module and records its
//! answers, and the port must reproduce them.
//!
//! Regenerate with `bun xtask/oracle/bless-ardent.ts crates/edm-core/tests/fixtures`.

use edm_core::ardent;
use edm_core::consts::ARDENT_BASE_URL;
use edm_core::js::json::JsValue;

fn fixture() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ardent_contract.tsv");
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{e} — run `bun xtask/oracle/bless-ardent.ts`"))
}

/// Renders our `parse_system` into the shape `ardent.ts` returns, so a
/// disagreement names the field rather than the whole struct.
fn render_system(value: &JsValue) -> String {
    match ardent::parse_system(value) {
        None => "null".to_owned(),
        Some(s) => JsValue::parse(&format!(
            r#"{{"name":{},"address":{},"coords":{{"x":{},"y":{},"z":{}}}}}"#,
            JsValue::Str(s.name.into_boxed_str()).stringify_compact(),
            edm_core::js::js_number(s.address),
            edm_core::js::js_number(s.coordinates.x),
            edm_core::js::js_number(s.coordinates.y),
            edm_core::js::js_number(s.coordinates.z),
        ))
        .expect("well-formed")
        .stringify_compact(),
    }
}

fn render_matches(value: &JsValue) -> String {
    let rendered: Vec<String> = ardent::parse_station_matches(value)
        .into_iter()
        .map(|m| {
            format!(
                r#"{{"stationName":{},"systemName":{},"stationType":{},"pad":{}}}"#,
                JsValue::Str(m.station_name.into_boxed_str()).stringify_compact(),
                JsValue::Str(m.system_name.into_boxed_str()).stringify_compact(),
                m.station_type.map_or_else(
                    || "null".to_owned(),
                    |t| JsValue::Str(t.into_boxed_str()).stringify_compact()
                ),
                m.pad.map_or_else(|| "null".to_owned(), edm_core::js::js_number),
            )
        })
        .collect();
    format!("[{}]", rendered.join(","))
}

#[test]
fn the_ported_client_matches_the_module_it_replaced() {
    let body = fixture();
    let mut mismatches: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for (index, line) in body.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let line_no = index + 1;

        let (label, actual, expected) = match cols[0] {
            "BASE_URL" => ("BASE_URL", ARDENT_BASE_URL.to_owned(), cols[1].to_owned()),
            kind @ ("systemUrl" | "stationSearchUrl") => {
                let name: String =
                    serde_json::from_str(cols[1]).expect("the input column is a JSON string");
                let actual = if kind == "systemUrl" {
                    ardent::system_url(ARDENT_BASE_URL, &name)
                } else {
                    ardent::station_search_url(ARDENT_BASE_URL, &name)
                };
                (kind, actual, cols[2].to_owned())
            }
            kind @ ("parseSystem" | "parseStationMatches") => {
                let input = JsValue::parse(cols[1]).expect("the input column is JSON");
                let actual = if kind == "parseSystem" {
                    render_system(&input)
                } else {
                    render_matches(&input)
                };
                (kind, actual, cols[2].to_owned())
            }
            other => panic!("line {line_no}: unknown fixture kind {other}"),
        };

        if actual != expected && mismatches.len() < 12 {
            mismatches.push(format!(
                "  line {line_no} [{label}] {}\n    ts:   {expected}\n    rust: {actual}",
                cols.get(1).copied().unwrap_or("")
            ));
        }
        checked += 1;
    }

    assert!(checked > 50, "the fixture looks truncated: only {checked} rows");
    assert!(mismatches.is_empty(), "{checked} rows checked:\n{}", mismatches.join("\n"));
}
