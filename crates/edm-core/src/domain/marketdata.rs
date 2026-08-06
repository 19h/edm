//! Typed parsing for `/2.0/elite/starsystem/marketdata`.
//!
//! The endpoint publishes candidate prices and station metadata.  It does
//! **not** publish stock or demand quantities.  Consequently this model has no
//! quantity field: a producer is supply-only at `buyPrice`, and a consumer is
//! demand-only at `sellPrice`.  Callers must not turn `illegalJurisdictionQty`
//! (a small jurisdiction code) into inventory.

use std::collections::BTreeSet;

use crate::js::json::{JsObject, JsValue};

use super::resources::{ParseCounts, decimal_u64, exact_i64, exact_u64};

/// Candidate market data, in deterministic numeric-ID order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MarketData {
    pub systems: Vec<SystemMarketData>,
    pub rejected: ParseCounts,
}

/// Official metadata and candidate markets for one exact system address.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemMarketData {
    pub address: u64,
    pub name: String,
    pub has_fleet_carriers: bool,
    pub markets: Vec<BulkMarket>,
    pub tech_broker: String,
    pub material_trader: String,
    pub black_market: bool,
    pub facilitator: bool,
    pub voucher_redemption: bool,
    pub carrier_vendor: bool,
    pub module_packs: bool,
    /// Absolute Unix seconds.  Zero means missing/invalid and tells the cache
    /// layer to apply the finance fallback; it is never replaced here by 7200.
    pub cache_until_s: i64,
}

/// One market and its officially simulated candidate prices.
#[derive(Clone, Debug, PartialEq)]
pub struct BulkMarket {
    pub market_id: u64,
    pub system_name: String,
    pub name: String,
    /// `distFromSystem`, in arrival light-seconds (not inter-system LY).
    pub arrival_ls: f64,
    pub market_state: String,
    pub internal_starsystem_id: u64,
    pub blackmarket_service: i64,
    pub commodities_service: i64,
    pub max_create_commodities: Option<i64>,
    pub colonisation_template: Option<String>,
    pub commodities: Vec<BulkCommodity>,
    pub allow_dumping: bool,
    /// Candidate-price simulation time, not a quantity-verification time.
    pub simulated_at_s: i64,
    pub small_pads: bool,
    pub medium_pads: bool,
    pub large_pads: bool,
    pub surface: bool,
    pub commodity_overrides_only: bool,
}

/// The only two meanings observed for a bulk commodity row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Producer,
    Consumer,
}

/// One candidate price row.
///
/// There is deliberately no stock or demand member.  Both quantities are
/// unpublished by this endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkCommodity {
    pub commodity_id: u64,
    pub side: Side,
    /// Present only for a producer; this is the station's candidate supply
    /// price.  A consumer's stray `buyPrice` is ignored, never made supply.
    pub buy_price: Option<i64>,
    /// The wire's `sellPrice`.  It is a demand price only when `side` is
    /// [`Side::Consumer`].
    pub sell_price: i64,
    pub illegal: bool,
    /// Despite the wire suffix `Qty`, this is a jurisdiction code/mask.
    pub illegal_jurisdiction_code: i64,
}

impl BulkCommodity {
    /// Candidate price paid by a commander buying from a producer.
    #[must_use]
    pub const fn supply_price(&self) -> Option<i64> {
        match self.side {
            Side::Producer => self.buy_price,
            Side::Consumer => None,
        }
    }

    /// Candidate price paid by a consumer buying from a commander.
    #[must_use]
    pub const fn demand_price(&self) -> Option<i64> {
        match self.side {
            Side::Consumer => Some(self.sell_price),
            Side::Producer => None,
        }
    }
}

