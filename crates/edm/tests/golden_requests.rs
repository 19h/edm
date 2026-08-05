//! The request-building path, against ciphertext produced by the original.
//!
//! This is the sharpest offline check the port has. With `--nonce`, `--f-time`
//! and `--request-time` pinned, the encrypted query is a pure function of the
//! envelope — so a single wrong byte anywhere upstream (a field out of order, a
//! number stringified the Rust way, a nonce taken as six bytes instead of
//! twelve characters, a base64 alphabet) changes three kilobytes of output and
//! cannot hide.
//!
//! Regenerate with `bun xtask/oracle/bless-requests.ts crates/edm/tests/fixtures`.

use edm_core::consts::{MARKET_TRADE, STARSYSTEM};
use edm_core::domain::trade::{Kind, TradePlan};
use edm_core::wire::Nonce;
use edm::capi::{self, Credentials, HeaderConfig, Stamp};

fn credentials() -> Credentials {
    Credentials::load("F1234567", "machine-1", &"m".repeat(80), &"a".repeat(2024))
        .expect("the fixture's credentials are well formed")
}

fn stamp() -> Stamp {
    Stamp {
        nonce: Nonce::parse_arg("0123456789ab").expect("twelve hex characters"),
        frontier_time: 1_700_000_000.0,
        request_time: 12345,
    }
}

fn trade(
    market_id: &str,
    kind: Kind,
    commodity_id: f64,
    qty: f64,
    final_qty: f64,
    unit_price: f64,
    stolen: bool,
) -> TradePlan {
    TradePlan {
        market_id: market_id.to_owned(),
        kind,
        commodity_id,
        commodity_name: String::new(),
        black_market: stolen,
        stolen,
        unit_price,
        qty,
        final_qty,
    }
}

/// Builds the request each fixture row describes.
fn build(name: &str) -> capi::PreparedRequest {
    let credentials = credentials();
    let time = 1_700_000_000.0;

    match name {
        "trade_buy" => capi::prepare(
            MARKET_TRADE,
            None,
            capi::trade_fields(
                &trade("4306502403", Kind::Buy, 128_049_204.0, 7.0, 7.0, 517.0, false),
                &credentials,
                time,
            ),
            stamp(),
            &HeaderConfig::default(),
        ),
        "trade_sell_stolen" => capi::prepare(
            MARKET_TRADE,
            None,
            capi::trade_fields(
                &trade("128667761", Kind::Sell, 128_049_152.0, 13.0, 130.0, 3340.0, true),
                &credentials,
                time,
            ),
            stamp(),
            &HeaderConfig::default(),
        ),
        // R53: `trade` never parses `--market-id`, so the leading zeros are on
        // the wire. A port that reached for a `u64` here would send
        // `4306502403` and the ciphertext would not match.
        "trade_leading_zero_market" => capi::prepare(
            MARKET_TRADE,
            None,
            capi::trade_fields(
                &trade("0004306502403", Kind::Buy, 1.0, 1.0, 1.0, 1.0, false),
                &credentials,
                time,
            ),
            stamp(),
            &HeaderConfig::default(),
        ),
        "markets_by_address" => capi::prepare(
            STARSYSTEM,
            None,
            capi::starsystem_fields(5_378_909_424_384.0, "en", 0.0, &credentials, time),
            stamp(),
            &HeaderConfig::default(),
        ),
        // R65: `--language` is unvalidated, so a non-ASCII value lengthens the
        // plaintext in bytes and shifts everything after it.
        "markets_language" => capi::prepare(
            STARSYSTEM,
            None,
            capi::starsystem_fields(10_477_373_803.0, "fr-Ø", 0.0, &credentials, time),
            stamp(),
            &HeaderConfig::default(),
        ),
        // R66: the verb is uppercased by the session and changes only the
        // method, never the query.
        "markets_method_override" => capi::prepare(
            STARSYSTEM,
            Some("PUT"),
            capi::starsystem_fields(10_477_373_803.0, "en", 0.0, &credentials, time),
            stamp(),
            &HeaderConfig::default(),
        ),
        other => panic!("no builder for fixture case {other}"),
    }
}

#[test]
fn every_golden_request_matches_byte_for_byte() {
    let body = include_str!("fixtures/requests.tsv");
    let mut checked = 0usize;

    for (index, line) in body.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        let (name, method, url, headers, envelope, plaintext_length) =
            (cols[0], cols[2], cols[3], cols[4], cols[5], cols[6]);

        let request = build(name);
        assert_eq!(request.method, method, "{name}: method");

        // The whole point: three kilobytes of ciphertext, or nothing.
        assert_eq!(request.url, url, "{name}: the sealed URL differs");

        // Headers as `Headers` iteration presents them — lowercased, sorted,
        // duplicates combined. R71.
        let mut ours: Vec<(String, String)> = request
            .headers
            .iter()
            .map(|(name, value)| ((*name).to_lowercase(), value.clone()))
            .collect();
        ours.sort_by(|a, b| a.0.cmp(&b.0));
        let expected: Vec<(String, String)> = {
            let parsed: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(headers).expect("headers column");
            parsed
                .into_iter()
                .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned()))
                .collect()
        };
        assert_eq!(ours, expected, "{name}: headers");

        // The envelope as the request table renders it, in wire order — which
        // is also the order it was sealed in.
        let ours: Vec<(String, String)> = request
            .fields
            .iter()
            .map(|field| (field.name.to_owned(), field.value.display()))
            .collect();
        // Parsed with our own reader, not `serde_json`: `serde_json::Map` is
        // BTreeMap-backed without `preserve_order` and would sort the keys —
        // destroying the very ordering this assertion exists to check.
        let expected: Vec<(String, String)> = {
            let parsed = edm_core::js::json::JsValue::parse(envelope).expect("envelope column");
            parsed
                .as_object()
                .expect("an object")
                .iter()
                .map(|(key, value)| (key.to_owned(), edm_core::js::json::to_js_string(value)))
                .collect()
        };
        assert_eq!(ours, expected, "{name}: envelope fields");

        assert_eq!(
            request.plaintext_bytes.to_string(),
            plaintext_length,
            "{name}: plaintext byte length"
        );

        checked += 1;
        let _ = index;
    }

    assert_eq!(checked, 6, "the fixture should carry six cases");
}
