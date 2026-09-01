//! What a solver returns: a route, the claim being made about the search that
//! found it, and the claims *not* being made about the model it was found in.
//!
//! > `Guarantee` is a claim about the search. `Caveat` is a claim about the
//! > model.
//!
//! They are orthogonal, and neither is allowed to be rendered without the
//! other. A `ProvedOptimal` result carrying four caveats is the honest answer:
//! proved optimal for this model, and here is what the model omits. To make
//! that structural rather than a convention, a route's rate is unreachable
//! except through [`Route::rate`], which hands back the guarantee and the
//! caveats alongside it — an unprovable answer cannot read as a proved one by
//! omission, because there is no accessor that would let it.

use crate::num::{Credits, Millis, Ratio};
use crate::time::Geometry;
use crate::weight::{LegChoice, Limiter};

/// What was searched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    /// One laden hop. Has no steady-state rate: repeating it means flying back
    /// empty.
    SingleHop,
    /// Out and back, with different cargo each way.
    RoundTrip,
    /// A closed cycle of `stops` distinct stations.
    Loop {
        /// How many stations the cycle visits.
        stops: usize,
    },
}

impl RouteKind {
    /// Whether the route returns to where it started.
    ///
    /// The distinction a renderer needs: a cycle's last destination is its
    /// first origin and so is not worth printing twice, while an open route's
    /// destination is the whole point of it.
    #[must_use]
    pub const fn is_cycle(self) -> bool {
        match self {
            Self::SingleHop => false,
            Self::RoundTrip | Self::Loop { .. } => true,
        }
    }
}

/// What is claimed about the search that produced a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guarantee {
    /// The best route that exists in the modelled instance, full stop. Reached
    /// when the credit cap provably never binds, so the searched objective and
    /// the realised one are identically the same function.
    ProvedOptimal,
    /// The best route for the objective the search used, which prices every leg
    /// at the *starting* balance. Threading credits through the route can only
    /// raise its profit, so this is a lower bound on the truth and never an
    /// overstatement — but a different route might have gained more.
    OptimalForStartingCredits,
    /// Search stopped early. Nothing better than `upper` exists, and the route
    /// in hand achieves its own rate; the gap between them is the residue.
    ///
    /// Stored as the upper bound rather than as a subtraction: the difference
    /// of two rates has the product of their denominators underneath it, which
    /// does not fit the numerator type, and both ends are more useful anyway.
    BoundedGap {
        /// A proved upper bound on any route of this shape.
        upper: Ratio,
    },
    /// No claim. The reason says which approximation was taken.
    Heuristic {
        /// Which approximation.
        reason: HeuristicReason,
    },
}

/// Why a result is only heuristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeuristicReason {
    /// The node set was capped before searching, so the search answered a
    /// question about a subgraph and not about the sweep.
    ///
    /// Nothing in this crate sets this, and that is deliberate: choosing which
    /// markets to consider is the caller's decision, made before a `Market` is
    /// ever handed over, and this crate answers exactly the question it is
    /// given. A caller that trims the node set applies this itself through
    /// [`Route::with_guarantee`]. The polynomial ratio-cycle solver is what
    /// makes running the whole graph affordable in the first place, so the trim
    /// is an escape hatch whose threshold has to come from measurement.
    NodesCapped,
    /// A route found on the way to the optimum, kept to populate the listing.
    /// Its own optimality was never established, only the head of the list's.
    RunnerUp,
    /// The hold was filled with more than one commodity, which is a
    /// two-resource knapsack under a binding credit cap.
    MultiCommodityFill,
    /// The branch and bound exhausted its node budget.
    SearchBudgetExhausted,
    /// Destination prices use the empirical cargo-quantity estimator.
    BulkPriceEstimate,
    /// The instance was re-priced after the search, so the ordering is correct
    /// for the routes that were rescored and says nothing about the rest.
    ///
    /// Nothing in this crate sets this either. It belongs to a caller that
    /// ranks on one set of prices, re-reads a few markets live, and rescores —
    /// which can only ever *demote* a route. A better route that the first
    /// ranking buried is not discovered by rescoring, so the claim "the best of
    /// these N" survives and the claim "the best N there are" does not.
    RescoredAfterSearch,
}

