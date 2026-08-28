//! Planning the disposal of a hold that is already bought \[C41\].
//!
//! A different question from the rest of this crate. Everywhere else the
//! problem is *what should I buy and where should I sell it*; here the cargo
//! exists, its cost is sunk, and the only decisions left are **which buyers to
//! visit, in what order, and how much to leave at each**.
//!
//! Three structural differences from a trade route, and each one makes this
//! easier rather than harder:
//!
//! - **There is no purse.** The credit cap is what turns a multi-commodity fill
//!   into a two-resource knapsack with no usable bound — the reason
//!   `weight::greedy_fill` can only claim `MultiCommodityFill`. A disposal
//!   spends nothing, so that coupling is simply absent.
//! - **Revenue does not depend on the order.** Each market is visited once and
//!   its published demand does not move between stops, so the allocation and
//!   the ordering separate completely: allocate over a *set*, then order the
//!   set.
//! - **Nothing repeats.** There is no lap and no steady state, so the approach
//!   from where the ship is standing is part of the cost rather than a one-off
//!   amortised away. That is a deliberate reversal of C40, which keeps the
//!   approach out of a *rate* because a rate is per lap.
//!
//! **What is exact and what is not.** Given the candidate set and the prices
//! handed in, the allocation and the ordering are both provably optimal — the
//! allocation by an exchange argument (below), the ordering by enumeration. The
//! heuristic is the *candidate set*, which arrives already trimmed by Ardent's
//! row cap and by `--top`. So the caller claims `NodesCapped`, and never
//! optimality over the region.
//!
//! **Why the objective is `credits − λ·time` and not credits per hour.** A
//! disposal is a finite task, and maximising a rate over a finite task pays you
//! to stop early: eight hundred tons in nineteen minutes beats twelve hundred
//! in forty-one on credits per hour, while leaving four hundred aboard. Rate is
//! the right objective for a loop precisely because a loop has no end.
//! Maximising `W − λ·T` is instead the literal form of "is the extra stop worth
//! the flight": take it exactly when `Δcredits > λ·Δtime`. Setting λ to the
//! incumbent plan's own rate and iterating is Dinkelbach, and it converges back
//! to the rate objective — which is why that mode is not offered separately. It
//! is the fixed point of this one.

use std::collections::HashMap;

use crate::model::{CommodityId, DemandQty, Market};
use crate::num::{Credits, Millis, Ratio, Tons};
use crate::time::Geometry;
use crate::weight::effective_demand;
use edm_core::domain::id64::Coordinates;

/// One commodity's worth of clean, sellable cargo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    pub commodity: CommodityId,
    pub tons: Tons,
}

/// What the plan leaves at one stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drop {
    pub commodity: CommodityId,
    pub tons: Tons,
    pub unit_price: Credits,
    pub credits: Credits,
    /// The destination published a bracket and no tonnage, so its capacity was
    /// assumed rather than read.
    pub demand_assumed: bool,
}

/// One stop on a disposal, and everything sold there.
///
/// No distance field, deliberately: this module is on the exactness gate's
/// solving path and holds no floating point at all. The light years are a
/// display quantity and the renderer asks `Geometry::ly_from` for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stop {
    /// Index into the market slice the plan was built against.
    pub market: u32,
    pub drops: Vec<Drop>,
    /// Travel from the previous stop, or from the ship for the first.
    pub millis: Millis,
}

impl Stop {
    #[must_use]
    pub fn credits(&self) -> Credits {
        Credits(self.drops.iter().map(|drop| drop.credits.0).sum())
    }

    #[must_use]
    pub fn tons(&self) -> Tons {
        Tons(self.drops.iter().map(|drop| drop.tons.0).sum())
    }
}

/// A complete disposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub stops: Vec<Stop>,
    pub revenue: Credits,
    pub millis: Millis,
    pub sold: Tons,
    /// What stays aboard, by commodity, when no chosen stop would take it.
    pub unsold: Vec<Held>,
}

