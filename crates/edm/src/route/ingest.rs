//! From a decrypted game-internal API listing to the optimiser's model.
//!
//! One direction, no interpretation. `edm-route` is exact-integer arithmetic
//! over a wire shape that is `f64` all the way down, so this is where the two
//! meet — and it is the only place a number crosses that line.
//!
//! **The crossing is where the exactness claim is either kept or quietly
//! lost.** Every price, stock and demand the game reports is an integer; the
//! port carries them as `f64` because JavaScript does, and because
//! `lookupFaction` depends on values above 2⁵³ rounding. A row whose price is
//! not an exact integer is therefore not a price this program can act on, and
//! it is dropped and counted rather than truncated into one.

use std::collections::HashMap;

use edm_core::ardent::ArdentStation;
use edm_core::domain::Commodity;
use edm_route::model::{
    Commodities, IngestCounts, Market, MarketIdentity, RawCommodity, RowFloors,
};
use edm_route::num::Tons;

use crate::route::acquire::Listing;

/// What crossing the boundary cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Crossing {
    /// Listings whose payload was not a market listing at all.
    pub unparsed: u32,
    /// Rows carrying a quantity or price that is not an exact integer.
    ///
    /// Non-zero here means the wire is reporting something this program has no
    /// model for, which is worth saying out loud rather than rounding away.
    pub non_integral: u32,
    /// Listings missing a stable positive market/system identity or finite
    /// system coordinates. These are excluded rather than collapsed into zero.
    pub invalid_identity: u32,
    /// What the optimiser's own ingest counted.
    pub rows: IngestCounts,
}

/// Build the optimiser's market list from a sweep's listings.
///
/// `stations` supplies the coordinates and arrival distance, which the market
/// payload does not carry — the position of a station is a property of where it
/// is, not of what it sells.
///
/// **Takes the listings by value and drops each payload as it is consumed.** A
/// parsed 391-commodity document is ~0.48 MiB resident — 5.5x its own
/// decompressed wire text — so at five thousand markets holding them all costs
/// about 2.3 GiB, and the trade graph the optimiser is about to build is
/// another gigabyte and a half. Nothing reads a `JsValue` again after its
/// `Market` exists, so keeping them alive only raised the peak.
#[must_use]
pub fn markets(
    listings: Vec<Listing>,
    stations: &[ArdentStation],
    floors: &RowFloors,
) -> (Vec<Market>, Commodities, Crossing) {
    markets_with_candidates(listings, stations, floors, &HashMap::new())
}

/// Build markets while pairing official commander-neutral candidate prices by
/// `(market_id, commodity_id)`. The verified listing still supplies demand,
/// stock and mean price; an absent candidate leaves the legacy fixed quote.
#[must_use]
pub fn markets_with_candidates<S: std::hash::BuildHasher>(
    listings: Vec<Listing>,
    stations: &[ArdentStation],
    floors: &RowFloors,
    candidate_demand_prices: &HashMap<(i64, i64), i64, S>,
) -> (Vec<Market>, Commodities, Crossing) {
    let mut commodities = Commodities::new();
    let mut crossing = Crossing::default();
    let mut built = Vec::with_capacity(listings.len());

    for listing in listings {
        let Some(snapshot) = listing.snapshot() else {
            crossing.unparsed += 1;
            continue;
        };
        let Some(market_id) = exact(listing.market_id).filter(|id| *id > 0) else {
            crossing.invalid_identity += 1;
            continue;
        };
        let Some(station) = stations.iter().find(|s| s.market_id == listing.market_id) else {
            crossing.invalid_identity += 1;
            continue;
        };
        let Some(system_address) = exact(station.system_address).filter(|id| *id > 0) else {
            crossing.invalid_identity += 1;
            continue;
        };
        let coords = station.coordinates;
        if !coords.x.is_finite() || !coords.y.is_finite() || !coords.z.is_finite() {
            crossing.invalid_identity += 1;
            continue;
        }
        let rows: Vec<RawCommodity> = snapshot
            .commodities
            .iter()
            .filter_map(|row| {
                let mut raw = raw_commodity(row, &mut crossing)?;
                let Some(commodity_id) = exact(row.id) else {
                    crossing.non_integral += 1;
                    return None;
                };
                raw.candidate_sell_price = candidate_demand_prices
                    .get(&(market_id, commodity_id))
                    .copied();
                Some(raw)
            })
            .collect();

        built.push(Market::from_rows(
            MarketIdentity {
                market_id,
                station: listing.station_name.clone(),
                system: listing.system_name.clone(),
                system_address,
                coords,
                // An unreported arrival distance is charged as if the station
                // were at the star. That understates the supercruise leg, which
                // is the direction that *overstates* the rate — so it is
                // recorded as a caveat by the caller rather than hidden.
                arrival_ls: station.distance_to_arrival.unwrap_or(0.0),
            },
            &rows,
            &mut commodities,
            floors,
            &mut crossing.rows,
        ));
    }

    (built, commodities, crossing)
}

