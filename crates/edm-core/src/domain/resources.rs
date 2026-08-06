#![allow(
    clippy::disallowed_types,
    reason = "numeric commodity catalog needs deterministic ID order; this is not JSON object ordering"
)]

//! Narrow, typed readers for the Frontier finance and commodity resources.
//!
//! These payloads are not general configuration documents.  In particular,
//! the finance resource is a heterogeneous object with hundreds of unrelated
//! fields.  The readers below deliberately recognise only the fields used by
//! market-data acquisition and reject lossy numeric IDs rather than pretending
//! that an already-rounded `f64` is exact.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Deref;

use crate::js::json::{JsObject, JsValue};

/// Conservative values used when the finance resource is missing or malformed.
pub const DEFAULT_SYSTEM_MARKET_CACHE_SECONDS: u64 = 7_200;
pub const DEFAULT_MAX_MARKETDATA_DISTANCE_LY: f64 = 40.0;
pub const DEFAULT_SYSTEMS_PER_REQUEST: usize = 5;

/// A hard client-side cap, independent of what a resource or server accepts.
pub const MARKETDATA_BATCH_MAX: usize = 5;

const MAX_EXACT_F64_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Counts recoverable schema violations.
///
/// A parser returns the useful rows beside these counts.  One bad commodity or
/// market must not erase the sound rows around it, but it must also not vanish
/// without an observable trace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParseCounts {
    /// A value had the wrong JSON type, was missing, or was not an exact value.
    pub malformed: usize,
    /// Two input rows resolved to the same numeric ID.
    pub duplicates: usize,
    /// An embedded ID disagreed with its authoritative object key.
    pub id_mismatches: usize,
    /// A closed discriminator (for example a market side) was unknown.
    pub unknown_kinds: usize,
    /// Fields were individually well-typed but described an unsafe meaning.
    pub invalid_semantics: usize,
}

impl ParseCounts {
    /// Total anomalies counted by this parser.
    #[must_use]
    pub const fn total(self) -> usize {
        self.malformed
            + self.duplicates
            + self.id_mismatches
            + self.unknown_kinds
            + self.invalid_semantics
    }
}

/// A useful value accompanied by all recoverable parse anomalies.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed<T> {
    pub value: T,
    pub counts: ParseCounts,
}

impl<T> Parsed<T> {
    #[must_use]
    pub fn into_value(self) -> T {
        self.value
    }
}

impl<T> Deref for Parsed<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

/// The three finance tunables that govern bulk market-data reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FinanceRules {
    pub system_market_cache_seconds: u64,
    pub max_marketdata_distance_ly: f64,
    pub systems_per_request: usize,
}

impl Default for FinanceRules {
    fn default() -> Self {
        Self {
            system_market_cache_seconds: DEFAULT_SYSTEM_MARKET_CACHE_SECONDS,
            max_marketdata_distance_ly: DEFAULT_MAX_MARKETDATA_DISTANCE_LY,
            systems_per_request: DEFAULT_SYSTEMS_PER_REQUEST,
        }
    }
}

/// Parse only the three relevant string fields of `/resources/finance`.
///
/// Every setting is independently optional: a bad live tunable falls back to a
/// conservative value without discarding the other two.  Even a valid larger
/// `StarSystemsPerRequest` is capped at five; probes show that larger batches
/// are accepted by the server, but five is the observed client policy.
#[must_use]
pub fn parse_finance(document: &JsValue) -> Parsed<FinanceRules> {
    let mut rules = FinanceRules::default();
    let mut counts = ParseCounts::default();
    let Some(object) = document.as_record() else {
        counts.malformed += 1;
        return Parsed {
            value: rules,
            counts,
        };
    };

    if let Some(value) = object.get("SystemMarketCacheTime") {
        match value.as_str().and_then(positive_decimal_u64) {
            Some(seconds) => rules.system_market_cache_seconds = seconds,
            None => counts.malformed += 1,
        }
    }

    if let Some(value) = object.get("MaxMarketDataDistance") {
        match value.as_str().and_then(positive_finite) {
            Some(distance) => rules.max_marketdata_distance_ly = distance,
            None => counts.malformed += 1,
        }
    }

    if let Some(value) = object.get("StarSystemsPerRequest") {
        match value.as_str().and_then(positive_decimal_u64) {
            Some(batch) => {
                let batch = usize::try_from(batch).unwrap_or(usize::MAX);
                rules.systems_per_request = batch.min(MARKETDATA_BATCH_MAX);
            }
            None => counts.malformed += 1,
        }
    }

    Parsed {
        value: rules,
        counts,
    }
}

