//! `edm route --json` — one document, for piping.
//!
//! **C28.** The ported commands' `--json` is guarded at about twenty individual
//! sites and misses several, so a diagnostic can land in the middle of the
//! stream and corrupt it (R76). That is faithfully reproduced for them and is
//! deliberately *not* reproduced here: `route` is a new command with no oracle,
//! so its JSON is one well-formed document or nothing.
//!
//! The same pairing rule as [`crate::view`] applies, and here it is structural:
//! a route's rate is written as an object that carries `guarantee` and
//! `caveats` beside it, so a consumer cannot read the number without the claim
//! unless it goes out of its way to.

use edm_core::js::json::{JsObject, JsValue};

use crate::Solution;
use crate::model::{Commodities, Market};
use crate::num::Ratio;
use crate::report::{Caveat, Guarantee, Route, RouteKind};
use crate::weight::Limiter;

/// The whole answer, as one document.
#[must_use]
pub fn document(
    solution: &Solution,
    markets: &[Market],
    commodities: &Commodities,
    coverage: JsValue,
) -> JsValue {
    obj(vec![
        ("coverage", coverage),
        ("single", routes(&solution.single, markets, commodities)),
        (
            "roundTrip",
            routes(&solution.round_trip, markets, commodities),
        ),
        ("loops", routes(&solution.loops, markets, commodities)),
    ])
}

fn routes(routes: &[Route], markets: &[Market], commodities: &Commodities) -> JsValue {
    JsValue::Arr(
        routes
            .iter()
            .map(|route| one(route, markets, commodities))
            .collect(),
    )
}

