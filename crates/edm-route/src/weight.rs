//! `w(a, b)` — the best profit obtainable from one laden hop.
//!
//! ```text
//! units  = min(cargo, stock_a, effective_demand_b, floor(credits / buyPrice_a))
//! profit = units * (sellPrice_b - buyPrice_a)
//! ```
//!
//! The binding cap is recorded rather than discarded. It is the most useful
//! diagnostic a route can carry: `Credits` means *come back richer*, `Stock`
//! means *this will not repeat*, `Demand` means *the other end fills up*.
//!
//! Every number in this module is an integer. Units floor, profit floors, and
//! nothing here evaluates a quotient except an exact flooring division.

use crate::model::{Demand, DemandQty, ShipConfig, Supply};
use crate::num::{Credits, Tons};

/// Which of the four caps decided how much cargo moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Limiter {
    /// The hold filled.
    Cargo,
    /// The seller ran out.
    Stock,
    /// The buyer filled up.
    Demand,
    /// The balance ran out.
    Credits,
}

/// The trade a leg actually performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegChoice {
    /// What is carried.
    pub commodity: crate::model::CommodityId,
    /// Paid per ton at the origin.
    pub buy_price: Credits,
    /// Received per ton at the destination.
    pub sell_price: Credits,
    /// Tons moved.
    pub units: Tons,
    /// Total profit, `units * (sell - buy)`.
    pub profit: Credits,
    /// Which cap bound.
    pub limiter: Limiter,
    /// True when the destination's quantity was assumed rather than published.
    pub demand_assumed: bool,
}

/// How much of a commodity a market will actually take.
///
/// See [`DemandQty`] for why the unpublished case is not a rounding error.
#[must_use]
pub fn effective_demand(qty: DemandQty, cargo: Tons) -> (Tons, bool) {
    match qty {
        DemandQty::Published(tons) => (tons, false),
        DemandQty::Unpublished => (cargo, true),
    }
}

/// How many tons a balance can buy at a price. Floors, and is never negative.
///
/// A non-positive price would make this unbounded and silently void the credit
/// constraint, so it yields nothing instead; ingest already rejects such rows,
/// and this is the second lock on the same door.
#[must_use]
pub fn affordable(credits: Credits, price: Credits) -> Tons {
    if price.0 <= 0 || credits.0 <= 0 {
        return Tons::ZERO;
    }
    Tons(credits.0 / price.0)
}

/// The four caps, resolved in a fixed order so the reported limiter is stable.
///
/// Order matters only for reporting: the minimum is the minimum whichever way
/// it is computed, but when two caps tie, the one named is the earlier of
/// cargo, stock, demand, credits. That makes the diagnostic a function of the
/// data alone and not of iteration order.
#[must_use]
pub fn trade_units(
    buy_price: Credits,
    stock: Tons,
    qty: DemandQty,
    ship: &ShipConfig,
    credits: Credits,
) -> (Tons, Limiter, bool) {
    let (wanted, assumed) = effective_demand(qty, ship.cargo);
    let by_credits = affordable(credits, buy_price);

    let mut units = ship.cargo;
    let mut limiter = Limiter::Cargo;
    if stock < units {
        units = stock;
        limiter = Limiter::Stock;
    }
    if wanted < units {
        units = wanted;
        limiter = Limiter::Demand;
    }
    if by_credits < units {
        units = by_credits;
        limiter = Limiter::Credits;
    }
    if units.0 < 0 {
        units = Tons::ZERO;
    }
    (units, limiter, assumed)
}

/// The trade a pair of rows supports, or `None` if it supports none.
///
/// `credits` is passed separately from `ship` so credit rethreading can
/// re-evaluate a finished leg at a later balance without rebuilding the ship.
#[must_use]
pub fn leg_weight(
    supply: &Supply,
    demand: &Demand,
    ship: &ShipConfig,
    credits: Credits,
    min_units: Tons,
) -> Option<LegChoice> {
    debug_assert_eq!(supply.commodity, demand.commodity);
    let margin = demand.sell_price - supply.buy_price;
    if margin.0 <= 0 {
        return None;
    }
    let (units, limiter, demand_assumed) =
        trade_units(supply.buy_price, supply.stock, demand.qty, ship, credits);
    if units < min_units || units.0 <= 0 {
        return None;
    }
    Some(LegChoice {
        commodity: supply.commodity,
        buy_price: supply.buy_price,
        sell_price: demand.sell_price,
        units,
        profit: margin * units,
        limiter,
        demand_assumed,
    })
}

/// A hold filled with more than one commodity.
///
/// Greedy by unit margin under two simultaneous resources — hold and balance —
/// which is a two-resource knapsack, and no bound for it was found. It is
/// therefore never used by the search: it re-evaluates finalists only, and
/// doing so downgrades the guarantee to `Heuristic`. Reported separately from
/// the searched objective so the two can never be confused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FillPlan {
    /// The commodities taken, best margin first.
    pub picks: Vec<LegChoice>,
    /// Their total profit.
    pub profit: Credits,
    /// Tons loaded.
    pub units: Tons,
}