fn positive_finite(text: &str) -> Option<f64> {
    // `str::parse` accepts `NaN` and infinities, so the checks are not
    // decorative.  Whitespace is intentionally not trimmed: the resource's
    // values are decimal strings, not user input.
    if text.is_empty() || text.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let value = text.parse::<f64>().ok()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

fn positive_decimal_u64(text: &str) -> Option<u64> {
    let value = decimal_u64(text)?;
    (value > 0).then_some(value)
}

/// Parse an exact, canonical unsigned decimal string.
///
/// Object keys and string IDs take this path and therefore retain every digit
/// above 2^53.  Numeric JSON values cannot make the same promise and use
/// [`exact_u64`] instead.
#[must_use]
pub fn decimal_u64(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.is_empty()
        || !bytes.iter().all(u8::is_ascii_digit)
        || (bytes.len() > 1 && bytes[0] == b'0')
    {
        return None;
    }
    text.parse().ok()
}

/// Read an exact ID from a string or a safely representable JSON number.
#[must_use]
pub fn exact_u64(value: &JsValue) -> Option<u64> {
    match value {
        JsValue::Str(text) => decimal_u64(text),
        JsValue::Num(number)
            if number.is_finite()
                && *number >= 0.0
                && *number <= MAX_EXACT_F64_INTEGER
                && number.fract() == 0.0 =>
        {
            Some(*number as u64)
        }
        _ => None,
    }
}

/// Read an exact signed integer from either of the resource's wire encodings.
#[must_use]
pub(crate) fn exact_i64(value: &JsValue) -> Option<i64> {
    match value {
        JsValue::Str(text) => decimal_i64(text),
        JsValue::Num(number)
            if number.is_finite()
                && number.abs() <= MAX_EXACT_F64_INTEGER
                && number.fract() == 0.0 =>
        {
            Some(*number as i64)
        }
        _ => None,
    }
}

fn decimal_i64(text: &str) -> Option<i64> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, text),
    };
    if digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || (digits.len() > 1 && digits.as_bytes()[0] == b'0')
        || (negative && digits == "0")
    {
        return None;
    }
    text.parse().ok()
}

/// One row of `/resources/commodities`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceCommodity {
    pub id: u64,
    pub name: String,
    pub category: String,
    pub mean_price: Option<i64>,
    pub created_anywhere: bool,
    pub display: i64,
    pub carrier_display: i64,
}

impl ResourceCommodity {
    /// A stable identity for an ID absent from the resource catalog.
    ///
    /// Bulk market rows are authoritative about the numeric ID even when the
    /// auxiliary catalog is old.  Returning a placeholder retains that row
    /// instead of silently dropping a potentially tradable commodity.
    #[must_use]
    pub fn unknown(id: u64) -> Self {
        Self {
            id,
            name: format!("commodity:{id}"),
            category: "Unknown".to_owned(),
            mean_price: None,
            created_anywhere: false,
            display: 0,
            carrier_display: 0,
        }
    }
}

/// Counts for a commodity array, including rows that parsed successfully.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatalogCounts {
    pub seen: usize,
    pub accepted: usize,
    pub rejected: ParseCounts,
}

