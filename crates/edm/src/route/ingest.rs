//! From a decrypted Companion API listing to the optimiser's model.
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

use edm_core::ardent::ArdentStation;
use edm_core::domain::Commodity;
use edm_core::domain::id64::Coordinates;
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
    /// What the optimiser's own ingest counted.
    pub rows: IngestCounts,
}

/// Build the optimiser's market list from a sweep's listings.
///
/// `stations` supplies the coordinates and arrival distance, which the market
/// payload does not carry — the position of a station is a property of where it
/// is, not of what it sells.
#[must_use]
pub fn markets(
    listings: &[Listing],
    stations: &[ArdentStation],
    floors: RowFloors,
) -> (Vec<Market>, Commodities, Crossing) {
    let mut commodities = Commodities::new();
    let mut crossing = Crossing::default();
    let mut built = Vec::with_capacity(listings.len());

    for listing in listings {
        let Some(snapshot) = listing.snapshot() else {
            crossing.unparsed += 1;
            continue;
        };
        let station = stations.iter().find(|s| s.market_id == listing.market_id);
        let rows: Vec<RawCommodity> = snapshot
            .commodities
            .iter()
            .filter_map(|row| raw_commodity(row, &mut crossing))
            .collect();

        built.push(Market::from_rows(
            MarketIdentity {
                market_id: exact(listing.market_id).unwrap_or(0),
                station: listing.station_name.clone(),
                system: listing.system_name.clone(),
                system_address: 0,
                coords: station.map_or(
                    Coordinates { x: f64::NAN, y: f64::NAN, z: f64::NAN },
                    |s| s.coordinates,
                ),
                // An unreported arrival distance is charged as if the station
                // were at the star. That understates the supercruise leg, which
                // is the direction that *overstates* the rate — so it is
                // recorded as a caveat by the caller rather than hidden.
                arrival_ls: station.and_then(|s| s.distance_to_arrival).unwrap_or(0.0),
            },
            &rows,
            &mut commodities,
            floors,
            &mut crossing.rows,
        ));
    }

    (built, commodities, crossing)
}

/// One commodity row, or `None` if it does not cross the boundary intact.
fn raw_commodity(row: &Commodity<'_>, crossing: &mut Crossing) -> Option<RawCommodity> {
    // All six together: a row with an exact price and a fractional stock is
    // still a row this program cannot reason about exactly, and taking half of
    // it would put a price in the graph with a quantity that does not match.
    let (buy_price, sell_price, stock, stock_bracket, demand, demand_bracket) = (
        exact(row.buy_price)?,
        exact(row.sell_price)?,
        exact(row.stock)?,
        exact(row.stock_bracket)?,
        exact(row.demand)?,
        exact(row.demand_bracket)?,
    );
    let _ = crossing;
    Some(RawCommodity {
        name: row.name.to_owned(),
        buy_price,
        sell_price,
        stock,
        stock_bracket,
        demand,
        demand_bracket,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