/// Fills a hold greedily from the matched rows of one station pair.
///
/// `pairs` must be same-commodity `(supply, demand)` matches for one leg.
#[must_use]
pub fn greedy_fill(
    pairs: &[(Supply, Demand)],
    ship: &ShipConfig,
    credits: Credits,
    min_units: Tons,
) -> FillPlan {
    let mut ranked: Vec<(Credits, usize)> = pairs
        .iter()
        .enumerate()
        .map(|(i, (s, d))| (d.sell_price - s.buy_price, i))
        .filter(|(margin, _)| margin.0 > 0)
        .collect();
    // Descending margin, then by index so a tie is resolved by the caller's
    // order rather than by the sort's internal one.
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

    let mut hold_left = ship.cargo;
    let mut purse = credits;
    let mut picks = Vec::new();
    let mut profit = Credits::ZERO;
    let mut units_total = Tons::ZERO;

    for (_, i) in ranked {
        if hold_left.0 <= 0 || purse.0 <= 0 {
            break;
        }
        let (supply, demand) = &pairs[i];
        let remaining = ShipConfig { cargo: hold_left, credits: purse };
        let Some(mut choice) = leg_weight(supply, demand, &remaining, purse, min_units) else {
            continue;
        };
        let margin = demand.sell_price - supply.buy_price;
        choice.profit = margin * choice.units;
        hold_left = hold_left - choice.units;
        purse = purse - supply.buy_price * choice.units;
        profit += choice.profit;
        units_total = units_total + choice.units;
        picks.push(choice);
    }

    FillPlan { picks, profit, units: units_total }
}

#[cfg(test)]
mod tests {
    use super::{Limiter, affordable, greedy_fill, leg_weight, trade_units};
    use crate::model::{CommodityId, Demand, DemandQty, ShipConfig, Supply};
    use crate::num::{Credits, Tons};

    fn ship(cargo: i64, credits: i64) -> ShipConfig {
        ShipConfig { cargo: Tons(cargo), credits: Credits(credits) }
    }

    fn supply(price: i64, stock: i64) -> Supply {
        Supply { commodity: CommodityId(0), buy_price: Credits(price), stock: Tons(stock) }
    }

    fn demand(price: i64, qty: DemandQty) -> Demand {
        Demand { commodity: CommodityId(0), sell_price: Credits(price), qty }
    }

    #[test]
    fn affordability_floors() {
        assert_eq!(affordable(Credits(999), Credits(100)), Tons(9));
        assert_eq!(affordable(Credits(0), Credits(100)), Tons::ZERO);
        assert_eq!(affordable(Credits(999), Credits(0)), Tons::ZERO);
    }

    #[test]
    fn each_cap_is_named_when_it_binds() {
        let s = ship(100, 1_000_000);
        let cases = [
            (Tons(500), DemandQty::Published(Tons(500)), 1_000_000, Limiter::Cargo, 100),
            (Tons(30), DemandQty::Published(Tons(500)), 1_000_000, Limiter::Stock, 30),
            (Tons(500), DemandQty::Published(Tons(20)), 1_000_000, Limiter::Demand, 20),
            (Tons(500), DemandQty::Published(Tons(500)), 550, Limiter::Credits, 5),
        ];
        for (stock, qty, credits, expect, units) in cases {
            let (got, limiter, _) =
                trade_units(Credits(100), stock, qty, &s, Credits(credits));
            assert_eq!(limiter, expect);
            assert_eq!(got, Tons(units));
        }
    }

    #[test]
    fn unpublished_demand_is_cargo_limited_not_zero() {
        let s = ship(720, 1_000_000_000);
        let taken = leg_weight(
            &supply(1, 10_000),
            &demand(59_759, DemandQty::Unpublished),
            &s,
            s.credits,
            Tons(1),
        )
        .expect("an unpublished-demand row is still a buyer");
        assert_eq!(taken.units, Tons(720));
        assert!(taken.demand_assumed);
        assert_eq!(taken.profit, Credits(720 * 59_758));
    }

    #[test]
    fn a_non_positive_margin_is_not_a_leg() {
        let s = ship(100, 1_000_000);
        assert!(
            leg_weight(
                &supply(500, 100),
                &demand(500, DemandQty::Published(Tons(100))),
                &s,
                s.credits,
                Tons(1)
            )
            .is_none()
        );
    }

    #[test]
    fn greedy_fill_spends_the_hold_on_the_widest_margins_first() {
        let s = ship(100, 1_000_000_000);
        let pairs = [
            (
                Supply { commodity: CommodityId(0), buy_price: Credits(10), stock: Tons(60) },
                Demand {
                    commodity: CommodityId(0),
                    sell_price: Credits(1_010),
                    qty: DemandQty::Published(Tons(1_000)),
                },
            ),
            (
                Supply { commodity: CommodityId(1), buy_price: Credits(10), stock: Tons(1_000) },
                Demand {
                    commodity: CommodityId(1),
                    sell_price: Credits(110),
                    qty: DemandQty::Published(Tons(1_000)),
                },
            ),
        ];
        let plan = greedy_fill(&pairs, &s, s.credits, Tons(1));
        assert_eq!(plan.units, Tons(100));
        // 60 tons at 1000 margin, then 40 at 100.
        assert_eq!(plan.profit, Credits(60_000 + 4_000));
        assert_eq!(plan.picks[0].commodity, CommodityId(0));
    }
}