impl Plan {
    /// Credits per hour over the whole disposal, for display only.
    ///
    /// Never the ranking key — see the module note. A finite task ranked by its
    /// own rate pays the commander to stop early.
    #[must_use]
    pub fn rate(&self) -> Ratio {
        Ratio::new(self.revenue, self.millis)
    }

    /// The objective: `λ.millis·revenue − λ.credits·time`, in `i128`.
    ///
    /// Reuses `ratio::reduced`, which is the same arithmetic the loop search
    /// uses to ask whether an edge beats a rate. Compared as an integer and
    /// never divided.
    #[must_use]
    pub fn score(&self, worth: Ratio) -> i128 {
        crate::ratio::reduced(worth, self.revenue, self.millis)
    }

    /// Market ids, for the caller's verification pass.
    #[must_use]
    pub fn market_ids(&self, markets: &[Market]) -> Vec<i64> {
        self.stops
            .iter()
            .filter_map(|stop| markets.get(stop.market as usize))
            .map(|market| market.market_id)
            .collect()
    }
}

/// How many ordered paths the search may enumerate before it refuses.
///
/// A bound the caller can see and act on, not a silent truncation: the refusal
/// names the count and the three flags that reduce it. At `--stops 3` this
/// admits 216 candidates, at `--stops 4` fifty-seven — and in practice the
/// spend gate caps the candidate set long before this does, because every
/// candidate costs one authenticated read to verify.
pub const MAX_ORDERED_PATHS: u128 = 10_000_000;

/// Why a search would not be run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooManyPaths {
    pub candidates: usize,
    pub stops: usize,
    pub paths: u128,
}

/// How many ordered paths `candidates` and `stops` would enumerate.
#[must_use]
pub fn ordered_paths(candidates: usize, stops: usize) -> u128 {
    let mut total: u128 = 0;
    let mut falling: u128 = 1;
    for k in 1..=stops {
        let Some(next) = (candidates as u128).checked_sub(k as u128 - 1) else {
            break;
        };
        falling = falling.saturating_mul(next);
        total = total.saturating_add(falling);
        if total > MAX_ORDERED_PATHS {
            return total;
        }
    }
    total
}

/// Allocate a hold across a fixed set of markets, optimally.
///
/// **Optimal, not greedy-in-the-pejorative-sense.** Within one commodity the
/// revenue at a market is linear in tons — every ton fetches the same
/// `sell_price` — and the markets' capacities are independent. Filling the
/// dearest buyer first is then the fractional-knapsack exchange argument on
/// integral tons at integral prices: any allocation that puts a ton in a
/// cheaper market when a dearer one still has room can be improved by swapping
/// it, so no such allocation is optimal. Commodities do not interact, because a
/// `Market` holds one `Demand` per `CommodityId` and a disposal has no shared
/// budget to contend over.
///
/// Returns the per-market drops in the order given, and whatever the set could
/// not absorb.
#[must_use]
fn allocate(hold: &[Held], markets: &[Market], chosen: &[u32]) -> (Vec<Vec<Drop>>, Vec<Held>) {
    let mut drops: Vec<Vec<Drop>> = vec![Vec::new(); chosen.len()];
    let mut unsold: Vec<Held> = Vec::new();

    for held in hold {
        // Dearest first. Ties break on the market's position in the chosen set,
        // so an identical input always produces an identical plan.
        let mut offers: Vec<(usize, Credits, DemandQty)> = chosen
            .iter()
            .enumerate()
            .filter_map(|(slot, index)| {
                let row = markets
                    .get(*index as usize)?
                    .demand
                    .iter()
                    .find(|row| row.commodity == held.commodity)?;
                Some((slot, row.sell_price, row.qty))
            })
            .collect();
        offers.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));

        let mut remaining = held.tons;
        for (slot, price, qty) in offers {
            if remaining.0 <= 0 {
                break;
            }
            let (capacity, assumed) = effective_demand(qty, remaining);
            let tons = Tons(remaining.0.min(capacity.0));
            if tons.0 <= 0 {
                continue;
            }
            drops[slot].push(Drop {
                commodity: held.commodity,
                tons,
                unit_price: price,
                credits: Credits(tons.0 * price.0),
                demand_assumed: assumed,
            });
            remaining = Tons(remaining.0 - tons.0);
        }
        if remaining.0 > 0 {
            unsold.push(Held {
                commodity: held.commodity,
                tons: remaining,
            });
        }
    }
    (drops, unsold)
}