/// What the model does not know, independently of how well it was searched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Caveat {
    /// A leg is limited by the seller's stock, so a second lap buys less.
    StockDepletion,
    /// A destination publishes a bracket but no quantity, and was assumed to
    /// take a full hold.
    DemandUnpublished,
    /// Market observations are already ageing and may come from different instants.
    StaleListing,
    /// Destination price was estimated from cargo quantity and published demand.
    BulkPriceEstimated,
    /// A leg buys from a fleet carrier, whose stock is a fixed pot rather than
    /// a regenerating economy.
    ///
    /// The rate assumes a lap can be repeated. A station restocks; a carrier's
    /// shelf was filled by one commander and, once bought out, stays empty. The
    /// hop can still be the most profitable row in the table — it is simply a
    /// hop that can be flown once.
    CarrierSourceDoesNotRestock,
    /// At least one price in this route was read before this run and reused
    /// from the local cache rather than measured now.
    ///
    /// Distinct from [`Caveat::StaleListing`], which is unconditional and says
    /// only that prices age. This one says *this* route was not fully measured
    /// this run, and its absence therefore means something.
    PricedFromCache,
    /// Jump count is `ceil(ly / range)` on a straight line; the real galaxy has
    /// gaps, neutron boosts and fuel.
    JumpGraphUnmodelled,
    /// The starting balance bound at least one leg, so the route is optimal for
    /// that balance rather than in general.
    CreditCapBinds,
    /// A single hop has no steady rate — flying it repeatedly means
    /// deadheading back empty, which this figure does not charge for.
    SingleHopNotRepeatable,
    /// Landing pads, permits, allegiance and the black market are not modelled.
    AccessUnmodelled,
    /// The travel model is a calibrated approximation, not a measurement of
    /// your ship.
    TimeModelAssumed,
    /// Edges below the profit floor were removed, so the result is optimal over
    /// a stated subset of the legs rather than over all of them.
    EdgesBelowFloorDropped,
}

/// One laden hop of a route.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RouteLeg {
    /// Origin market, as an index into the instance's market list.
    pub from: u32,
    /// Destination market, likewise.
    pub to: u32,
    /// The trade performed.
    pub choice: LegChoice,
    /// Wall-clock for the hop, charged at the destination.
    pub millis: Millis,
    /// Straight-line separation, for the report only.
    pub distance_ly: f64,
}

/// A route's rate, and everything needed to read it honestly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateClaim<'a> {
    /// Credits per millisecond in the steady state, once the route is being
    /// flown repeatedly. `None` for a shape that has no steady state.
    pub steady: Option<Ratio>,
    /// Credits per millisecond over the first lap, which pays the one-off cost
    /// of reaching and loading at the first station.
    pub first_lap: Ratio,
    /// What is claimed about the search.
    pub guarantee: Guarantee,
    /// What is not claimed about the model.
    pub caveats: &'a [Caveat],
}

/// The result of a search.
///
/// `steady_rate` is private on purpose; see the module documentation.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    /// Which search produced it.
    pub kind: RouteKind,
    /// The hops, in flying order. For a cycle, the last leg's destination is
    /// the first leg's origin.
    pub legs: Vec<RouteLeg>,
    /// Total profit of one lap.
    pub profit: Credits,
    /// Wall-clock of one lap in the steady state. For a cycle this counts every
    /// station's approach exactly once, because each leg charges arrival,
    /// docking and the market screen at its *destination* and the last
    /// destination is the first origin.
    pub cycle_millis: Millis,
    /// Wall-clock of the first lap, which additionally pays for reaching and
    /// loading at the starting station.
    pub first_lap_millis: Millis,
    /// What is claimed about the search that found it.
    pub guarantee: Guarantee,
    /// What is not claimed about the model it was found in.
    pub caveats: Vec<Caveat>,
    /// The same route re-evaluated with credits accumulating leg by leg, if
    /// that has been done.
    pub threaded: Option<Threaded>,
    /// The total order this route is ranked by.
    pub rank: RankKey,
    steady_rate: Option<Ratio>,
}

