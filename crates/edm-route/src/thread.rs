//! Credit rethreading, and the re-ranking that has to follow it.
//!
//! The search prices every leg at the **starting** balance. That is a
//! conservative simplification and not an optimistic one — more credits can
//! only raise `units`, which can only raise profit — and it is what keeps the
//! weight matrix independent of where in a route you are, which is what makes
//! the graph algorithms valid at all.
//!
//! Finalists are then re-walked with the balance rising as cargo is sold. The
//! part that is easy to get wrong is what happens next: **rethreading changes
//! the ranking**. A route whose first leg is credit-starved gains more from
//! threading than one that was never limited, so the order after threading is
//! not the order before it. `edtrade/src/main.ts:239` threads the top-N *after*
//! truncating and never re-sorts, so its printed order is not monotone in the
//! number printed beside it. Here the shortlist handed to threading is wider
//! than the list finally printed — [`crate::model::Limits::shortlist_factor`]
//! times wider — and it is re-sorted afterwards, so a route that threading
//! promotes past the cut can actually arrive.
//!
//! # The case that is exactly optimal end to end
//!
//! If the balance can buy a full hold of the most expensive thing anywhere in
//! the instance, then `floor(credits / price) >= cargo` for every listing, the
//! credit cap never binds, and the searched objective and the threaded one are
//! *identically the same function*. The result is then `ProvedOptimal` rather
//! than `OptimalForStartingCredits` — a real strengthening rather than a hedge,
//! and cheap to check.
//!
//! The test has to be taken over the whole instance, not over the route's own
//! listings. The claim being upgraded is about the *search*, which priced every
//! edge in the graph; a route whose own legs happen to be affordable says
//! nothing about the edges the search compared it against.

use crate::model::{FillPolicy, Limits, Market, ShipConfig, Supply};
use crate::num::{Credits, Ratio};
use crate::report::{Guarantee, HeuristicReason, Route, RouteKind, Threaded};
use crate::weight::{FillPlan, greedy_fill, leg_weight};

/// Re-evaluates finished routes with credits accumulating.
#[derive(Clone, Copy, Debug)]
pub struct Threading<'a> {
    /// The instance the routes were found in.
    pub markets: &'a [Market],
    /// The ship they were planned for.
    pub ship: &'a ShipConfig,
    /// The limits the search ran under.
    pub limits: &'a Limits,
}

impl Threading<'_> {
    /// Threads, re-ranks and truncates a shortlist to the reported top-N.
    #[must_use]
    pub fn rethread(&self, mut routes: Vec<Route>) -> Vec<Route> {
        let unbound = self.credit_cap_never_binds();
        for route in &mut routes {
            let threaded = self.walk(route);
            route.set_threaded(threaded);
            if unbound && route.guarantee == Guarantee::OptimalForStartingCredits {
                route.guarantee = Guarantee::ProvedOptimal;
            }
            if threaded.greedy_fill {
                route.guarantee = Guarantee::Heuristic {
                    reason: HeuristicReason::MultiCommodityFill,
                };
            }
        }
        // Descending: the ranking key orders better routes greater.
        routes.sort_by(|a, b| b.rank.cmp(&a.rank));
        routes.truncate(self.limits.top_n);
        routes
    }

    /// Whether the balance can buy a full hold of the dearest listing anywhere.
    #[must_use]
    pub fn credit_cap_never_binds(&self) -> bool {
        let dearest = self
            .markets
            .iter()
            .flat_map(|market| &market.supply)
            .map(|row| row.buy_price)
            .fold(
                Credits::ZERO,
                |most, price| if price > most { price } else { most },
            );
        if dearest.0 <= 0 {
            return true;
        }
        // `cargo * dearest` is at most 2^15 * 2^20, comfortably inside the type.
        self.ship.credits.0 >= self.ship.cargo.0 * dearest.0
    }

    fn walk(&self, route: &Route) -> Threaded {
        let mut credits = self.ship.credits;
        let mut profit = Credits::ZERO;

        for leg in &route.legs {
            let earned = match self.limits.fill {
                FillPolicy::BestCommodity => self.single(leg.from, leg.to, leg, credits),
                FillPolicy::GreedyFill => self.greedy(leg.from, leg.to, credits).profit,
            };
            profit += earned;
            credits += earned;
        }

        let steady = match route.kind {
            RouteKind::SingleHop => None,
            RouteKind::RoundTrip | RouteKind::Loop { .. } => {
                Some(Ratio::new(profit, route.cycle_millis))
            }
        };
        Threaded {
            profit,
            steady,
            greedy_fill: self.limits.fill == FillPolicy::GreedyFill,
        }
    }

    /// The leg's own trade, re-priced at the balance in hand.
    ///
    /// The commodity is not re-chosen. A richer balance could make a different
    /// commodity the argmax, but re-choosing would report a route the search
    /// never evaluated, and a report has to describe the thing that was ranked.
    fn single(
        &self,
        from: u32,
        to: u32,
        leg: &crate::report::RouteLeg,
        credits: Credits,
    ) -> Credits {
        let commodity = leg.choice.commodity;
        let Some(supply) = self.markets[from as usize]
            .supply
            .iter()
            .find(|row| row.commodity == commodity)
        else {
            return leg.choice.profit;
        };
        let Some(demand) = self.markets[to as usize]
            .demand
            .iter()
            .find(|row| row.commodity == commodity)
        else {
            return leg.choice.profit;
        };
        leg_weight(supply, demand, self.ship, credits, self.limits.min_units)
            .map_or(leg.choice.profit, |choice| choice.profit)
    }

    fn greedy(&self, from: u32, to: u32, credits: Credits) -> FillPlan {
        let wanted = &self.markets[to as usize].demand;
        let pairs: Vec<(Supply, crate::model::Demand)> = self.markets[from as usize]
            .supply
            .iter()
            .filter_map(|supply| {
                let demand = wanted
                    .iter()
                    .find(|row| row.commodity == supply.commodity)?;
                Some((*supply, *demand))
            })
            .collect();
        greedy_fill(&pairs, self.ship, credits, self.limits.min_units)
    }
}