/// Commodity metadata keyed and iterated by exact numeric ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommodityCatalog {
    pub commodities: BTreeMap<u64, ResourceCommodity>,
    pub counts: CatalogCounts,
}

impl CommodityCatalog {
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&ResourceCommodity> {
        self.commodities.get(&id)
    }

    /// Resolve an ID without losing unknown-but-valid market rows.
    #[must_use]
    pub fn get_or_unknown(&self, id: u64) -> Cow<'_, ResourceCommodity> {
        self.get(id)
            .map_or_else(|| Cow::Owned(ResourceCommodity::unknown(id)), Cow::Borrowed)
    }
}

/// Parse `/resources/commodities`, retaining valid rows around bad ones.
#[must_use]
pub fn parse_commodities(document: &JsValue) -> CommodityCatalog {
    let mut catalog = CommodityCatalog::default();
    let Some(rows) = document
        .as_record()
        .and_then(|object| object.get("commodities"))
        .and_then(JsValue::as_array)
    else {
        catalog.counts.rejected.malformed += 1;
        return catalog;
    };

    for value in rows {
        catalog.counts.seen += 1;
        let Some(row) = value.as_record() else {
            catalog.counts.rejected.malformed += 1;
            continue;
        };
        let Some(commodity) = parse_resource_commodity(row, &mut catalog.counts.rejected) else {
            continue;
        };

        if catalog.commodities.contains_key(&commodity.id) {
            // First valid row wins.  That rule is deterministic even when two
            // conflicting duplicate rows occur in a document.
            catalog.counts.rejected.duplicates += 1;
            continue;
        }
        catalog.counts.accepted += 1;
        catalog.commodities.insert(commodity.id, commodity);
    }

    catalog
}

/// An explicit name for callers that treat the resource as a catalog.
#[must_use]
pub fn parse_commodity_catalog(document: &JsValue) -> CommodityCatalog {
    parse_commodities(document)
}

fn parse_resource_commodity(row: &JsObject, counts: &mut ParseCounts) -> Option<ResourceCommodity> {
    let id = required(row, "id", exact_u64, counts)?;
    if id == 0 {
        counts.invalid_semantics += 1;
        return None;
    }

    let name = required_string(row, "name", counts)?;
    let category = required_string(row, "categoryname", counts)?;
    if name.is_empty() || category.is_empty() {
        counts.malformed += 1;
        return None;
    }

    let Some(JsValue::Bool(created_anywhere)) = row.get("createdAnywhere") else {
        counts.malformed += 1;
        return None;
    };
    let created_anywhere = *created_anywhere;
    let display = required(row, "display", exact_i64, counts)?;
    let carrier_display = required(row, "carrierdisplay", exact_i64, counts)?;

    // `cost_mean` is the catalog's one deliberately optional interpretation:
    // a bad advisory mean must not discard otherwise usable identity metadata.
    let mean_price = match row.get("cost_mean") {
        Some(JsValue::Str(text)) => match decimal_i64(text) {
            Some(value) if value >= 0 => Some(value),
            _ => {
                counts.malformed += 1;
                None
            }
        },
        Some(JsValue::Null) | None => None,
        Some(_) => {
            counts.malformed += 1;
            None
        }
    };

    Some(ResourceCommodity {
        id,
        name: name.to_owned(),
        category: category.to_owned(),
        mean_price,
        created_anywhere,
        display,
        carrier_display,
    })
}

fn required<'a, T>(
    row: &'a JsObject,
    key: &str,
    parser: impl FnOnce(&'a JsValue) -> Option<T>,
    counts: &mut ParseCounts,
) -> Option<T> {
    if let Some(value) = row.get(key).and_then(parser) {
        Some(value)
    } else {
        counts.malformed += 1;
        None
    }
}

