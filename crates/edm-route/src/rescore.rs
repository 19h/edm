//! Re-pricing finished routes against a changed instance.
//!
//! A caller that ranks on cheap prices and then re-reads a few markets live
//! needs to know what its shortlist is worth *now*. Re-running [`crate::solve`]
//! would answer that exactly and cannot be afforded: the trade graph is
//! quadratic in the market count and was measured at 127 seconds and a 4.1 GiB
//! peak over 5,049 markets, and one Dinkelbach round at 205 seconds on top.
//! Rescoring a shortlist of eighty routes is microseconds.
//!
//! **What rescoring can and cannot do.** It re-prices routes that already
//! exist. It cannot discover a route the first ranking buried — if a refreshed
//! price makes some unranked pair the best in the region, nothing here will
//! find it. So the ordering claim narrows from *"the best routes there are"* to
//! *"these routes, correctly ordered at today's prices"*, and every rescored
//! route says so through [`Route::mark_rescored`].
//!
//! This is [`crate::thread::Threading::rethread`]'s shape — rescore, re-sort,
//! truncate — differing only in that the legs' *prices* have moved underneath,
//! not just the balance carried into them. A leg that has stopped being
//! tradeable at all drops its route entirely rather than being priced at zero:
//! a route with a dead leg is not a worse route, it is not a route.

use crate::model::{Limits, Market, ShipConfig};
use crate::report::{Route, RouteKind};
use crate::time::{Geometry, TimeModel};
use crate::weight::leg_weight;

/// Re-price routes against `markets`, re-sort, and truncate to `limits.top_n`.
///
/// `markets` must be the same vector the routes were found in, with entries
/// replaced in place — every [`crate::report::RouteLeg`] addresses a market by
/// its index, so a reordered or filtered vector silently re-points every leg at
/// the wrong station.
#[must_use]
pub fn rescore(
    markets: &[Market],
    time: TimeModel,
    ship: &ShipConfig,
    limits: &Limits,
    routes: Vec<Route>,
) -> Vec<Route> {
    let geometry = Geometry::ranked_by(markets, time, limits.objective);
    let mut rescored: Vec<Route> = routes
        .into_iter()
        .filter_map(|route| reprice(&geometry, markets, ship, limits, &route))
        .collect();
    // Descending: the ranking key orders better routes greater.
    rescored.sort_by(|a, b| b.rank.cmp(&a.rank));
    rescored.truncate(limits.top_n);
    rescored
}