/// How many of these listings the optimiser can actually price.
///
/// Counted here rather than by the caller because the caller no longer has the
/// listings once [`markets`] has consumed them — and counting before ingest
/// would parse every payload twice.
#[must_use]
pub fn priced(listings: &[Listing]) -> usize {
    listings.iter().filter(|listing| listing.snapshot().is_some()).count()
}

/// One commodity row, or `None` if it does not cross the boundary intact.
fn raw_commodity(row: &Commodity<'_>, crossing: &mut Crossing) -> Option<RawCommodity> {
    // All six together: a row with an exact price and a fractional stock is
    // still a row this program cannot reason about exactly, and taking half of
    // it would put a price in the graph with a quantity that does not match.
    let [Some(buy_price), Some(sell_price), Some(mean_price), Some(stock), Some(stock_bracket), Some(demand), Some(demand_bracket)] = [
        exact(row.buy_price),
        exact(row.sell_price),
        exact(row.mean_price),
        exact(row.stock),
        exact(row.stock_bracket),
        exact(row.demand),
        exact(row.demand_bracket),
    ] else {
        crossing.non_integral += 1;
        return None;
    };
    Some(RawCommodity {
        name: row.name.to_owned(),
        buy_price,
        sell_price,
        candidate_sell_price: None,
        mean_price,
        stock,
        stock_bracket,
        demand,
        demand_bracket,
        category: row.category.to_owned(),
        // From *this* market's `legality` field, which is why it is read per
        // row rather than looked up per commodity.
        illegal: row.illegal,
    })
}

/// An `f64` that is exactly an integer, as an `i64`.
///
/// `as` would truncate `1.5` to `1` and saturate an out-of-range value to a
/// bound, both silently. Neither is a price.
fn exact(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    // 2^63 is not representable as a bound in `f64` comparison without care;
    // 2^53 is where `f64` stops representing consecutive integers anyway, and
    // no quantity in this game approaches it.
    if value.abs() > 9_007_199_254_740_992.0 {
        return None;
    }
    Some(value as i64)
}