/// A route re-walked with the balance rising as cargo is sold.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Threaded {
    /// Profit over one lap with credits accumulating.
    pub profit: Credits,
    /// The steady rate implied by that profit, when the shape has one.
    pub steady: Option<Ratio>,
    /// True when the hold was filled greedily rather than with the single
    /// commodity the search ranked.
    ///
    /// Set whenever the policy is in force, not only when more than one
    /// commodity was taken. Greedy filling maximises *margin* where the search
    /// maximises *profit*, and those come apart as soon as stock binds, so even
    /// a single-commodity greedy result can differ from the thing that was
    /// ranked — and a figure that does not answer the question the ranking
    /// asked must not be reported as if it did.
    pub greedy_fill: bool,
}

impl Route {
    /// The rate, with the claims that qualify it.
    #[must_use]
    pub fn rate(&self) -> RateClaim<'_> {
        RateClaim {
            steady: self.steady_rate,
            first_lap: Ratio::new(self.profit, self.first_lap_millis),
            guarantee: self.guarantee,
            caveats: &self.caveats,
        }
    }

    /// Builds a one-hop route. It gets no steady rate, and says so.
    #[must_use]
    pub fn single_hop(geometry: &Geometry<'_>, from: u32, to: u32, choice: LegChoice) -> Self {
        let leg = RouteLeg {
            from,
            to,
            choice,
            millis: geometry.leg_millis(from, to),
            distance_ly: geometry.leg_ly(from, to),
        };
        let first_lap_millis = geometry.startup_millis(from) + leg.millis;
        let mut caveats = leg_caveats(std::slice::from_ref(&leg), geometry.markets);
        caveats.push(Caveat::SingleHopNotRepeatable);
        caveats.sort_unstable();
        caveats.dedup();
        let rank = RankKey::build(
            geometry,
            std::slice::from_ref(&leg),
            Ratio::new(choice.profit, first_lap_millis),
            choice.profit,
            first_lap_millis,
            false,
        );
        let guarantee = bulk_guarantee(&[leg]).unwrap_or(Guarantee::OptimalForStartingCredits);
        Self {
            kind: RouteKind::SingleHop,
            legs: vec![leg],
            profit: choice.profit,
            cycle_millis: leg.millis,
            first_lap_millis,
            guarantee,
            caveats,
            threaded: None,
            rank,
            steady_rate: None,
        }
    }

    /// Builds a closed route from a cycle of nodes and the trade on each leg.
    ///
    /// `nodes[i]` trades `choices[i]` to `nodes[i + 1]`, and the final choice
    /// closes back to `nodes[0]`.
    ///
    /// # Panics
    ///
    /// If the two slices disagree in length, or the cycle is shorter than two
    /// stops. Both are solver bugs rather than data conditions.
    #[must_use]
    pub fn cycle(geometry: &Geometry<'_>, nodes: &[u32], choices: &[LegChoice]) -> Self {
        assert_eq!(nodes.len(), choices.len(), "one trade per leg");
        assert!(nodes.len() >= 2, "a cycle needs at least two stops");

        let legs: Vec<RouteLeg> = nodes
            .iter()
            .enumerate()
            .map(|(i, &from)| {
                let to = nodes[(i + 1) % nodes.len()];
                RouteLeg {
                    from,
                    to,
                    choice: choices[i],
                    millis: geometry.leg_millis(from, to),
                    distance_ly: geometry.leg_ly(from, to),
                }
            })
            .collect();

        let profit: Credits = legs.iter().map(|l| l.choice.profit).sum();
        let cycle_millis: Millis = legs.iter().map(|l| l.millis).sum();
        let first_lap_millis = geometry.startup_millis(nodes[0]) + cycle_millis;
        let steady = Ratio::new(profit, cycle_millis);
        let mut caveats = leg_caveats(&legs, geometry.markets);
        caveats.sort_unstable();
        caveats.dedup();
        let rank = RankKey::build(geometry, &legs, steady, profit, cycle_millis, true);
        let guarantee = bulk_guarantee(&legs).unwrap_or(Guarantee::OptimalForStartingCredits);

        Self {
            kind: if nodes.len() == 2 {
                RouteKind::RoundTrip
            } else {
                RouteKind::Loop { stops: nodes.len() }
            },
            legs,
            profit,
            cycle_millis,
            first_lap_millis,
            guarantee,
            caveats,
            threaded: None,
            rank,
            steady_rate: Some(steady),
        }
    }

    /// Replaces the guarantee, returning the route.
    #[must_use]
    pub fn with_guarantee(mut self, guarantee: Guarantee) -> Self {
        self.guarantee = bulk_guarantee(&self.legs).unwrap_or(guarantee);
        self
    }

    /// Mark the whole optimisation instance as dependent on empirical bulk
    /// pricing. This is stronger than inspecting the selected legs: a quote on
    /// an unselected edge can change which route wins, so no result from that
    /// graph may retain an exact optimality claim.
    pub fn mark_bulk_price_estimated(&mut self) {
        self.guarantee = Guarantee::Heuristic {
            reason: HeuristicReason::BulkPriceEstimate,
        };
        self.add_caveat(Caveat::BulkPriceEstimated);
    }

    /// Mark this route as re-priced after the search that produced it.
    ///
    /// The caller ranked on one set of prices, re-read some markets live, and
    /// rescored. That can only demote: a better route the first ranking buried
    /// is not discovered by rescoring it was never part of. So the ordering
    /// claim narrows to the routes that were rescored, and the optimality claim
    /// over the instance goes away.
    pub fn mark_rescored(&mut self) {
        self.guarantee = Guarantee::Heuristic {
            reason: HeuristicReason::RescoredAfterSearch,
        };
    }

    /// Adds a caveat, keeping the list sorted and unique.
    pub fn add_caveat(&mut self, caveat: Caveat) {
        if let Err(at) = self.caveats.binary_search(&caveat) {
            self.caveats.insert(at, caveat);
        }
    }

    /// Attaches a rethreaded evaluation and re-derives the ranking key from it.
    pub fn set_threaded(&mut self, threaded: Threaded) {
        self.rank.rate = match threaded.steady {
            Some(steady) => steady,
            None => Ratio::new(threaded.profit, self.first_lap_millis),
        };
        self.rank.profit = threaded.profit;
        self.threaded = Some(threaded);
    }
}