fn one(route: &Route, markets: &[Market], commodities: &Commodities) -> JsValue {
    let claim = route.rate();
    obj(vec![
        ("kind", JsValue::Str(kind(route.kind).into())),
        ("profit", num(route.profit.0)),
        ("cycleMillis", num(route.cycle_millis.0)),
        ("firstLapMillis", num(route.first_lap_millis.0)),
        // The rate is an object, not a number, so `guarantee` and `caveats`
        // travel with it. A consumer that reads `rate.creditsPerHour` has the
        // claim in hand whether it looks at it or not.
        (
            "rate",
            obj(vec![
                (
                    "creditsPerHour",
                    claim
                        .steady
                        .map_or(JsValue::Null, |steady| num(steady.credits_per_hour_floor())),
                ),
                (
                    "firstLapCreditsPerHour",
                    num(claim.first_lap.credits_per_hour_floor()),
                ),
                ("guarantee", guarantee(claim.guarantee)),
                (
                    "caveats",
                    JsValue::Arr(
                        claim
                            .caveats
                            .iter()
                            .map(|c| JsValue::Str(caveat(*c).into()))
                            .collect(),
                    ),
                ),
            ]),
        ),
        (
            "legs",
            JsValue::Arr(
                route
                    .legs
                    .iter()
                    .map(|leg| {
                        obj(vec![
                            ("from", station(markets, leg.from)),
                            ("to", station(markets, leg.to)),
                            (
                                "commodity",
                                commodities
                                    .name(leg.choice.commodity)
                                    .map_or(JsValue::Null, |name| JsValue::Str(name.into())),
                            ),
                            ("units", num(leg.choice.units.0)),
                            ("buyPrice", num(leg.choice.buy_price.0)),
                            ("sellPrice", num(leg.choice.sell_price.0)),
                            ("profit", num(leg.choice.profit.0)),
                            (
                                "limitedBy",
                                JsValue::Str(limiter(leg.choice.limiter).into()),
                            ),
                            ("distanceLy", JsValue::Num(leg.distance_ly)),
                            ("millis", num(leg.millis.0)),
                            ("demandAssumed", JsValue::Bool(leg.choice.demand_assumed)),
                            (
                                "priceProvenance",
                                JsValue::Str(
                                    if leg.choice.bulk_estimated {
                                        "empiricalBulkEstimate"
                                    } else {
                                        "verifiedListing"
                                    }
                                    .into(),
                                ),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn station(markets: &[Market], index: u32) -> JsValue {
    markets.get(index as usize).map_or(JsValue::Null, |market| {
        obj(vec![
            ("marketId", num(market.market_id)),
            ("station", JsValue::Str(market.station.as_str().into())),
            ("system", JsValue::Str(market.system.as_str().into())),
        ])
    })
}

/// A guarantee, with its parameter where it has one.
fn guarantee(guarantee: Guarantee) -> JsValue {
    match guarantee {
        Guarantee::ProvedOptimal => obj(vec![("kind", JsValue::Str("provedOptimal".into()))]),
        Guarantee::OptimalForStartingCredits => obj(vec![(
            "kind",
            JsValue::Str("optimalForStartingCredits".into()),
        )]),
        Guarantee::BoundedGap { upper } => obj(vec![
            ("kind", JsValue::Str("boundedGap".into())),
            (
                "upperCreditsPerHour",
                num(Ratio::credits_per_hour_floor(upper)),
            ),
        ]),
        Guarantee::Heuristic { reason } => obj(vec![
            ("kind", JsValue::Str("heuristic".into())),
            ("reason", JsValue::Str(format!("{reason:?}").into())),
        ]),
    }
}

const fn kind(kind: RouteKind) -> &'static str {
    match kind {
        RouteKind::SingleHop => "singleHop",
        RouteKind::RoundTrip => "roundTrip",
        RouteKind::Loop { .. } => "loop",
    }
}

const fn limiter(limiter: Limiter) -> &'static str {
    match limiter {
        Limiter::Cargo => "cargo",
        Limiter::Stock => "stock",
        Limiter::Demand => "demand",
        Limiter::Credits => "credits",
    }
}

const fn caveat(caveat: Caveat) -> &'static str {
    match caveat {
        Caveat::StockDepletion => "stockDepletion",
        Caveat::DemandUnpublished => "demandUnpublished",
        Caveat::StaleListing => "staleListing",
        Caveat::BulkPriceEstimated => "bulkPriceEstimated",
        Caveat::JumpGraphUnmodelled => "jumpGraphUnmodelled",
        Caveat::CreditCapBinds => "creditCapBinds",
        Caveat::SingleHopNotRepeatable => "singleHopNotRepeatable",
        Caveat::AccessUnmodelled => "accessUnmodelled",
        Caveat::TimeModelAssumed => "timeModelAssumed",
        Caveat::EdgesBelowFloorDropped => "edgesBelowFloorDropped",
    }
}

/// An integer, as JavaScript renders one.
///
/// `JsValue::Num` is the only numeric variant and `stringify` prints an
/// integral `f64` with no decimal point, which is what keeps this document
/// readable by the same tools that read the ported commands' output (F2).
fn num(value: i64) -> JsValue {
    JsValue::Num(value as f64)
}

fn obj(fields: Vec<(&str, JsValue)>) -> JsValue {
    JsValue::Obj(JsObject::from_document_order(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pairing rule, made structural: the number and the claim are in the
    /// same object, so a consumer cannot pick one up without the other in hand.
    #[test]
    fn a_rate_carries_its_guarantee_and_caveats() {
        let route = crate::fixture::proved_round_trip();
        let JsValue::Obj(document) = one(&route, &[], &Commodities::new()) else {
            panic!("an object")
        };
        let Some(JsValue::Obj(rate)) = document.get("rate") else {
            panic!("a rate object")
        };
        assert!(rate.get("creditsPerHour").is_some());
        assert!(
            rate.get("guarantee").is_some(),
            "the guarantee travels with the rate"
        );
        assert!(rate.get("caveats").is_some(), "and so do the caveats");
    }

    /// One document, well formed, whatever is in it. C28 — the ported
    /// commands' `--json` leaks diagnostics into the stream (R76) and this one
    /// does not.
    #[test]
    fn the_document_round_trips_through_a_parser() {
        let solution = Solution {
            round_trip: vec![crate::fixture::proved_round_trip()],
            ..Solution::default()
        };
        let text = document(&solution, &[], &Commodities::new(), JsValue::Null).stringify(2);
        let parsed = JsValue::parse(&text).expect("one well-formed document");
        let JsValue::Obj(root) = parsed else {
            panic!("an object")
        };
        for key in ["coverage", "single", "roundTrip", "loops"] {
            assert!(root.get(key).is_some(), "missing {key}");
        }
    }

    /// Integers print without a decimal point, so the same tools that read the
    /// ported commands' JSON read this (F2).
    #[test]
    fn integers_have_no_decimal_point() {
        assert_eq!(num(3_292_800).stringify_compact(), "3292800");
        assert_eq!(num(0).stringify_compact(), "0");
    }

    /// Every caveat and every guarantee has a distinct wire name, or a consumer
    /// cannot tell two different claims apart.
    #[test]
    fn every_claim_has_its_own_wire_name() {
        let names = [
            Caveat::StockDepletion,
            Caveat::DemandUnpublished,
            Caveat::StaleListing,
            Caveat::BulkPriceEstimated,
            Caveat::JumpGraphUnmodelled,
            Caveat::CreditCapBinds,
            Caveat::SingleHopNotRepeatable,
            Caveat::AccessUnmodelled,
            Caveat::TimeModelAssumed,
            Caveat::EdgesBelowFloorDropped,
        ]
        .map(caveat);
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }

    #[test]
    fn each_leg_names_whether_its_price_is_verified_or_empirical() {
        let mut route = crate::fixture::proved_round_trip();
        route.legs[0].choice.bulk_estimated = true;
        let JsValue::Obj(document) = one(&route, &[], &Commodities::new()) else {
            panic!("route object")
        };
        let Some(JsValue::Arr(legs)) = document.get("legs") else {
            panic!("legs")
        };
        let JsValue::Obj(first) = &legs[0] else {
            panic!("first leg")
        };
        assert_eq!(
            first.get("priceProvenance").and_then(JsValue::as_str),
            Some("empiricalBulkEstimate")
        );
        let JsValue::Obj(second) = &legs[1] else {
            panic!("second leg")
        };
        assert_eq!(
            second.get("priceProvenance").and_then(JsValue::as_str),
            Some("verifiedListing")
        );
    }
}