/// Parse a marketdata document without letting one malformed row poison its
/// siblings.
#[must_use]
pub fn parse_marketdata(document: &JsValue) -> MarketData {
    let mut parsed = MarketData::default();
    let Some(systems) = document
        .as_record()
        .and_then(|root| root.get("starsystems"))
        .and_then(JsValue::as_record)
    else {
        parsed.rejected.malformed += 1;
        return parsed;
    };

    let mut addresses = BTreeSet::new();
    for (key, value) in systems.iter() {
        let Some(address) = positive_key(key, &mut parsed.rejected) else {
            continue;
        };
        if !addresses.insert(address) {
            parsed.rejected.duplicates += 1;
            continue;
        }
        let Some(system) = parse_system(address, value, &mut parsed.rejected) else {
            continue;
        };
        parsed.systems.push(system);
    }

    parsed.systems.sort_by_key(|system| system.address);
    parsed
}

/// Spelling for callers that separate the words in the endpoint name.
#[must_use]
pub fn parse_market_data(document: &JsValue) -> MarketData {
    parse_marketdata(document)
}

fn parse_system(
    address: u64,
    value: &JsValue,
    counts: &mut ParseCounts,
) -> Option<SystemMarketData> {
    let record = as_record(value, counts)?;
    if !optional_embedded_id_matches(record, &["systemAddr", "address", "id64"], address, counts) {
        return None;
    }

    let name = nonempty_string(record, "name", counts)?.to_owned();
    let has_fleet_carriers = required_bool(record, "hasFleetCarriers", counts)?;
    let tech_broker = required_string(record, "techBroker", counts)?.to_owned();
    let material_trader = required_string(record, "materialTrader", counts)?.to_owned();
    let black_market = required_bool(record, "blackMarket", counts)?;
    let facilitator = required_bool(record, "facilitator", counts)?;
    let voucher_redemption = required_bool(record, "voucherredemption", counts)?;
    let carrier_vendor = required_bool(record, "carrierVendor", counts)?;
    let module_packs = required_bool(record, "modulepacks", counts)?;

    // An invalid server expiry is recoverable: zero is an explicit sentinel
    // for applying SystemMarketCacheTime relative to the read instant.
    let cache_until_s = match record.get("cacheuntil").and_then(exact_i64) {
        Some(value) if value > 0 => value,
        _ => {
            counts.malformed += 1;
            0
        }
    };

    let Some(markets) = record.get("markets").and_then(JsValue::as_record) else {
        counts.malformed += 1;
        return None;
    };

    let mut market_ids = BTreeSet::new();
    let mut parsed_markets = Vec::with_capacity(markets.len());
    for (key, value) in markets.iter() {
        let Some(market_id) = positive_key(key, counts) else {
            continue;
        };
        if !market_ids.insert(market_id) {
            counts.duplicates += 1;
            continue;
        }
        if let Some(market) = parse_market(market_id, &name, value, counts) {
            parsed_markets.push(market);
        }
    }
    parsed_markets.sort_by_key(|market| market.market_id);

    Some(SystemMarketData {
        address,
        name,
        has_fleet_carriers,
        markets: parsed_markets,
        tech_broker,
        material_trader,
        black_market,
        facilitator,
        voucher_redemption,
        carrier_vendor,
        module_packs,
        cache_until_s,
    })
}

