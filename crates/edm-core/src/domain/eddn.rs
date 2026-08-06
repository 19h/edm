//! Building an EDDN `commodity/3` message.
//!
//! The schema at `/models/dev/EDDN/schemas/commodity-v3.0.json` is strict in a
//! way that punishes carelessness: `message` sets `additionalProperties: false`,
//! and `id`, `Producer` and `Rare` inside a commodity map to a `disallowed`
//! definition that matches *no JSON value at all* — so including any of them is
//! a hard validation failure rather than a warning. The row type below simply
//! has no such fields, which makes them unsendable by construction.
//!
//! Note also that every price and quantity is declared `"type": "integer"`, and
//! the gateway validates with CPython's `jsonschema` where a float is not an
//! integer. That is why the payload is serialized through
//! [`crate::js::json`] and never through `serde_json`. See F2.

use crate::consts::{EDDN_SCHEMA, EDDN_SOFTWARE_NAME, EDDN_SOFTWARE_VERSION, EDDN_GAME_VERSION};
use crate::js::json::{JsObject, JsValue};

use super::Commodity;

/// The names EDDN requires for a market, which the game-internal API's listing does
/// not carry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EddnStation {
    pub system_name: String,
    pub station_name: String,
    pub station_type: Option<String>,
    pub economies: Option<Vec<(String, f64)>>,
}

/// Everything about the sender, from flags and defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct EddnOptions {
    /// Publish against the `/test` schema, which the gateway accepts but does
    /// not relay onward.
    pub test: bool,
    pub uploader_id: String,
    pub software_name: String,
    pub software_version: String,
    pub game_version: String,
    pub game_build: String,
    /// `None` means "omit the key", which is not the same as `Some(false)`.
    pub horizons: Option<bool>,
    pub odyssey: Option<bool>,
}

impl Default for EddnOptions {
    fn default() -> Self {
        Self {
            test: false,
            uploader_id: String::new(),
            software_name: EDDN_SOFTWARE_NAME.to_owned(),
            software_version: EDDN_SOFTWARE_VERSION.to_owned(),
            game_version: EDDN_GAME_VERSION.to_owned(),
            game_build: String::new(),
            horizons: None,
            odyssey: None,
        }
    }
}

/// A built message and the number of commodity rows in it.
#[derive(Clone, Debug)]
pub struct EddnMessage {
    pub payload: JsValue,
    pub count: usize,
}

/// `eddnCommodities` (ts:2886).
///
/// A quantity as EDDN's schema requires it: an integer \[C32\].
///
/// **The game-internal API sends fractional quantities.** `Water` with a demand of
/// `113.47560000000001` is a real row from a real market; 29,370 such values
/// appear across 29,152 markets scanned on 2026-08-06, and **29.7% of markets
/// carry at least one**. The schema types `demand` and `stock` as `integer`, so
/// every one of those markets uploaded a message the gateway answered with
/// HTTP 400 and `FAIL: Schema Validation`.
///
/// The original forwards the value verbatim, and `PORTING.md` recorded that as
/// a TypeScript defect preserved rather than fixed — which was the right call
/// until it turned out to be rejecting a third of all uploads.
///
/// Truncation, not rounding, because that is what
/// `EDMarketConnector/plugins/eddn.py:624-629` does — `int(commodity['demand'])`
/// — and EDDN is a shared dataset whose value depends on senders agreeing. A
/// rounding rule of our own would put this program's rows subtly out of step
/// with every other uploader's for the same market.
fn whole(value: f64) -> JsValue {
    // `trunc` on a non-finite value is still non-finite, and `stringify` writes
    // that as `null` — which fails the schema loudly rather than silently
    // shipping a wrong number.
    JsValue::Num(value.trunc())
}

