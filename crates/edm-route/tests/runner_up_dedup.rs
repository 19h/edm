//! The runner-up listing must not be quadratic in its own budget.
//!
//! `round::listing` enumerates up to `RUNNER_UP_BUDGET` cycles — two hundred
//! thousand — and rejects the ones whose station set it has already seen. That
//! set used to be a `Vec` scanned linearly, which is about 2e10 `Vec<i64>`
//! comparisons: a **fixed stall in complete silence on every run** large enough
//! to saturate the budget, which is roughly two hundred markets upward. It did
//! not shrink with a narrower search, because the budget is a constant.
//!
//! Measured on this instance, release, identical output either way:
//!
//! | dedup            | `solve` |
//! |------------------|---------|
//! | `Vec::contains`  | 57.3 s  |
//! | `HashSet::insert`|  0.42 s |
//!
//! A wall-clock assertion is a blunt instrument and this one is deliberately
//! loose — it is not measuring performance, it is catching the reintroduction
//! of a quadratic. Thirty seconds is under the old cost and far above the new
//! one in either profile (2.0 s debug, 0.42 s release), so it cannot flake on a
//! slow machine without the quadratic actually being back.

use std::time::{Duration, Instant};

use edm_core::domain::id64::Coordinates;
use edm_route::model::{
    Commodities, IngestCounts, Limits, Market, MarketIdentity, RawCommodity, RowFloors, ShipConfig,
};
use edm_route::time::TimeModel;

/// The ceiling. See the module docs: this is a quadratic detector, not a
/// benchmark.
const CEILING: Duration = Duration::from_secs(30);

/// Enough markets, each trading enough commodities, that the cycle enumeration
/// saturates its budget — which is the only condition under which the old code
/// was slow.
const MARKETS: i64 = 240;

fn market(id: i64) -> Market {
    // Prices that vary per market and per commodity, so most ordered pairs are
    // profitable in one direction and the graph is dense enough to enumerate.
    let rows: Vec<RawCommodity> = (0..40)
        .map(|c| {
            let base = 1_000 + ((id * 37 + c * 91) % 400);
            RawCommodity {
                name: format!("c{c}"),
                buy_price: base,
                sell_price: base - 50,
                stock: 5_000,
                stock_bracket: 3,
                demand: 5_000,
                demand_bracket: 3,
            }
        })
        .collect();

    let mut commodities = Commodities::new();
    let mut counts = IngestCounts::default();
    Market::from_rows(
        MarketIdentity {
            market_id: id,
            station: format!("Station {id}"),
            system: format!("System {id}"),
            system_address: id,
            coords: Coordinates { x: id as f64 * 3.0, y: 0.0, z: 0.0 },
            arrival_ls: 100.0,
        },
        &rows,
        &mut commodities,
        RowFloors::default(),
        &mut counts,
    )
}

#[test]
fn the_runner_up_listing_does_not_rescan_everything_it_has_seen() {
    let markets: Vec<Market> = (0..MARKETS).map(market).collect();

    let started = Instant::now();
    let solution =
        edm_route::solve(&markets, TimeModel::default(), &ShipConfig::default(), &Limits::default());
    let elapsed = started.elapsed();

    // The instance has to actually reach the listing, or the timing proves
    // nothing at all.
    assert!(!solution.round_trip.is_empty(), "the instance must produce round trips");
    assert!(
        solution.round_trip.len() > 1,
        "and runners-up, or the dedup is never exercised: {}",
        solution.round_trip.len()
    );

    assert!(
        elapsed < CEILING,
        "solve took {elapsed:?} for {MARKETS} markets — the runner-up dedup has gone quadratic \
         in RUNNER_UP_BUDGET again (it was 57 s before, 0.42 s after)"
    );
}

/// And the fix changed nothing about the answer: a set and a linear scan reject
/// exactly the same duplicates, because `RankKey::build` already rotates the
/// smallest market id to the front and so gives one canonical form per cycle.
#[test]
fn runners_up_are_distinct_routes() {
    let markets: Vec<Market> = (0..40).map(market).collect();
    let solution =
        edm_route::solve(&markets, TimeModel::default(), &ShipConfig::default(), &Limits::default());

    let mut seen: Vec<&[i64]> = Vec::new();
    for route in &solution.round_trip {
        let stations = route.rank.stations.as_slice();
        assert!(!seen.contains(&stations), "duplicate route in the listing: {stations:?}");
        seen.push(stations);
    }
    assert!(seen.len() > 1, "the listing must hold more than the head");
}