fn parse_market(
    market_id: u64,
    parent_system_name: &str,
    value: &JsValue,
    counts: &mut ParseCounts,
) -> Option<BulkMarket> {
    let record = as_record(value, counts)?;
    if !required_embedded_id_matches(record, "id", market_id, counts) {
        return None;
    }

    let system_name = match record.get("systemName").and_then(JsValue::as_str) {
        Some(name) if !name.is_empty() => name.to_owned(),
        // The parent supplies the same official field, so this fallback does
        // not invent a name when a market row omits its redundant copy.
        None => parent_system_name.to_owned(),
        _ => {
            counts.malformed += 1;
            return None;
        }
    };
    let name = nonempty_string(record, "name", counts)?.to_owned();
    let arrival_ls = match record.get("distFromSystem").and_then(JsValue::as_f64) {
        Some(value) if value.is_finite() && value >= 0.0 => value,
        _ => {
            counts.malformed += 1;
            return None;
        }
    };
    let market_state = required_string(record, "market_state", counts)?.to_owned();
    let internal_starsystem_id = positive_id_field(record, "starsystem_id", counts)?;
    let blackmarket_service = nonnegative_integer(record, "service_blackmarket", counts)?;
    let commodities_service = nonnegative_integer(record, "service_commodities", counts)?;
    let max_create_commodities =
        optional_nonnegative_integer(record, "maxCreateCommodities", counts)?;
    let colonisation_template = optional_string(record, "colonisationTemplate", counts)?;
    let allow_dumping = required_bool(record, "allowDumping", counts)?;
    let simulated_at_s = positive_integer(record, "simulatedAt", counts)?;
    let small_pads = required_bool(record, "smallPads", counts)?;
    let medium_pads = required_bool(record, "mediumPads", counts)?;
    let large_pads = required_bool(record, "largePads", counts)?;
    let surface = required_bool(record, "surface", counts)?;
    let commodity_overrides_only = match record.get("commodityOverridesOnly") {
        None => false,
        Some(JsValue::Bool(value)) => *value,
        Some(_) => {
            counts.malformed += 1;
            return None;
        }
    };

    let Some(commodities) = record.get("commodities").and_then(JsValue::as_record) else {
        counts.malformed += 1;
        return None;
    };
    let mut commodity_ids = BTreeSet::new();
    let mut parsed_commodities = Vec::with_capacity(commodities.len());
    for (key, value) in commodities.iter() {
        let Some(commodity_id) = positive_key(key, counts) else {
            continue;
        };
        if !commodity_ids.insert(commodity_id) {
            counts.duplicates += 1;
            continue;
        }
        if let Some(commodity) = parse_commodity(commodity_id, value, counts) {
            parsed_commodities.push(commodity);
        }
    }
    parsed_commodities.sort_by_key(|commodity| commodity.commodity_id);

    Some(BulkMarket {
        market_id,
        system_name,
        name,
        arrival_ls,
        market_state,
        internal_starsystem_id,
        blackmarket_service,
        commodities_service,
        max_create_commodities,
        colonisation_template,
        commodities: parsed_commodities,
        allow_dumping,
        simulated_at_s,
        small_pads,
        medium_pads,
        large_pads,
        surface,
        commodity_overrides_only,
    })
}

fn parse_commodity(
    commodity_id: u64,
    value: &JsValue,
    counts: &mut ParseCounts,
) -> Option<BulkCommodity> {
    let record = as_record(value, counts)?;
    if !optional_embedded_id_matches(record, &["id", "commodityId"], commodity_id, counts) {
        return None;
    }

    let side = match record.get("type").and_then(JsValue::as_str) {
        Some("producer") => Side::Producer,
        Some("consumer") => Side::Consumer,
        Some(_) => {
            counts.unknown_kinds += 1;
            return None;
        }
        None => {
            counts.malformed += 1;
            return None;
        }
    };

    let sell_price = match record.get("sellPrice").and_then(exact_i64) {
        Some(price) if price >= 0 => price,
        Some(_) => {
            counts.invalid_semantics += 1;
            return None;
        }
        None => {
            counts.malformed += 1;
            return None;
        }
    };

    let buy_price = match side {
        Side::Producer => match record.get("buyPrice").and_then(exact_i64) {
            Some(price) if price > 0 => Some(price),
            Some(_) => {
                counts.invalid_semantics += 1;
                return None;
            }
            None => {
                counts.malformed += 1;
                return None;
            }
        },
        // Consumer buyPrice is not supply evidence, even if a drifted payload
        // happens to include it.
        Side::Consumer => None,
    };
    if side == Side::Consumer && sell_price == 0 {
        counts.invalid_semantics += 1;
        return None;
    }

    let illegal = match record.get("illegal") {
        None => false,
        Some(JsValue::Bool(value)) => *value,
        Some(JsValue::Num(value)) if *value == 0.0 => false,
        Some(JsValue::Num(value)) if *value == 1.0 => true,
        Some(_) => {
            counts.invalid_semantics += 1;
            return None;
        }
    };
    let illegal_jurisdiction_code = match record.get("illegalJurisdictionQty").and_then(exact_i64) {
        Some(code) if code >= 0 => code,
        Some(_) => {
            counts.invalid_semantics += 1;
            return None;
        }
        None => {
            counts.malformed += 1;
            return None;
        }
    };

    Some(BulkCommodity {
        commodity_id,
        side,
        buy_price,
        sell_price,
        illegal,
        illegal_jurisdiction_code,
    })
}