fn required_string<'a>(row: &'a JsObject, key: &str, counts: &mut ParseCounts) -> Option<&'a str> {
    required(row, key, JsValue::as_str, counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(source: &str) -> JsValue {
        JsValue::parse(source).expect("fixture is JSON")
    }

    #[test]
    fn finance_is_narrow_capped_and_independently_defaulted() {
        let parsed = parse_finance(&json(
            r#"{
                "SystemMarketCacheTime":"3600",
                "MaxMarketDataDistance":"25.5",
                "StarSystemsPerRequest":"99",
                "unrelated":{"nested":true},
                "alsoUnrelated":123
            }"#,
        ));
        assert_eq!(parsed.system_market_cache_seconds, 3_600);
        assert_eq!(parsed.max_marketdata_distance_ly, 25.5);
        assert_eq!(
            parsed.systems_per_request, 5,
            "the hard cap is not a server limit"
        );
        assert_eq!(parsed.counts, ParseCounts::default());

        let bad = parse_finance(&json(
            r#"{
                "SystemMarketCacheTime":7200,
                "MaxMarketDataDistance":"NaN",
                "StarSystemsPerRequest":"0"
            }"#,
        ));
        assert_eq!(bad.value, FinanceRules::default());
        assert_eq!(bad.counts.malformed, 3);
    }

    #[test]
    fn a_non_object_finance_document_is_always_safe() {
        let parsed = parse_finance(&json("[]"));
        assert_eq!(parsed.value, FinanceRules::default());
        assert_eq!(parsed.counts.malformed, 1);
    }

    #[test]
    fn exact_ids_prefer_strings_and_reject_lossy_numbers() {
        assert_eq!(
            exact_u64(&JsValue::Str("9007199254740993".into())),
            Some(9_007_199_254_740_993)
        );
        assert_eq!(decimal_u64("18446744073709551615"), Some(u64::MAX));
        assert_eq!(decimal_u64("01"), None);
        assert_eq!(
            exact_u64(&JsValue::Num(9_007_199_254_740_992.0)),
            None,
            "the source decimal is unknowable once an f64 crosses 2^53"
        );
    }

    #[test]
    fn commodity_rows_are_sorted_counted_and_deduplicated() {
        let catalog = parse_commodities(&json(
            r#"{
                "commodities":[
                    {"id":20,"name":"Gold","categoryname":"Metals","cost_mean":"50000","createdAnywhere":true,"display":1,"carrierdisplay":1},
                    {"id":10,"name":"Water","categoryname":"Chemicals","cost_mean":"120","createdAnywhere":false,"display":1,"carrierdisplay":0},
                    {"id":20,"name":"duplicate","categoryname":"Other","cost_mean":"1","createdAnywhere":false,"display":0,"carrierdisplay":0},
                    {"id":"9007199254740993","name":"Future","categoryname":"Technology","cost_mean":"bad","createdAnywhere":true,"display":2,"carrierdisplay":2},
                    {"id":30,"name":"broken"},
                    null
                ]
            }"#,
        ));

        assert_eq!(
            catalog.commodities.keys().copied().collect::<Vec<_>>(),
            vec![10, 20, 9_007_199_254_740_993]
        );
        assert_eq!(catalog.counts.seen, 6);
        assert_eq!(catalog.counts.accepted, 3);
        assert_eq!(catalog.counts.rejected.duplicates, 1);
        assert_eq!(catalog.counts.rejected.malformed, 3);
        assert_eq!(
            catalog.get(20).unwrap().name,
            "Gold",
            "first valid duplicate wins"
        );
        assert_eq!(catalog.get(9_007_199_254_740_993).unwrap().mean_price, None);
    }

    #[test]
    fn unknown_catalog_ids_survive_with_stable_metadata() {
        let catalog = CommodityCatalog::default();
        let unknown = catalog.get_or_unknown(128_999_999);
        assert_eq!(unknown.id, 128_999_999);
        assert_eq!(unknown.name, "commodity:128999999");
        assert_eq!(unknown.category, "Unknown");
        assert_eq!(catalog.counts, CatalogCounts::default());
    }
}