fn bulk_guarantee(legs: &[RouteLeg]) -> Option<Guarantee> {
    legs.iter()
        .any(|leg| leg.choice.bulk_estimated)
        .then_some(Guarantee::Heuristic {
            reason: HeuristicReason::BulkPriceEstimate,
        })
}

fn leg_caveats(legs: &[RouteLeg], markets: &[crate::model::Market]) -> Vec<Caveat> {
    // Three of these are unconditional: they are properties of reading a market
    // over a network at all, and a report that only mentioned them sometimes
    // would read as if their absence meant something.
    let mut caveats = vec![
        Caveat::StaleListing,
        Caveat::JumpGraphUnmodelled,
        Caveat::TimeModelAssumed,
        Caveat::AccessUnmodelled,
    ];
    for leg in legs {
        match leg.choice.limiter {
            Limiter::Stock => caveats.push(Caveat::StockDepletion),
            Limiter::Credits => caveats.push(Caveat::CreditCapBinds),
            Limiter::Cargo | Limiter::Demand => {}
        }
        if leg.choice.demand_assumed {
            caveats.push(Caveat::DemandUnpublished);
        }
        if leg.choice.bulk_estimated {
            caveats.push(Caveat::BulkPriceEstimated);
        }
        // Frontier's own id space says what a fleet carrier is: a carrier's
        // market id is `fleetCarrierId * 256 + 3_290_400_000`, so a market id
        // that is congruent to the base on that stride is a carrier and nothing
        // else is. No station type is needed, and none reaches this crate.
        if markets
            .get(leg.from as usize)
            .is_some_and(|market| {
                edm_core::carrier::carrier_id(market.market_id as f64).is_some()
            })
        {
            caveats.push(Caveat::CarrierSourceDoesNotRestock);
        }
    }
    caveats
}