fn positive_key(key: &str, counts: &mut ParseCounts) -> Option<u64> {
    match decimal_u64(key) {
        Some(id) if id > 0 => Some(id),
        Some(_) => {
            counts.invalid_semantics += 1;
            None
        }
        None => {
            counts.malformed += 1;
            None
        }
    }
}

fn as_record<'a>(value: &'a JsValue, counts: &mut ParseCounts) -> Option<&'a JsObject> {
    if let Some(record) = value.as_record() {
        Some(record)
    } else {
        counts.malformed += 1;
        None
    }
}

fn required_embedded_id_matches(
    record: &JsObject,
    field: &str,
    authoritative: u64,
    counts: &mut ParseCounts,
) -> bool {
    let Some(value) = record.get(field) else {
        counts.malformed += 1;
        return false;
    };
    match exact_u64(value) {
        Some(embedded) if embedded == authoritative => true,
        Some(_) => {
            counts.id_mismatches += 1;
            false
        }
        None => {
            counts.malformed += 1;
            false
        }
    }
}

fn optional_embedded_id_matches(
    record: &JsObject,
    fields: &[&str],
    authoritative: u64,
    counts: &mut ParseCounts,
) -> bool {
    for field in fields {
        let Some(value) = record.get(field) else {
            continue;
        };
        return match exact_u64(value) {
            Some(embedded) if embedded == authoritative => true,
            Some(_) => {
                counts.id_mismatches += 1;
                false
            }
            None => {
                counts.malformed += 1;
                false
            }
        };
    }
    true
}

fn required_string<'a>(
    record: &'a JsObject,
    field: &str,
    counts: &mut ParseCounts,
) -> Option<&'a str> {
    if let Some(value) = record.get(field).and_then(JsValue::as_str) {
        Some(value)
    } else {
        counts.malformed += 1;
        None
    }
}

fn nonempty_string<'a>(
    record: &'a JsObject,
    field: &str,
    counts: &mut ParseCounts,
) -> Option<&'a str> {
    match required_string(record, field, counts) {
        Some(value) if !value.is_empty() => Some(value),
        Some(_) => {
            counts.malformed += 1;
            None
        }
        None => None,
    }
}

fn required_bool(record: &JsObject, field: &str, counts: &mut ParseCounts) -> Option<bool> {
    if let Some(JsValue::Bool(value)) = record.get(field) {
        Some(*value)
    } else {
        counts.malformed += 1;
        None
    }
}

fn positive_id_field(record: &JsObject, field: &str, counts: &mut ParseCounts) -> Option<u64> {
    match record.get(field).and_then(exact_u64) {
        Some(value) if value > 0 => Some(value),
        Some(_) => {
            counts.invalid_semantics += 1;
            None
        }
        None => {
            counts.malformed += 1;
            None
        }
    }
}

fn positive_integer(record: &JsObject, field: &str, counts: &mut ParseCounts) -> Option<i64> {
    match record.get(field).and_then(exact_i64) {
        Some(value) if value > 0 => Some(value),
        Some(_) => {
            counts.invalid_semantics += 1;
            None
        }
        None => {
            counts.malformed += 1;
            None
        }
    }
}

fn nonnegative_integer(record: &JsObject, field: &str, counts: &mut ParseCounts) -> Option<i64> {
    match record.get(field).and_then(exact_i64) {
        Some(value) if value >= 0 => Some(value),
        Some(_) => {
            counts.invalid_semantics += 1;
            None
        }
        None => {
            counts.malformed += 1;
            None
        }
    }
}

#[allow(
    clippy::option_option,
    reason = "outer None rejects the row; inner None preserves a nullable field"
)]
fn optional_nonnegative_integer(
    record: &JsObject,
    field: &str,
    counts: &mut ParseCounts,
) -> Option<Option<i64>> {
    match record.get(field) {
        None | Some(JsValue::Null) => Some(None),
        Some(value) => match exact_i64(value) {
            Some(value) if value >= 0 => Some(Some(value)),
            Some(_) => {
                counts.invalid_semantics += 1;
                None
            }
            None => {
                counts.malformed += 1;
                None
            }
        },
    }
}