#[cfg(test)]
mod tests {
    use super::Threading;
    use crate::fixture::{limits, market};
    use crate::model::{Limits, ShipConfig};
    use crate::num::{Credits, Tons};
    use crate::report::Guarantee;

    fn instance() -> Vec<crate::model::Market> {
        vec![
            market(1, 0.0, &[(0, 100, 1_000)], &[(1, 900, 1_000)]),
            market(2, 5.0, &[(1, 100, 1_000)], &[(0, 900, 1_000)]),
        ]
    }

    fn solved(ship: ShipConfig, limits: Limits) -> crate::Solution {
        crate::solve(
            &instance(),
            crate::time::TimeModel::default(),
            &ship,
            &limits,
            crate::Wanted::all(),
            crate::watch::Watch::unlimited(),
        )
    }

    #[test]
    fn threading_never_lowers_a_routes_profit() {
        let ship = ShipConfig {
            cargo: Tons(100),
            credits: Credits(20_000),
        };
        let solution = solved(ship, limits());
        let route = &solution.round_trip[0];
        let threaded = route.threaded.expect("a threaded evaluation");
        assert!(
            threaded.profit >= route.profit,
            "{threaded:?} vs {:?}",
            route.profit
        );
    }

    #[test]
    fn a_binding_credit_cap_is_relaxed_by_the_second_leg() {
        // 100 tons of hold, but only enough credits for 50 tons at 100 each.
        // The first leg is credit-limited; by the second the balance is not.
        let ship = ShipConfig {
            cargo: Tons(100),
            credits: Credits(5_000),
        };
        let solution = solved(ship, limits());
        let route = &solution.round_trip[0];
        assert_eq!(route.legs[0].choice.units, Tons(50));
        let threaded = route.threaded.expect("a threaded evaluation");
        assert!(threaded.profit > route.profit);
        assert_ne!(route.guarantee, Guarantee::ProvedOptimal);
    }

    #[test]
    fn an_unbindable_credit_cap_upgrades_the_guarantee_and_changes_nothing_else() {
        let ship = ShipConfig {
            cargo: Tons(100),
            credits: Credits(1_000_000_000),
        };
        let solution = solved(ship, limits());
        let route = &solution.round_trip[0];
        assert_eq!(route.guarantee, Guarantee::ProvedOptimal);
        assert_eq!(route.threaded.expect("threaded").profit, route.profit);
    }

    #[test]
    fn a_greedily_filled_hold_forfeits_the_guarantee() {
        // The searched objective moves one commodity; filling the rest of the
        // hold with a second is a two-resource knapsack under a credit cap, and
        // no bound for it was found. The extra profit is real and it is
        // reported — but not as an optimum.
        let ship = ShipConfig {
            cargo: Tons(1_000),
            credits: Credits(1_000_000_000),
        };
        let limits = Limits {
            fill: crate::model::FillPolicy::GreedyFill,
            ..limits()
        };
        let solution = solved(ship, limits);
        let route = &solution.round_trip[0];
        assert_eq!(
            route.rate().guarantee,
            Guarantee::Heuristic {
                reason: crate::report::HeuristicReason::MultiCommodityFill
            }
        );
        assert!(route.threaded.expect("threaded").greedy_fill);
    }

    #[test]
    fn the_cap_test_is_taken_over_the_whole_instance() {
        let markets = instance();
        let ship = ShipConfig {
            cargo: Tons(100),
            credits: Credits(10_000),
        };
        let limits = limits();
        let threading = Threading {
            markets: &markets,
            ship: &ship,
            limits: &limits,
        };
        // 100 tons at 100 credits is exactly 10,000: the cap is reachable but
        // does not bind.
        assert!(threading.credit_cap_never_binds());
        let poorer = ShipConfig {
            cargo: Tons(100),
            credits: Credits(9_999),
        };
        let threading = Threading {
            markets: &markets,
            ship: &poorer,
            limits: &limits,
        };
        assert!(!threading.credit_cap_never_binds());
    }
}