/// Time to fly an ordered set of stops, starting from the ship.
fn tour_millis(
    geometry: &Geometry<'_>,
    from: Coordinates,
    order: &[u32],
) -> Option<(Millis, Vec<Millis>)> {
    let mut legs = Vec::with_capacity(order.len());
    let mut total: i64 = 0;
    let mut at = from;
    for index in order {
        let market = geometry.markets.get(*index as usize)?;
        let millis = geometry.millis_from(at, *index);
        total = total.checked_add(millis.0)?;
        legs.push(millis);
        at = market.coords;
    }
    Some((Millis(total), legs))
}

/// Build the plan for one ordered set of stops, dropping stops that get nothing.
#[must_use]
fn plan_for(
    geometry: &Geometry<'_>,
    from: Coordinates,
    hold: &[Held],
    order: &[u32],
) -> Option<Plan> {
    let (drops, unsold) = allocate(hold, geometry.markets, order);
    // A stop that would be flown to and sold nothing is not part of the plan.
    // Keeping it would charge its travel against the objective and make the
    // search prefer shorter sets for the wrong reason.
    let kept: Vec<u32> = order
        .iter()
        .zip(&drops)
        .filter(|(_, drops)| !drops.is_empty())
        .map(|(index, _)| *index)
        .collect();
    if kept.is_empty() {
        return None;
    }
    let kept_drops: Vec<Vec<Drop>> = drops.into_iter().filter(|d| !d.is_empty()).collect();

    let (millis, legs) = tour_millis(geometry, from, &kept)?;
    let stops: Vec<Stop> = kept
        .iter()
        .zip(kept_drops)
        .zip(legs)
        .map(|((index, drops), millis)| Stop {
            market: *index,
            drops,
            millis,
        })
        .collect();

    let revenue = Credits(stops.iter().map(|stop| stop.credits().0).sum());
    let sold = Tons(stops.iter().map(|stop| stop.tons().0).sum());
    Some(Plan {
        stops,
        revenue,
        millis,
        sold,
        unsold,
    })
}

/// Every plan worth showing, best first at `worth`.
///
/// The set is enumerated, the allocation is exact for each set, and the order
/// within a set is chosen by enumeration too — so the only approximation is
/// which candidates were handed in.
///
/// `worth` is the exchange rate between credits and time: a stop joins the plan
/// exactly when it earns more than `worth` for the time it costs.
pub fn plans(
    geometry: &Geometry<'_>,
    from: Coordinates,
    hold: &[Held],
    candidates: &[u32],
    stops: usize,
    worth: Ratio,
) -> Result<Vec<Plan>, TooManyPaths> {
    let paths = ordered_paths(candidates.len(), stops);
    if paths > MAX_ORDERED_PATHS {
        return Err(TooManyPaths {
            candidates: candidates.len(),
            stops,
            paths,
        });
    }

    // Keyed by the *set* of markets actually sold at, so two orderings of the
    // same set keep only the faster one and the caller never sees a plan twice.
    // A hash map, and the iteration order it does not promise is harmless: the
    // sort below ends in a total tie-break on the market ids themselves, so the
    // returned order is a function of the plans and not of how they were stored.
    let mut best: HashMap<Vec<u32>, Plan> = HashMap::new();
    let mut order: Vec<u32> = Vec::with_capacity(stops);
    walk(
        geometry, from, hold, candidates, stops, &mut order, &mut best,
    );

    let mut found: Vec<Plan> = best.into_values().collect();
    found.sort_by(|a, b| {
        b.score(worth)
            .cmp(&a.score(worth))
            .then(b.revenue.0.cmp(&a.revenue.0))
            .then(a.millis.0.cmp(&b.millis.0))
            .then_with(|| {
                let ids = |plan: &Plan| plan.stops.iter().map(|s| s.market).collect::<Vec<_>>();
                ids(a).cmp(&ids(b))
            })
    });
    Ok(found)
}