/// `commodity-README.md:48` — skip `NonMarketable` goods (limpets) and anything
/// with a non-empty legality string. Names are lowercased to the symbol form
/// EDDN indexes on: the game-internal API gives `AgronomicTreatment` and journal
/// senders give `$agronomictreatment_name;`, which both reduce to the same
/// lowercase token.
fn commodity_rows(commodities: &[Commodity<'_>]) -> Vec<JsValue> {
    commodities
        .iter()
        .filter(|c| c.category != "NonMarketable" && !c.illegal)
        .map(|c| {
            object([
                ("name", JsValue::Str(c.name.to_lowercase().into_boxed_str())),
                ("meanPrice", whole(c.mean_price)),
                ("buyPrice", whole(c.buy_price)),
                ("stock", whole(c.stock)),
                // Brackets are not quantities: the schema's `levelType` is the
                // enum `[0, 1, 2, 3, ""]`, and every value the game-internal API
                // has been observed to send is already in it — 29,152 markets
                // scanned 2026-08-06, not one outside. Truncating one would
                // turn an unexpected value into a plausible wrong one.
                ("stockBracket", JsValue::Num(c.stock_bracket)),
                ("sellPrice", whole(c.sell_price)),
                ("demand", whole(c.demand)),
                ("demandBracket", JsValue::Num(c.demand_bracket)),
            ])
        })
        .collect()
}

/// `buildEddnMessage` (ts:2904).
///
/// Key order is insertion order and is part of the output: the harness diffs
/// these bytes against the TypeScript's.
#[must_use]
pub fn build_message(
    station: &EddnStation,
    market_id: f64,
    commodities: &[Commodity<'_>],
    timestamp: &str,
    options: &EddnOptions,
) -> EddnMessage {
    let rows = commodity_rows(commodities);
    let count = rows.len();

    let mut message = vec![
        ("systemName", JsValue::Str(station.system_name.clone().into_boxed_str())),
        ("stationName", JsValue::Str(station.station_name.clone().into_boxed_str())),
        ("marketId", JsValue::Num(market_id)),
        ("timestamp", JsValue::Str(timestamp.into())),
        ("commodities", JsValue::Arr(rows)),
    ];

    if let Some(kind) = station.station_type.as_deref().filter(|k| !k.is_empty()) {
        message.push(("stationType", JsValue::Str(kind.into())));
    }
    // "You MUST NOT send empty lists" — omit the key rather than send `[]`.
    if let Some(economies) = station.economies.as_deref().filter(|e| !e.is_empty()) {
        message.push((
            "economies",
            JsValue::Arr(
                economies
                    .iter()
                    .map(|(name, proportion)| {
                        object([
                            ("name", JsValue::Str(name.clone().into_boxed_str())),
                            ("proportion", JsValue::Num(*proportion)),
                        ])
                    })
                    .collect(),
            ),
        ));
    }
    // Absent and false are different messages; only send what is known.
    if let Some(horizons) = options.horizons {
        message.push(("horizons", JsValue::Bool(horizons)));
    }
    if let Some(odyssey) = options.odyssey {
        message.push(("odyssey", JsValue::Bool(odyssey)));
    }

    let schema_ref = if options.test {
        format!("{EDDN_SCHEMA}/test")
    } else {
        EDDN_SCHEMA.to_owned()
    };

    let payload = JsValue::Obj(JsObject::from_document_order(vec![
        ("$schemaRef".into(), JsValue::Str(schema_ref.into_boxed_str())),
        (
            "header".into(),
            object([
                ("uploaderID", JsValue::Str(options.uploader_id.clone().into_boxed_str())),
                ("softwareName", JsValue::Str(options.software_name.clone().into_boxed_str())),
                (
                    "softwareVersion",
                    JsValue::Str(options.software_version.clone().into_boxed_str()),
                ),
                ("gameversion", JsValue::Str(options.game_version.clone().into_boxed_str())),
                ("gamebuild", JsValue::Str(options.game_build.clone().into_boxed_str())),
            ]),
        ),
        ("message".into(), object(message)),
    ]));

    EddnMessage { payload, count }
}

/// Builds an object from entries already in the order they should appear.
///
/// Routing through `from_document_order` rather than constructing directly is
/// deliberate: none of these keys are array indices, so the ECMAScript
/// reordering is the identity here, and going through the same door means a key
/// that ever *did* look numeric would be ordered the way JavaScript would order
/// it rather than the way it was written.
fn object<'k>(entries: impl IntoIterator<Item = (&'k str, JsValue)>) -> JsValue {
    JsValue::Obj(JsObject::from_document_order(
        entries.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    ))
}