/// Ship and search settings, from the command line.
#[must_use]
pub fn floors(config: &edm_core::cli::config::RouteConfig) -> RowFloors {
    RowFloors {
        min_stock: Tons(exact(config.min_supply).unwrap_or(1)),
        min_demand: Tons(exact(config.min_demand).unwrap_or(1)),
        categories: config.categories.clone(),
        allow_illegal: config.include_illegal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edm_core::domain::id64::Coordinates;

    #[test]
    fn only_exact_integers_cross() {
        assert_eq!(exact(1_234.0), Some(1_234));
        assert_eq!(exact(-1.0), Some(-1));
        assert_eq!(exact(0.0), Some(0));
        assert_eq!(exact(1.5), None, "a fractional price is not a price");
        assert_eq!(exact(f64::NAN), None);
        assert_eq!(exact(f64::INFINITY), None);
        // Past 2^53 the input is already an approximation of something else.
        assert_eq!(exact(9_007_199_254_740_994.0), None);
    }

    /// A row is taken whole or not at all. Half a row would put a price in the
    /// graph beside a quantity that does not belong to it.
    #[test]
    fn a_row_with_one_bad_field_does_not_half_cross() {
        let mut crossing = Crossing::default();
        let good = Commodity {
            id: 1.0,
            name: "gold",
            category: "metals",
            stock: 100.0,
            stock_bracket: 2.0,
            buy_price: 9_000.0,
            sell_price: 9_500.0,
            fence_price: 0.0,
            demand: 50.0,
            demand_bracket: 2.0,
            mean_price: 9_200.0,
            consumer: true,
            producer: true,
            rare: false,
            illegal: false,
        };
        assert!(raw_commodity(&good, &mut crossing).is_some());

        let fractional_stock = Commodity { stock: 100.5, ..good };
        assert!(raw_commodity(&fractional_stock, &mut crossing).is_none());

        let fractional_price = Commodity { buy_price: 9_000.5, ..good };
        assert!(raw_commodity(&fractional_price, &mut crossing).is_none());
        assert_eq!(crossing.non_integral, 2, "each rejected row is reported exactly once");
    }

    fn listing(market_id: f64) -> Listing {
        Listing {
            market_id,
            station_name: "Galileo".to_owned(),
            system_name: "Sol".to_owned(),
            document: edm_core::js::json::JsValue::parse(
                r#"{"commodities":{"1":{"id":1,"name":"Gold","categoryname":"Metals","stock":10,"stockBracket":1,"buyPrice":100,"sellPrice":90,"meanPrice":95,"demand":10,"demandBracket":1,"producer":1,"consumer":1,"legality":""}},"inventory":[]}"#,
            ).expect("fixture JSON"),
            read_at_ms: 1_000.0,
            observed_at_ms: None,
            from_cache: false,
        }
    }

    fn placed_station(address: f64) -> ArdentStation {
        ArdentStation {
            market_id: 42.0,
            station_name: "Galileo".to_owned(),
            system_name: "Sol".to_owned(),
            system_address: address,
            station_type: Some("Orbis".to_owned()),
            max_landing_pad_size: Some(3.0),
            distance_to_arrival: Some(500.0),
            coordinates: Coordinates { x: 0.0, y: 0.0, z: 0.0 },
        }
    }

    #[test]
    fn placed_identity_reaches_the_optimizer_without_zero_sentinels() {
        let (markets, _, crossing) = markets(
            vec![listing(42.0)],
            &[placed_station(10_477_373_803.0)],
            &RowFloors::default(),
        );
        assert_eq!(crossing.invalid_identity, 0);
        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0].market_id, 42);
        assert_eq!(markets[0].system_address, 10_477_373_803);
    }

    #[test]
    fn invalid_or_missing_identity_is_skipped_and_counted_not_collapsed_to_zero() {
        let bad = placed_station(f64::NAN);
        let (built, _, crossing) =
            markets(vec![listing(42.0)], &[bad], &RowFloors::default());
        assert!(built.is_empty());
        assert_eq!(crossing.invalid_identity, 1);

        let (built, _, crossing) =
            markets(vec![listing(42.0)], &[], &RowFloors::default());
        assert!(built.is_empty());
        assert_eq!(crossing.invalid_identity, 1);
    }


    #[test]
    fn official_candidate_price_pairs_with_verified_quantity_and_mean() {
        let candidates = HashMap::from([((42, 1), 120_i64)]);
        let (built, _, crossing) = markets_with_candidates(
            vec![listing(42.0)],
            &[placed_station(10_477_373_803.0)],
            &RowFloors::default(),
            &candidates,
        );
        assert_eq!(crossing.invalid_identity, 0);
        let demand = &built[0].demand[0];
        assert_eq!(demand.sell_price.0, 120, "base is the optimistic graph bound");
        let bulk = demand.bulk.expect("paired quote");
        assert_eq!(bulk.base_sell_price.0, 120);
        assert_eq!(bulk.mean_price.0, 95);
    }

}