fn walk(
    geometry: &Geometry<'_>,
    from: Coordinates,
    hold: &[Held],
    candidates: &[u32],
    stops: usize,
    order: &mut Vec<u32>,
    best: &mut HashMap<Vec<u32>, Plan>,
) {
    if !order.is_empty()
        && let Some(plan) = plan_for(geometry, from, hold, order)
    {
        let mut key: Vec<u32> = plan.stops.iter().map(|stop| stop.market).collect();
        key.sort_unstable();
        // Same set, different order: keep whichever is quicker. Revenue is
        // order-independent, so time is the only thing that can differ.
        match best.get(&key) {
            Some(incumbent) if incumbent.millis.0 <= plan.millis.0 => {}
            _ => {
                best.insert(key, plan);
            }
        }
    }
    if order.len() == stops {
        return;
    }
    for candidate in candidates {
        if order.contains(candidate) {
            continue;
        }
        order.push(*candidate);
        walk(geometry, from, hold, candidates, stops, order, best);
        order.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::market;
    use crate::time::TimeModel;

    const TRITIUM: u32 = 0;
    const GOLD: u32 = 1;

    /// Where the ship is standing, at the origin. Written as a constant rather
    /// than a helper because this module is on the exactness gate's solving
    /// path and may not name a float type anywhere, tests included.
    const SHIP: Coordinates = Coordinates {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    fn held(commodity: u32, tons: i64) -> Held {
        Held {
            commodity: CommodityId(commodity),
            tons: Tons(tons),
        }
    }

    /// Three buyers: near and cheap, mid and dear but small, far and dearest.
    fn markets() -> Vec<Market> {
        vec![
            market(1, 10.0, &[], &[(TRITIUM, 40_000, 10_000)]),
            market(2, 20.0, &[], &[(TRITIUM, 50_000, 400)]),
            market(3, 400.0, &[], &[(TRITIUM, 60_000, 10_000)]),
        ]
    }

    fn worth(credits_per_hour: i64) -> Ratio {
        Ratio::new(Credits(credits_per_hour), Millis(3_600_000))
    }

    fn plan_at(bar: i64, stops: usize) -> Plan {
        let markets = markets();
        let geometry = Geometry::new(&markets, TimeModel::default());
        plans(
            &geometry,
            SHIP,
            &[held(TRITIUM, 1_232)],
            &[0, 1, 2],
            stops,
            worth(bar),
        )
        .expect("within the bound")
        .into_iter()
        .next()
        .expect("a plan")
    }

    /// The exchange argument: the dearest buyer in the chosen set fills first.
    #[test]
    fn the_allocation_fills_the_dearest_buyer_first() {
        let markets = markets();
        let (drops, unsold) = allocate(&[held(TRITIUM, 1_232)], &markets, &[0, 1]);
        // Market 1 pays 50,000 but takes only 400; the rest goes to market 0.
        assert_eq!(drops[1][0].tons, Tons(400));
        assert_eq!(drops[0][0].tons, Tons(832));
        assert!(unsold.is_empty());
    }

    /// What no chosen market will take is reported, never silently dropped.
    #[test]
    fn cargo_no_chosen_market_will_take_is_reported_unsold() {
        let markets = markets();
        let (_, unsold) = allocate(&[held(TRITIUM, 1_232)], &markets, &[1]);
        assert_eq!(unsold, vec![held(TRITIUM, 832)]);
    }

    /// A commodity nobody in the set buys does not silently vanish.
    #[test]
    fn a_commodity_with_no_buyer_is_entirely_unsold() {
        let markets = markets();
        let (drops, unsold) = allocate(&[held(GOLD, 50)], &markets, &[0, 1, 2]);
        assert!(drops.iter().all(Vec::is_empty));
        assert_eq!(unsold, vec![held(GOLD, 50)]);
    }

    /// The whole point of the objective. A high bar refuses the 400 Ly hop to
    /// the dearest buyer; a low bar accepts it.
    #[test]
    fn the_bar_decides_whether_the_far_buyer_is_worth_it() {
        let patient = plan_at(1, 3);
        assert!(
            patient.stops.iter().any(|stop| stop.market == 2),
            "a bar of nothing should fly 400 Ly for 60,000 a ton"
        );

        let impatient = plan_at(500_000_000_000, 3);
        assert!(
            !impatient.stops.iter().any(|stop| stop.market == 2),
            "an enormous bar should refuse the 400 Ly hop: {:?}",
            impatient.stops.iter().map(|s| s.market).collect::<Vec<_>>()
        );
    }

    /// Rate would pay the commander to stop early; the objective must not.
    /// Selling 400 t at the dear nearby market is a better *rate* than clearing
    /// the hold, and a plan ranked by rate would take it.
    #[test]
    fn the_objective_does_not_reward_leaving_cargo_aboard() {
        let plan = plan_at(1, 3);
        assert_eq!(plan.sold, Tons(1_232), "the hold is cleared");
        assert!(plan.unsold.is_empty());
    }

    /// Two orderings of the same set differ only in time, so only the quicker
    /// survives — the caller never sees the same set twice.
    #[test]
    fn the_same_set_appears_once_at_its_quickest() {
        let markets = markets();
        let geometry = Geometry::new(&markets, TimeModel::default());
        let found = plans(
            &geometry,
            SHIP,
            &[held(TRITIUM, 1_232)],
            &[0, 1],
            2,
            worth(1),
        )
        .unwrap();
        let mut sets: Vec<Vec<u32>> = found
            .iter()
            .map(|plan| {
                let mut ids: Vec<u32> = plan.stops.iter().map(|s| s.market).collect();
                ids.sort_unstable();
                ids
            })
            .collect();
        let before = sets.len();
        sets.sort();
        sets.dedup();
        assert_eq!(sets.len(), before, "a set appeared twice: {sets:?}");
    }

    /// An unpublished demand is assumed to take what is offered, and says so —
    /// the same rule `effective_demand` applies everywhere else.
    #[test]
    fn an_unpublished_demand_is_assumed_and_flagged() {
        let mut markets = markets();
        markets[0].demand[0].qty = DemandQty::Unpublished;
        let (drops, _) = allocate(&[held(TRITIUM, 1_232)], &markets, &[0]);
        assert_eq!(drops[0][0].tons, Tons(1_232));
        assert!(drops[0][0].demand_assumed);
    }

    #[test]
    fn the_path_count_is_the_falling_factorial_sum() {
        // 20 + 20*19 + 20*19*18 = 20 + 380 + 6,840
        assert_eq!(ordered_paths(20, 3), 7_240);
        assert_eq!(ordered_paths(30, 4), 682_980);
        assert_eq!(ordered_paths(3, 1), 3);
    }

    /// The bound refuses rather than truncating, and says how big the ask was.
    #[test]
    fn an_oversized_search_is_refused_with_its_count() {
        let markets = markets();
        let geometry = Geometry::new(&markets, TimeModel::default());
        let candidates: Vec<u32> = (0..4_000).collect();
        let error = plans(
            &geometry,
            SHIP,
            &[held(TRITIUM, 10)],
            &candidates,
            3,
            worth(1),
        )
        .unwrap_err();
        assert_eq!(error.candidates, 4_000);
        assert_eq!(error.stops, 3);
        assert!(error.paths > MAX_ORDERED_PATHS);
    }

    /// A stop that would be flown to and sold nothing is not a stop.
    #[test]
    fn a_stop_that_sells_nothing_is_dropped_from_the_plan() {
        let markets = markets();
        let geometry = Geometry::new(&markets, TimeModel::default());
        // Only 400 t: market 1 alone absorbs it, so pairing it with market 0
        // must not produce a two-stop plan.
        let found = plans(
            &geometry,
            SHIP,
            &[held(TRITIUM, 400)],
            &[0, 1],
            2,
            worth(1),
        )
        .unwrap();
        assert!(
            found.iter().all(|plan| plan
                .stops
                .iter()
                .all(|stop| !stop.drops.is_empty())),
            "a stop sold nothing"
        );
    }
}