/// One route, re-priced. `None` when a leg no longer trades.
fn reprice(
    geometry: &Geometry<'_>,
    markets: &[Market],
    ship: &ShipConfig,
    limits: &Limits,
    route: &Route,
) -> Option<Route> {
    let mut nodes = Vec::with_capacity(route.legs.len());
    let mut choices = Vec::with_capacity(route.legs.len());

    for leg in &route.legs {
        let from = markets.get(leg.from as usize)?;
        let to = markets.get(leg.to as usize)?;
        // The commodity is held fixed. Letting the rescore pick a different
        // commodity would be a fresh search over one pair, which is a different
        // and much weaker thing than re-pricing the route that was found — and
        // it would silently change what the row says you are carrying.
        let supply = from
            .supply
            .iter()
            .find(|row| row.commodity == leg.choice.commodity)?;
        let demand = to
            .demand
            .iter()
            .find(|row| row.commodity == leg.choice.commodity)?;
        let choice = leg_weight(supply, demand, ship, ship.credits, limits.min_units)?;
        if choice.profit <= limits.min_profit {
            return None;
        }
        nodes.push(leg.from);
        choices.push(choice);
    }

    let mut priced = match route.kind {
        RouteKind::SingleHop => {
            let leg = route.legs.first()?;
            Route::single_hop(geometry, leg.from, leg.to, *choices.first()?)
        }
        RouteKind::RoundTrip | RouteKind::Loop { .. } => {
            Route::cycle(geometry, &nodes, &choices)
        }
    };
    priced.mark_rescored();
    Some(priced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{limits, market, ship};
    use crate::report::{Guarantee, HeuristicReason};

    const GOLD: u32 = 1;
    const SILVER: u32 = 2;

    /// Buy gold at 1,000 from market 1, sell it at `sell` in market 2.
    fn instance(sell: i64) -> Vec<Market> {
        vec![
            market(1, 0.0, &[(GOLD, 1_000, 10_000)], &[]),
            market(2, 10.0, &[], &[(GOLD, sell, 10_000)]),
        ]
    }

    fn solve_one(markets: &[Market]) -> Vec<Route> {
        crate::solve(
            markets,
            TimeModel::default(),
            &ship(),
            &limits(),
            crate::Wanted::only(RouteKind::SingleHop),
            crate::watch::Watch::unlimited(),
        )
        .single
    }

    fn rescored(markets: &[Market], routes: Vec<Route>) -> Vec<Route> {
        rescore(markets, TimeModel::default(), &ship(), &limits(), routes)
    }

    #[test]
    fn a_route_repriced_against_an_unchanged_instance_keeps_its_profit() {
        let markets = instance(5_000);
        let found = solve_one(&markets);
        assert_eq!(found.len(), 1);
        let before = found[0].profit;
        let after = rescored(&markets, found);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].profit, before);
    }

    /// The point of the whole exercise: a price that has fallen since the
    /// search must show up as a smaller number, not the number we hoped for.
    #[test]
    fn a_fallen_price_demotes_the_route() {
        let found = solve_one(&instance(5_000));
        let optimistic = found[0].profit;
        let after = rescored(&instance(1_500), found);
        assert_eq!(after.len(), 1);
        assert!(
            after[0].profit < optimistic,
            "{:?} should be below {optimistic:?}",
            after[0].profit
        );
    }

    /// A leg that stopped trading is not a cheaper route, it is not a route.
    #[test]
    fn a_route_whose_leg_stopped_trading_is_dropped() {
        let found = solve_one(&instance(5_000));
        let gone = vec![
            market(1, 0.0, &[(GOLD, 1_000, 10_000)], &[]),
            market(2, 10.0, &[], &[(SILVER, 5_000, 10_000)]),
        ];
        assert!(rescored(&gone, found).is_empty());
    }

    /// Rescoring re-prices; it must not re-choose. Silver becoming the better
    /// trade between the same two markets does not change what this route says
    /// you are carrying.
    #[test]
    fn rescoring_holds_the_commodity_fixed() {
        let found = solve_one(&instance(5_000));
        let both = vec![
            market(
                1,
                0.0,
                &[(GOLD, 1_000, 10_000), (SILVER, 10, 10_000)],
                &[],
            ),
            market(
                2,
                10.0,
                &[],
                &[(GOLD, 5_000, 10_000), (SILVER, 90_000, 10_000)],
            ),
        ];
        let after = rescored(&both, found);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].legs[0].choice.commodity.0, GOLD);
    }

    /// The optimality claim cannot survive a re-price: the search compared this
    /// route against edges that were priced differently.
    #[test]
    fn rescoring_downgrades_the_guarantee() {
        let markets = instance(5_000);
        let found = solve_one(&markets);
        let rescored_reason = Guarantee::Heuristic {
            reason: HeuristicReason::RescoredAfterSearch,
        };
        assert_ne!(found[0].guarantee, rescored_reason);
        assert_eq!(rescored(&markets, found)[0].guarantee, rescored_reason);
    }

    /// Legs address markets by index, so the caller must patch in place. If a
    /// market is replaced by a different one at the same index the rescore
    /// prices the wrong station — this pins that it follows the index, which is
    /// what makes the in-place contract load-bearing.
    #[test]
    fn legs_follow_the_market_index_not_the_market_id() {
        let found = solve_one(&instance(5_000));
        let renumbered = vec![
            market(99, 0.0, &[(GOLD, 1_000, 10_000)], &[]),
            market(98, 10.0, &[], &[(GOLD, 5_000, 10_000)]),
        ];
        let after = rescored(&renumbered, found);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].legs[0].from, 0);
    }
}