/// The total order routes are ranked by.
///
/// Total, and ending in an absolute tie-break, so a shuffled input permutation
/// produces an identical ranking. Every field is exact: no rate is ever
/// compared as a quotient.
///
/// Ordering is "greater is better", so a max-heap over this is a best-first
/// list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankKey {
    /// Which of the two orderings below applies \[C47\].
    ///
    /// Stored on the key rather than consulted at the sort, because the search
    /// heap in `single`/`round` keeps its best N *by this key*: a high-profit,
    /// low-rate route would be evicted mid-search and never reach a display
    /// sort that wanted it.
    pub objective: crate::time::Objective,
    /// The rate this route is ranked by: the steady rate where one exists, the
    /// first-lap rate where it does not.
    pub rate: Ratio,
    /// Profit for one lap. Breaks a tie in rate toward the bigger trade.
    pub profit: Credits,
    /// Lap time. Breaks a tie in rate and profit toward the shorter route.
    pub millis: Millis,
    /// Market ids, rotated so the smallest is first for a cycle. Distinct
    /// routes always differ here or in the commodities, so this is where ties
    /// finally end.
    pub stations: Vec<i64>,
    /// The commodities carried, in the same rotation.
    pub commodities: Vec<u32>,
}

impl RankKey {
    fn build(
        geometry: &Geometry<'_>,
        legs: &[RouteLeg],
        rate: Ratio,
        profit: Credits,
        millis: Millis,
        rotate: bool,
    ) -> Self {
        let ids: Vec<i64> = legs
            .iter()
            .map(|l| geometry.markets[l.from as usize].market_id)
            .collect();
        let goods: Vec<u32> = legs.iter().map(|l| l.choice.commodity.0).collect();
        // A cycle has no start, so two rotations of it are the same route and
        // must produce the same key. Market ids are unique, so rotating the
        // smallest to the front is a canonical form.
        let shift = if rotate {
            ids.iter()
                .enumerate()
                .min_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(&b.0)))
                .map_or(0, |(i, _)| i)
        } else {
            0
        };
        let n = ids.len();
        let stations = (0..n).map(|i| ids[(i + shift) % n]).collect();
        let commodities = (0..n).map(|i| goods[(i + shift) % n]).collect();
        Self {
            objective: geometry.objective,
            rate,
            profit,
            millis,
            stations,
            commodities,
        }
    }
}

impl Ord for RankKey {
    /// Greater is better, under whichever objective the key carries.
    ///
    /// Both orderings end in the same absolute tie-break, so a shuffled input
    /// permutation still produces an identical ranking. Comparing two keys
    /// built under *different* objectives is meaningless, and cannot happen:
    /// the objective comes from the geometry, and one search has one geometry.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let head = match self.objective {
            crate::time::Objective::Rate => self
                .rate
                .cmp(&other.rate)
                .then(self.profit.cmp(&other.profit)),
            // Distance ignored: the whole point is that a route two hundred
            // light years out and one next door compare on credits alone. Time
            // still breaks an exact tie, because between two identical payouts
            // the shorter flight is strictly better.
            crate::time::Objective::Profit => self.profit.cmp(&other.profit),
        };
        head
            // Less time is better, so the comparison inverts here.
            .then(other.millis.cmp(&self.millis))
            // And the tie-break prefers the lower market id, deterministically.
            .then(other.stations.cmp(&self.stations))
            .then(other.commodities.cmp(&self.commodities))
    }
}

