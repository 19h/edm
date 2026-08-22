//! Exact single hop, by branch and bound.
//!
//! Naive pairing is markets × markets × commodities. The saving structure is
//! that both sides of a commodity pool arrive price-sorted and prices cluster
//! tightly, so a good incumbent prunes nearly everything. The three bounds are
//! the same ones the graph build uses, but here they run against the incumbent
//! heap rather than against a fixed floor, so they tighten as the search
//! proceeds.
//!
//! One deliberate difference from the sibling implementation: a candidate whose
//! bound *equals* the incumbent's rate is **not** pruned. The ranking key is a
//! total order ending in an absolute tie-break, so two routes at the same rate
//! are still ordered, and a candidate that ties on rate can still displace the
//! incumbent on profit, on time, or finally on station id. Pruning on `<=`
//! would make the reported ranking depend on the order the instance arrived in,
//! which is the one thing the total order exists to prevent.
//!
//! A hop is identified by its cargo as well as its endpoints. The same pair of
//! stations can appear twice in the ranking carrying different commodities,
//! and that is not a duplicate: the two trades have different stock behind
//! them and deplete differently, so a commander choosing between them is
//! choosing between two real options. The graph, whose edges must be single
//! numbers, keeps only the better of the two.
//!
//! A single hop is ranked by its first-lap rate — profit over the time to
//! reach, load, fly and sell — but that figure is not its steady-state rate,
//! because there is no steady state. The route says so in its caveats and its
//! steady rate is absent rather than optimistic.

use crate::graph::Pools;
use crate::model::{Limits, ShipConfig};
use crate::num::{Credits, Millis, Ratio, Tons};
use crate::report::{RankKey, Route};
use crate::time::Geometry;
use crate::topn::TopN;
use crate::weight::{affordable, leg_weight};

/// The best single hops, best first.
///
/// Returns up to `top_n * shortlist_factor` routes: credit rethreading re-ranks
/// afterwards, so the list handed to it has to be wider than the list finally
/// printed or a route that threading promotes could never reach the page.
#[must_use]
pub fn solve(
    pools: &Pools,
    geometry: &Geometry<'_>,
    ship: &ShipConfig,
    limits: &Limits,
) -> Vec<Route> {
    let capacity = limits.top_n.saturating_mul(limits.shortlist_factor.max(1));
    let mut heap: TopN<RankKey, Route> = TopN::new(capacity);
    // The shortest wall-clock any one-hop route can take: a zero-distance hop
    // to a station at its star, plus the approach to the first one. The
    // denominator of a rate is bounded below by this, so dividing a profit
    // bound by it cannot underestimate the achievable rate and cannot prune a
    // winner.
    let floor = geometry.time.min_lap_millis(1);
    let markets = geometry.markets;

    let mut ordered: Vec<(usize, Credits)> = pools
        .pools
        .iter()
        .enumerate()
        .map(|(i, pool)| {
            let bound = match (pool.suppliers.first(), pool.buyers.first()) {
                (Some(cheapest), Some(dearest)) => {
                    let spread = dearest.row.sell_price - cheapest.row.buy_price;
                    if spread.0 <= 0 {
                        Credits::ZERO
                    } else {
                        spread
                            * min_tons(ship.cargo, affordable(ship.credits, cheapest.row.buy_price))
                    }
                }
                _ => Credits::ZERO,
            };
            (i, bound)
        })
        .collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    for (index, bound) in ordered {
        if bound <= limits.min_profit {
            break;
        }
        // Bound 0. Pools are in descending bound order, so once one cannot beat
        // the incumbent, none can.
        if cannot_beat(bound, floor, heap.worst()) {
            break;
        }
        let pool = &pools.pools[index];
        let Some(best_buyer) = pool.buyers.first() else {
            continue;
        };
        let best_sell = best_buyer.row.sell_price;

        for supplier in &pool.suppliers {
            let buyable = min_tons(ship.cargo, affordable(ship.credits, supplier.row.buy_price));
            let outer = (best_sell - supplier.row.buy_price) * buyable;
            // Bound 1, over the supply sort. `stock` is excluded: it is not
            // monotone in the sort, so a bound that used it would not be
            // non-increasing and this `break` would be unsound.
            if outer <= limits.min_profit || cannot_beat(outer, floor, heap.worst()) {
                break;
            }

            let units_cap = min_tons(buyable, supplier.row.stock);
            // Stock is not monotone in the sort, so a thin row is skipped and
            // the scan continues.
            if units_cap < limits.min_units {
                continue;
            }

            for buyer in &pool.buyers {
                let inner = (buyer.row.sell_price - supplier.row.buy_price) * units_cap;
                // Bound 2, over the demand sort.
                if inner <= limits.min_profit || cannot_beat(inner, floor, heap.worst()) {
                    break;
                }
                if buyer.node == supplier.node {
                    continue;
                }
                if limits.exclude_same_system
                    && markets[buyer.node as usize].system_address
                        == markets[supplier.node as usize].system_address
                {
                    continue;
                }

                let Some(choice) = leg_weight(
                    &supplier.row,
                    &buyer.row,
                    ship,
                    ship.credits,
                    limits.min_units,
                ) else {
                    continue;
                };
                if choice.profit <= limits.min_profit {
                    continue;
                }
                let route = Route::single_hop(geometry, supplier.node, buyer.node, choice);
                heap.offer(route.rank.clone(), route);
            }
        }
    }

    heap.drain()
}

/// Whether a profit bound cannot reach the incumbent's rate.
///
/// Strictly less than, never less than or equal: see the module note.
fn cannot_beat(bound: Credits, floor: Millis, worst: Option<&RankKey>) -> bool {
    let Some(worst) = worst else { return false };
    Ratio::new(bound, floor) < worst.rate
}

fn min_tons(a: Tons, b: Tons) -> Tons {
    if a < b { a } else { b }
}

#[cfg(test)]
mod tests {
    use super::solve;
    use crate::fixture::{geometry, limits, market, ship};
    use crate::graph::Pools;

    #[test]
    fn the_nearest_of_two_equally_profitable_hops_wins() {
        let markets = [
            market(1, 0.0, &[(0, 100, 1_000)], &[]),
            market(2, 5.0, &[], &[(0, 200, 1_000)]),
            market(3, 500.0, &[], &[(0, 200, 1_000)]),
        ];
        let routes = solve(
            &Pools::from_markets(&markets),
            &geometry(&markets),
            &ship(),
            &limits(),
        );
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].legs[0].to, 1);
        assert_eq!(routes[0].profit, routes[1].profit);
    }

    #[test]
    fn a_single_hop_reports_no_steady_rate() {
        let markets = [
            market(1, 0.0, &[(0, 100, 1_000)], &[]),
            market(2, 5.0, &[], &[(0, 200, 1_000)]),
        ];
        let routes = solve(
            &Pools::from_markets(&markets),
            &geometry(&markets),
            &ship(),
            &limits(),
        );
        assert!(routes[0].rate().steady.is_none());
    }
}