#[allow(
    clippy::option_option,
    reason = "outer None rejects the row; inner None preserves a nullable field"
)]
fn optional_string(
    record: &JsObject,
    field: &str,
    counts: &mut ParseCounts,
) -> Option<Option<String>> {
    match record.get(field) {
        None | Some(JsValue::Null) => Some(None),
        Some(JsValue::Str(value)) => Some(Some(value.to_string())),
        Some(_) => {
            counts.malformed += 1;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(source: &str) -> JsValue {
        JsValue::parse(source).expect("fixture is JSON")
    }

    const SYSTEM_FIELDS: &str = r#"
        "name":"Exact Future",
        "hasFleetCarriers":true,
        "techBroker":"none",
        "materialTrader":"raw",
        "blackMarket":true,
        "facilitator":false,
        "voucherredemption":true,
        "carrierVendor":false,
        "modulepacks":false,
        "cacheuntil":1900000000
    "#;

    fn fixture() -> JsValue {
        // IDs deliberately arrive out of numeric order, and all three map
        // levels include an exact key above Number.MAX_SAFE_INTEGER.
        let source = format!(
            r#"{{"starsystems":{{
                "9007199254740993":{{
                    {SYSTEM_FIELDS},
                    "markets":{{
                        "9007199254740995":{{
                            "id":"9007199254740995",
                            "systemName":"Exact Future",
                            "name":"Far Future Port",
                            "distFromSystem":12.5,
                            "market_state":"",
                            "starsystem_id":"115271",
                            "service_blackmarket":"2",
                            "service_commodities":"1",
                            "maxCreateCommodities":"7",
                            "colonisationTemplate":"orbital",
                            "allowDumping":true,
                            "simulatedAt":1800000001,
                            "smallPads":true,
                            "mediumPads":true,
                            "largePads":false,
                            "surface":false,
                            "commodityOverridesOnly":true,
                            "commodities":{{
                                "9007199254740997":{{"type":"consumer","buyPrice":1,"sellPrice":500,"illegal":1,"illegalJurisdictionQty":46}},
                                "20":{{"type":"consumer","sellPrice":200,"illegalJurisdictionQty":0}},
                                "10":{{"type":"producer","buyPrice":100,"sellPrice":90,"illegal":true,"illegalJurisdictionQty":31}},
                                "21":{{"type":"mystery","sellPrice":1,"illegalJurisdictionQty":0}},
                                "22":{{"type":"consumer","sellPrice":0,"illegalJurisdictionQty":0}},
                                "23":{{"type":"consumer","sellPrice":1,"illegal":2,"illegalJurisdictionQty":0}},
                                "24":{{"id":"25","type":"consumer","sellPrice":1,"illegalJurisdictionQty":0}},
                                "bad":null
                            }}
                        }},
                        "11":{{"id":"12"}},
                        "broken":null
                    }}
                }},
                "2":{{
                    "name":"Empty",
                    "hasFleetCarriers":false,
                    "techBroker":"none",
                    "materialTrader":"none",
                    "blackMarket":false,
                    "facilitator":false,
                    "voucherredemption":false,
                    "carrierVendor":false,
                    "modulepacks":false,
                    "cacheuntil":"not-a-time",
                    "markets":{{}}
                }}
            }}}}"#,
        );
        json(&source)
    }

    #[test]
    fn exact_keys_survive_above_2_pow_53_and_every_level_is_sorted() {
        let parsed = parse_marketdata(&fixture());
        assert_eq!(
            parsed
                .systems
                .iter()
                .map(|system| system.address)
                .collect::<Vec<_>>(),
            vec![2, 9_007_199_254_740_993]
        );
        let future = &parsed.systems[1];
        assert_eq!(future.markets[0].market_id, 9_007_199_254_740_995);
        assert_eq!(
            future.markets[0]
                .commodities
                .iter()
                .map(|commodity| commodity.commodity_id)
                .collect::<Vec<_>>(),
            vec![10, 20, 9_007_199_254_740_997]
        );
        assert_eq!(
            parsed.systems[0].cache_until_s, 0,
            "finance fallback remains a cache decision"
        );
    }

    #[test]
    fn producer_is_supply_only_and_consumer_is_demand_only() {
        let parsed = parse_marketdata(&fixture());
        let rows = &parsed.systems[1].markets[0].commodities;
        let producer = rows.iter().find(|row| row.commodity_id == 10).unwrap();
        assert_eq!(producer.side, Side::Producer);
        assert_eq!(producer.supply_price(), Some(100));
        assert_eq!(
            producer.demand_price(),
            None,
            "producer sellPrice is not demand"
        );

        let consumer = rows.iter().find(|row| row.commodity_id == 20).unwrap();
        assert_eq!(consumer.side, Side::Consumer);
        assert_eq!(consumer.supply_price(), None);
        assert_eq!(consumer.demand_price(), Some(200));
        assert_eq!(consumer.buy_price, None);

        let drifted = rows
            .iter()
            .find(|row| row.commodity_id == 9_007_199_254_740_997)
            .unwrap();
        assert_eq!(
            drifted.buy_price, None,
            "a stray consumer buyPrice is ignored"
        );
        assert_eq!(
            drifted.illegal_jurisdiction_code, 46,
            "this is not a quantity"
        );
    }

    #[test]
    fn pads_services_and_simulation_times_remain_typed() {
        let parsed = parse_marketdata(&fixture());
        let system = &parsed.systems[1];
        let market = &system.markets[0];
        assert!(system.has_fleet_carriers);
        assert_eq!(system.tech_broker, "none");
        assert_eq!(system.material_trader, "raw");
        assert_eq!(system.cache_until_s, 1_900_000_000);
        assert_eq!(market.arrival_ls, 12.5);
        assert_eq!(market.internal_starsystem_id, 115_271);
        assert_eq!(market.blackmarket_service, 2);
        assert_eq!(market.commodities_service, 1);
        assert_eq!(market.max_create_commodities, Some(7));
        assert_eq!(market.colonisation_template.as_deref(), Some("orbital"));
        assert_eq!(market.simulated_at_s, 1_800_000_001);
        assert!(
            (
                market.small_pads,
                market.medium_pads,
                !market.large_pads,
                !market.surface
            )
                .0
        );
        assert!(market.commodity_overrides_only);
    }

    #[test]
    fn mismatches_malformed_rows_unknown_sides_and_illegal_values_are_local() {
        let parsed = parse_marketdata(&fixture());
        assert_eq!(
            parsed.systems.len(),
            2,
            "bad children do not erase their systems"
        );
        assert_eq!(parsed.systems[1].markets.len(), 1, "bad markets are local");
        assert_eq!(parsed.systems[1].markets[0].commodities.len(), 3);
        assert!(
            parsed.rejected.id_mismatches >= 2,
            "market and commodity mismatches counted"
        );
        assert_eq!(parsed.rejected.unknown_kinds, 1);
        assert!(
            parsed.rejected.invalid_semantics >= 2,
            "zero demand and illegal=2 are rejected"
        );
        assert!(parsed.rejected.malformed >= 3);
    }

    #[test]
    fn a_numeric_embedded_id_above_the_safe_range_is_not_guessed() {
        let source = format!(
            r#"{{"starsystems":{{"9007199254740993":{{
                {SYSTEM_FIELDS},
                "markets":{{"9007199254740995":{{"id":9007199254740995}}}}
            }}}}}}"#,
        );
        let parsed = parse_marketdata(&json(&source));
        assert_eq!(parsed.systems.len(), 1);
        assert!(parsed.systems[0].markets.is_empty());
        assert!(parsed.rejected.malformed > 0);
    }

    #[test]
    fn wrong_top_level_shape_is_an_empty_counted_result() {
        let parsed = parse_marketdata(&json(r#"{"starsystems":[]}"#));
        assert!(parsed.systems.is_empty());
        assert_eq!(parsed.rejected.malformed, 1);
    }
}