impl PartialOrd for RankKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::RankKey;
    use crate::fixture::{choice, geometry, market};


    /// A carrier's stock is a fixed pot one commander filled, not an economy.
    /// The rate assumes a repeatable lap, so a carrier *source* is exactly the
    /// case where it is worth the least — and the caveat is what says so now
    /// the rate is not on screen \[C39\].
    ///
    /// A carrier is recognised by Frontier's own id arithmetic, so no station
    /// type is needed: 3,711,014,400 is `T1N-W2F`, measured live.
    #[test]
    fn a_carrier_source_says_its_stock_does_not_restock() {
        const CARRIER: i64 = 3_711_014_400;
        const STATION: i64 = 128_016_384;

        let from_carrier = vec![
            market(CARRIER, 0.0, &[(0, 1_000, 10_000)], &[]),
            market(STATION, 10.0, &[], &[(0, 5_000, 10_000)]),
        ];
        let hop = Route::single_hop(&geometry(&from_carrier), 0, 1, choice(0, 1_000));
        assert!(
            hop.caveats.contains(&super::Caveat::CarrierSourceDoesNotRestock),
            "{:?}",
            hop.caveats
        );

        // Buying from a station and selling *to* a carrier is not the same
        // thing: what does not regenerate is the seller's shelf.
        let to_carrier = vec![
            market(STATION, 0.0, &[(0, 1_000, 10_000)], &[]),
            market(CARRIER, 10.0, &[], &[(0, 5_000, 10_000)]),
        ];
        let hop = Route::single_hop(&geometry(&to_carrier), 0, 1, choice(0, 1_000));
        assert!(
            !hop.caveats.contains(&super::Caveat::CarrierSourceDoesNotRestock),
            "{:?}",
            hop.caveats
        );
    }
    use crate::num::{Credits, Millis, Ratio};
    use crate::report::Route;

    #[test]
    fn a_rotated_cycle_ranks_identically() {
        let markets = [
            market(70, 0.0, &[], &[]),
            market(20, 5.0, &[], &[]),
            market(50, 9.0, &[], &[]),
        ];
        let geometry = geometry(&markets);
        let a = Route::cycle(
            &geometry,
            &[0, 1, 2],
            &[choice(0, 10), choice(1, 20), choice(2, 30)],
        );
        let b = Route::cycle(
            &geometry,
            &[1, 2, 0],
            &[choice(1, 20), choice(2, 30), choice(0, 10)],
        );
        assert_eq!(a.rank, b.rank);
        assert_eq!(a.rank.stations, vec![20, 50, 70]);
    }

    #[test]
    fn a_single_hop_has_no_steady_rate() {
        let markets = [market(1, 0.0, &[], &[]), market(2, 5.0, &[], &[])];
        let geometry = geometry(&markets);
        let route = Route::single_hop(&geometry, 0, 1, choice(0, 100));
        assert!(route.rate().steady.is_none());
        assert!(
            route
                .caveats
                .contains(&super::Caveat::SingleHopNotRepeatable)
        );
    }

    #[test]
    fn the_rank_key_is_a_total_order_that_ends_in_station_ids() {
        let base = RankKey {
            objective: crate::time::Objective::Rate,
            rate: Ratio {
                credits: 1,
                millis: 1,
            },
            profit: Credits(10),
            millis: Millis(10),
            stations: vec![5, 9],
            commodities: vec![0, 1],
        };
        let lower_id = RankKey {
            stations: vec![4, 9],
            ..base.clone()
        };
        assert!(lower_id > base);
        let faster = RankKey {
            millis: Millis(9),
            ..base.clone()
        };
        assert!(faster > base);
        let richer = RankKey {
            profit: Credits(11),
            ..base.clone()
        };
        assert!(richer > base);
    }

    #[test]
    fn a_cycle_charges_each_station_once() {
        // Two stations 5 LY apart, both at their star: each leg is one jump
        // plus the fixed approach, and the cycle is exactly twice a leg.
        let markets = [market(1, 0.0, &[], &[]), market(2, 5.0, &[], &[])];
        let geometry = geometry(&markets);
        let route = Route::cycle(&geometry, &[0, 1], &[choice(0, 100), choice(1, 100)]);
        let leg = geometry.leg_millis(0, 1);
        assert_eq!(route.cycle_millis, leg + leg);
        assert_eq!(
            route.first_lap_millis,
            route.cycle_millis + geometry.startup_millis(0)
        );
    }

    #[test]
    fn an_empirical_bulk_quote_can_never_be_relabelled_proved() {
        let markets = [market(1, 0.0, &[], &[]), market(2, 5.0, &[], &[])];
        let geometry = geometry(&markets);
        let mut estimated = choice(0, 100);
        estimated.bulk_estimated = true;
        let route = Route::single_hop(&geometry, 0, 1, estimated)
            .with_guarantee(super::Guarantee::ProvedOptimal);
        assert_eq!(
            route.guarantee,
            super::Guarantee::Heuristic {
                reason: super::HeuristicReason::BulkPriceEstimate
            }
        );
        assert!(route.caveats.contains(&super::Caveat::BulkPriceEstimated));
    }

    #[test]
    fn an_unselected_empirical_edge_still_downgrades_the_whole_instance() {
        let markets = [market(1, 0.0, &[], &[]), market(2, 5.0, &[], &[])];
        let geometry = geometry(&markets);
        let mut route = Route::single_hop(&geometry, 0, 1, choice(0, 100))
            .with_guarantee(super::Guarantee::ProvedOptimal);
        route.mark_bulk_price_estimated();
        assert_eq!(
            route.guarantee,
            super::Guarantee::Heuristic {
                reason: super::HeuristicReason::BulkPriceEstimate
            }
        );
        assert!(route.caveats.contains(&super::Caveat::BulkPriceEstimated));
    }

    /// The whole point of the flag: a route that pays more wins even when it
    /// takes far longer, which is exactly what the rate objective refuses
    /// \[C47\].
    #[test]
    fn by_profit_prefers_the_bigger_payout_over_the_faster_lap() {
        let quick = RankKey {
            objective: crate::time::Objective::Profit,
            rate: Ratio::new(Credits(1_000), Millis(1_000)),
            profit: Credits(1_000),
            millis: Millis(1_000),
            stations: vec![1, 2],
            commodities: vec![0],
        };
        let rich = RankKey {
            profit: Credits(10_000),
            millis: Millis(600_000),
            rate: Ratio::new(Credits(10_000), Millis(600_000)),
            stations: vec![3, 4],
            ..quick.clone()
        };
        assert!(rich > quick, "profit must win when distance is ignored");

        // And under the default objective the fast one wins, unchanged.
        let quick_rate = RankKey {
            objective: crate::time::Objective::Rate,
            ..quick
        };
        let rich_rate = RankKey {
            objective: crate::time::Objective::Rate,
            ..rich
        };
        assert!(
            quick_rate > rich_rate,
            "the rate objective must still prefer the faster lap"
        );
    }

    /// Equal payouts still break toward less flying: between two identical
    /// credits the shorter trip is strictly better.
    #[test]
    fn by_profit_breaks_an_exact_tie_toward_the_shorter_flight() {
        let near = RankKey {
            objective: crate::time::Objective::Profit,
            rate: Ratio::new(Credits(5_000), Millis(1_000)),
            profit: Credits(5_000),
            millis: Millis(1_000),
            stations: vec![1, 2],
            commodities: vec![0],
        };
        let far = RankKey {
            millis: Millis(900_000),
            ..near.clone()
        };
        assert!(near > far);
    }
}
