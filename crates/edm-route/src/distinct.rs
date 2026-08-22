//! Loops that visit at least `k_min` distinct stations.
//!
//! This, and not "distinct stations", is the genuinely hard variant. Distinct
//! stations come free: the bounded-length dynamic program's optimum is already
//! a simple cycle, by the mediant decomposition. But a *lower* bound on the
//! number of stops cannot be relaxed away — a two-stop shuttle drains both ends
//! in a single lap, and "give me a five-stop loop" is the request commanders
//! actually have — and requiring it makes the problem NP-hard. So this is a
//! branch and bound, with a budget, and it says which of the two it returned.
//!
//! # The bound
//!
//! The free optimum `λ*` from [`crate::ratio`] is an upper bound on every
//! cycle's rate, constrained or not, and at `λ*` the reduced graph has **no
//! positive cycle** — that is precisely what the ratio solver proved. So the
//! best reduced value of any walk from `v` back to the origin, `D*[v]`, is
//! finite and computable once per origin by a single Bellman-Ford pass.
//!
//! Write `h_λ(e) = w(e) − λ·t(e)`, so a cycle beats `λ` exactly when its
//! `h_λ` sum is positive. For a partial path `P` ending at `v` with incumbent
//! `λ_inc`, and any completion `R` back to the origin:
//!
//! ```text
//! Σ h_λinc(R) = Σ h_λ*(R) + (λ* − λinc)·T_R  ≤  D*[v] + (λ* − λinc)·T_max
//! ```
//!
//! so the partial path can be abandoned when
//!
//! ```text
//! Δ_P + D*[v] + (λ* − λinc)·T_max  ≤  0
//! ```
//!
//! Multiplying through by the two positive denominators clears every division
//! and leaves the integer test in [`beyond_reach`].
//!
//! **This parametric form is structurally immune to the bug in
//! `edtrade/src/solve/circuit.ts:99`**, which converts a profit bound into a
//! per-hour bound by dividing by the *maximum* leg count's time floor. That
//! divisor is too large for the shorter circuits in the same subtree, so the
//! bound comes out too tight and can prune the true winner. Folding numerator
//! and denominator into one quantity *before* bounding leaves nothing to get
//! backwards. `the_completion_bound_does_not_prune_a_short_winner` in the test
//! suite exists to fail if anyone simplifies this back to profit-per-hour.
//!
//! `T_max` must remain an **upper** bound on the completion's time. Tightening
//! it is tempting and easy to get backwards, so a `debug_assert` checks it
//! against the realised time every time a cycle closes.
//!
//! # Dominance
//!
//! Pareto on `(W, −T)`: a partial path is dominated by another ending at the
//! same node with the same stations visited, no less profit and no more time.
//! **Dominance on reduced value alone is not sound** for a ratio objective — a
//! path with smaller `W` but much smaller `T` can still win — and that is the
//! natural-looking rule.
//!
//! # Overflow
//!
//! The bound multiplies a Bellman-Ford distance (≤ 2^84) by a rate denominator
//! (≤ 2^36), so intermediates reach 2^120 and the three-term sum 2^122. That
//! fits `i128` with five bits to spare, and it is the widest arithmetic in the
//! crate — which is why rates are always reduced to lowest terms before they
//! get here.

use std::collections::HashMap;

use crate::graph::TradeGraph;
use crate::model::Limits;
use crate::num::{Credits, Millis, Ratio};
use crate::ratio::{self, Component, LocalEdge};
use crate::report::{Guarantee, HeuristicReason, Route};
use crate::round;
use crate::time::Geometry;
use crate::watch::{Event, Watch};

/// See `bounded::UNREACHABLE`.
const UNREACHABLE: i128 = i128::MIN / 4;

/// How many expansions pass between two consultations of the caller's budget.
///
/// The budget predicate is a call through a reference and, for a real caller,
/// a clock read; at the twenty million expansions [`Limits::search_budget`]
/// admits, asking on every one of them would cost more than the expansion. A
/// power of two so the test is a mask.
const EXPANSIONS_PER_CHECK: u64 = 4_096;

/// The best loop meeting the stop floor, and how well it was established.
#[derive(Clone, Debug)]
pub struct ConstrainedCycle {
    /// The cycle, in flying order.
    pub nodes: Vec<u32>,
    /// Its exact rate.
    pub rate: Ratio,
    /// Whether the search finished rather than running out of budget.
    pub proved: bool,
    /// The free optimum, which bounds every cycle in the graph including this
    /// one. Equal to `rate` when the free optimum already met the floor.
    pub upper: Ratio,
    /// Whether `upper` was itself proved.
    ///
    /// Separate from `proved`, and load-bearing. `upper` comes from the free
    /// ratio solver, which can now stop on the caller's budget — and a rate the
    /// free solver merely reached is a rate some cycle achieves, not a bound on
    /// every cycle. Quoting it as `Guarantee::BoundedGap` would claim "nothing
    /// better than this exists" from a search that never established it, which
    /// on the early-return path below would read as an optimum outright.
    pub bound_proved: bool,
    /// How many partial paths were expanded.
    pub expansions: u64,
}

/// Searches for the best loop with at least `k_min` distinct stations.
#[must_use]
pub fn best_with_min_stops(
    graph: &TradeGraph,
    limits: &Limits,
    k_min: usize,
    watch: Watch<'_>,
) -> Option<ConstrainedCycle> {
    let free = ratio::max_ratio_cycle(graph, watch)?;
    let upper = free.rate;

    // The cheapest possible outcome: the unconstrained optimum already meets
    // the floor, so it is optimal for the constrained problem as well. Worth
    // checking first — it is common, and it turns an NP-hard search into no
    // search at all.
    if free.nodes.len() >= k_min {
        return Some(ConstrainedCycle {
            nodes: free.nodes.clone(),
            rate: free.rate,
            proved: free.proved,
            upper,
            bound_proved: free.proved,
            expansions: 0,
        });
    }

    let ceiling = limits.max_stops.unwrap_or(graph.len());
    if ceiling < k_min {
        return None;
    }

    let mut search = Search {
        graph,
        upper,
        max_millis: graph.max_millis(),
        k_min,
        ceiling,
        budget: limits.search_budget,
        expansions: 0,
        expired: false,
        watch,
        best: None,
    };

    // The bound the pruning rests on is `D*[v]` measured at the free optimum,
    // and it is finite only because the ratio solver proved no cycle has
    // positive reduced weight there. An unproved `upper` invalidates the
    // arithmetic, not just the wording — so there is nothing to search.
    if free.proved {
        for component in Component::all(graph) {
            if component.nodes().len() < k_min {
                continue;
            }
            search.run(&component);
        }
    }

    // Same rule as `ratio::max_ratio_cycle`, and reachable here on ordinary
    // data: an unproved `free` skips the search entirely, so `best` is empty
    // and this `?` means "no cycle meets the floor" — which is a different fact
    // from "the clock ran out", and the two must not be announced together.
    // The withdrawal is therefore reported *here*, once, and only when there is
    // a route for it to be about.
    let (nodes, rate) = search.best.clone()?;
    if search.expired {
        watch.report(Event::Abandoned);
    }
    Some(ConstrainedCycle {
        nodes,
        rate,
        proved: free.proved && !search.expired && search.expansions < search.budget,
        upper,
        bound_proved: free.proved,
        expansions: search.expansions,
    })
}

struct Search<'a> {
    graph: &'a TradeGraph,
    upper: Ratio,
    max_millis: Millis,
    k_min: usize,
    ceiling: usize,
    budget: u64,
    expansions: u64,
    /// Whether the caller's budget stopped this search. Separate from the
    /// expansion count, because they are different reasons to say the same
    /// thing and only one of them is visible in `expansions`.
    expired: bool,
    watch: Watch<'a>,
    best: Option<(Vec<u32>, Ratio)>,
}

struct Frame<'a> {
    adjacency: &'a [Vec<LocalEdge>],
    reach: Vec<i128>,
    origin: u32,
    path: Vec<u32>,
    visited: Vec<bool>,
    profit: Credits,
    millis: Millis,
    seen: HashMap<(u32, u64), Vec<(Credits, Millis)>>,
    global: Vec<u32>,
}

impl Search<'_> {
    fn run(&mut self, component: &Component<'_>) {
        let adjacency = component.adjacency();
        let n = component.nodes().len();
        for origin in 0..n as u32 {
            // Asked unconditionally here, unlike inside the recursion.
            // `reachability` below is a Bellman-Ford pass over the whole
            // component — the dominant per-origin cost — and the expansion
            // counter barely moves between origins when the pruning is working,
            // which is the design goal. Gating this check on the counter made
            // the per-origin budget effectively dead: a search could spend
            // minutes past its deadline, one full pass at a time.
            if self.expansions >= self.budget || self.expired_now() {
                return;
            }
            // One Bellman-Ford pass per origin buys the bound for every partial
            // path that will ever end anywhere, which is what makes the pruning
            // affordable.
            let reach = reachability(&adjacency, n, self.upper, origin);
            let mut frame = Frame {
                adjacency: &adjacency,
                reach,
                origin,
                path: vec![origin],
                visited: vec![false; n],
                profit: Credits::ZERO,
                millis: Millis::ZERO,
                seen: HashMap::new(),
                global: component.nodes().to_vec(),
            };
            frame.visited[origin as usize] = true;
            self.descend(&mut frame, origin);
        }
    }

    /// Whether the caller's budget has been spent, asked rarely and remembered.
    ///
    /// Remembered because a predicate that has answered `true` once is
    /// documented never to answer `false` again, so re-asking is pure cost —
    /// and because the recursion below needs the answer at every level of a
    /// path it is unwinding.
    fn stop(&mut self) -> bool {
        if self.expired {
            return true;
        }
        if self.expansions.is_multiple_of(EXPANSIONS_PER_CHECK) && self.watch.expired() {
            self.expired = true;
        }
        self.expired
    }

    /// The same question, asked without the counter gate.
    ///
    /// For the once-per-origin check, where the work between calls is a whole
    /// Bellman-Ford pass rather than a handful of expansions.
    fn expired_now(&mut self) -> bool {
        if !self.expired && self.watch.expired() {
            self.expired = true;
        }
        self.expired
    }

    fn descend(&mut self, frame: &mut Frame<'_>, node: u32) {
        if self.expansions >= self.budget || self.stop() {
            return;
        }
        if self.expansions.is_multiple_of(EXPANSIONS_PER_CHECK) {
            self.watch.report(Event::Expanded {
                paths: self.expansions,
                budget: self.budget,
            });
        }
        self.expansions += 1;

        for edge in &frame.adjacency[node as usize] {
            if edge.to == frame.origin {
                if frame.path.len() >= self.k_min {
                    self.close(frame, *edge);
                }
                continue;
            }
            // Canonical form: a cycle is explored only from its lowest-numbered
            // station, so each one is generated once rather than once per
            // rotation. A station below the origin means this cycle will be
            // found from there instead.
            if edge.to < frame.origin || frame.visited[edge.to as usize] {
                continue;
            }
            if frame.path.len() >= self.ceiling {
                continue;
            }

            let profit = frame.profit + edge.weight;
            let millis = frame.millis + edge.millis;
            let incumbent = self.best.as_ref().map_or(Ratio::ZERO, |(_, rate)| *rate);
            let remaining = self.ceiling.saturating_sub(frame.path.len());
            if beyond_reach(
                Partial {
                    profit,
                    millis,
                    reach: frame.reach[edge.to as usize],
                },
                Bracket {
                    incumbent,
                    upper: self.upper,
                    max_millis: Millis(self.max_millis.0 * remaining as i64),
                },
            ) {
                continue;
            }
            if !frame.admit(edge.to, profit, millis) {
                continue;
            }

            let restore = (frame.profit, frame.millis);
            frame.path.push(edge.to);
            frame.visited[edge.to as usize] = true;
            frame.profit = profit;
            frame.millis = millis;
            self.descend(frame, edge.to);
            frame.visited[edge.to as usize] = false;
            frame.path.pop();
            frame.profit = restore.0;
            frame.millis = restore.1;
        }
    }

    fn close(&mut self, frame: &mut Frame<'_>, closing: LocalEdge) {
        let profit = frame.profit + closing.weight;
        let millis = frame.millis + closing.millis;
        debug_assert!(
            closing.millis.0 <= self.max_millis.0,
            "the completion bound must stay an upper bound on the realised time"
        );
        let rate = Ratio::new(profit, millis);
        let nodes: Vec<u32> = frame
            .path
            .iter()
            .map(|&local| frame.global[local as usize])
            .collect();
        // Only the incumbent is kept. Every closed path could be recorded for
        // the runner-up listing, but the budget admits twenty million of them
        // and the listing is enumerated separately and deterministically.
        let better = match &self.best {
            None => true,
            Some((current, current_rate)) => match rate.cmp(current_rate) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => nodes < *current,
                std::cmp::Ordering::Less => false,
            },
        };
        if better {
            debug_assert!(
                round::price_cycle(self.graph, &nodes).is_some(),
                "a closed path must be a cycle of the graph it came from"
            );
            self.best = Some((nodes, rate));
        }
    }
}

impl Frame<'_> {
    /// Pareto admission on `(profit, −time)` for a fixed node and station set.
    ///
    /// Only for components small enough to key the station set by a bitmask.
    /// Above that the parametric bound carries the search alone, which is
    /// slower but never wrong — the alternative, keying dominance on the node
    /// only, would compare partial paths with different completions available
    /// and is unsound.
    fn admit(&mut self, node: u32, profit: Credits, millis: Millis) -> bool {
        if self.visited.len() > 64 {
            return true;
        }
        let mut mask = 0u64;
        for (i, &seen) in self.visited.iter().enumerate() {
            if seen {
                mask |= 1u64 << i;
            }
        }
        mask |= 1u64 << node;
        let frontier = self.seen.entry((node, mask)).or_default();
        if frontier
            .iter()
            .any(|&(other_profit, other_millis)| other_profit >= profit && other_millis <= millis)
        {
            return false;
        }
        frontier.retain(|&(other_profit, other_millis)| {
            !(profit >= other_profit && millis <= other_millis)
        });
        frontier.push((profit, millis));
        true
    }
}

/// A partial path's totals, plus the best any completion of it can add.
#[derive(Clone, Copy, Debug)]
struct Partial {
    profit: Credits,
    millis: Millis,
    /// `D*[v]`: the best reduced value of a walk from here back to the origin,
    /// measured at the free optimum.
    reach: i128,
}

/// The two rates the bound is taken between, and the time budget it allows.
#[derive(Clone, Copy, Debug)]
struct Bracket {
    incumbent: Ratio,
    upper: Ratio,
    max_millis: Millis,
}

/// Whether no completion of this partial path can beat the incumbent.
///
/// The derivation is in the module documentation. Everything is `i128` and
/// nothing divides, so there is no rounding to argue about and no direction to
/// get backwards.
fn beyond_reach(partial: Partial, bracket: Bracket) -> bool {
    if partial.reach == UNREACHABLE {
        return true;
    }
    let incumbent_millis = i128::from(bracket.incumbent.millis);
    let incumbent_credits = i128::from(bracket.incumbent.credits);
    let upper_millis = i128::from(bracket.upper.millis);
    let upper_credits = i128::from(bracket.upper.credits);

    // Δ_P, scaled by the incumbent's denominator.
    let path = incumbent_millis * i128::from(partial.profit.0)
        - incumbent_credits * i128::from(partial.millis.0);
    // (λ* − λ_inc), scaled by both denominators.
    let spread = upper_credits * incumbent_millis - incumbent_credits * upper_millis;
    let total = path * upper_millis
        + partial.reach * incumbent_millis
        + spread * i128::from(bracket.max_millis.0);
    total <= 0
}

/// `D*[v]` for every `v`: the best reduced value of a walk from `v` to the
/// origin at the free optimum.
///
/// Finite because the ratio solver proved no cycle has positive reduced weight
/// at that rate — which is exactly what makes this bound computable at all.
fn reachability(adjacency: &[Vec<LocalEdge>], n: usize, upper: Ratio, origin: u32) -> Vec<i128> {
    let mut best = vec![UNREACHABLE; n];
    best[origin as usize] = 0;
    for _ in 0..n {
        let mut changed = false;
        for (from, edges) in adjacency.iter().enumerate() {
            for edge in edges {
                if best[edge.to as usize] == UNREACHABLE {
                    continue;
                }
                let candidate =
                    best[edge.to as usize] + ratio::reduced(upper, edge.weight, edge.millis);
                if candidate > best[from] {
                    best[from] = candidate;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    best
}

/// The best loops with at least `k_min` distinct stations, best first.
#[must_use]
pub fn solve(
    graph: &TradeGraph,
    geometry: &Geometry<'_>,
    limits: &Limits,
    k_min: usize,
    watch: Watch<'_>,
) -> Vec<Route> {
    let Some(best) = best_with_min_stops(graph, limits, k_min, watch) else {
        return Vec::new();
    };
    let Some(head) = round::route_of(graph, geometry, &best.nodes) else {
        return Vec::new();
    };
    // Three claims, in descending strength, and the middle one is the reason
    // `bound_proved` exists: an upper bound is only worth quoting if something
    // proved it. Without that distinction an exhausted run would announce a gap
    // of zero against a bound it invented, which reads as an optimum.
    let head_guarantee = if best.proved {
        Guarantee::OptimalForStartingCredits
    } else if best.bound_proved {
        Guarantee::BoundedGap { upper: best.upper }
    } else {
        Guarantee::Heuristic {
            reason: HeuristicReason::SearchBudgetExhausted,
        }
    };

    let ceiling = limits
        .max_stops
        .unwrap_or_else(|| k_min.max(round::DEFAULT_RUNNER_UP_STOPS))
        .max(k_min);
    let mut listing = round::listing(
        graph,
        geometry,
        limits,
        head.with_guarantee(head_guarantee),
        k_min..=ceiling,
    );
    // A proved *bound* is still a finished search: it says how much room was
    // left, which is a claim about the ranking as a whole. Only a run that
    // proved neither the optimum nor a bound taints its list.
    round::taint_unfinished(&mut listing, best.proved || best.bound_proved);
    listing
}

#[cfg(test)]
mod tests {
    use super::{Bracket, Partial, best_with_min_stops, beyond_reach};
    use crate::fixture::{geometry, limits, market, ship};
    use crate::graph::{Pools, TradeGraph};
    use crate::model::Market;
    use crate::num::{Credits, Millis, Ratio};
    use crate::report::{Guarantee, HeuristicReason};
    use crate::watch::Watch;

    fn build(markets: &[Market]) -> TradeGraph {
        TradeGraph::build(
            &Pools::from_markets(markets),
            &geometry(markets),
            &ship(),
            &limits(),
            Watch::unlimited(),
        )
    }

    #[test]
    fn an_unreachable_completion_is_always_pruned() {
        assert!(beyond_reach(
            Partial {
                profit: Credits(1_000_000),
                millis: Millis(1),
                reach: super::UNREACHABLE
            },
            Bracket {
                incumbent: Ratio::ZERO,
                upper: Ratio {
                    credits: 1,
                    millis: 1
                },
                max_millis: Millis(1),
            },
        ));
    }

    #[test]
    fn nothing_is_pruned_before_an_incumbent_exists() {
        // With the incumbent at zero the test reduces to "can this path earn
        // anything at all", and every profitable path can.
        assert!(!beyond_reach(
            Partial {
                profit: Credits(10),
                millis: Millis(100),
                reach: 0
            },
            Bracket {
                incumbent: Ratio::ZERO,
                upper: Ratio {
                    credits: 1,
                    millis: 1
                },
                max_millis: Millis(1_000),
            },
        ));
    }

    #[test]
    fn a_hopeless_path_is_pruned_once_the_incumbent_is_good() {
        // The incumbent earns 10 credits per millisecond and the free optimum
        // is the same, so no completion can add anything: a path earning one
        // credit per millisecond cannot recover.
        let rate = Ratio {
            credits: 10,
            millis: 1,
        };
        assert!(beyond_reach(
            Partial {
                profit: Credits(100),
                millis: Millis(100),
                reach: 0
            },
            Bracket {
                incumbent: rate,
                upper: rate,
                max_millis: Millis(1_000)
            },
        ));
    }

    #[test]
    fn the_free_optimum_is_returned_directly_when_it_already_has_enough_stops() {
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[(2, 400, 500)]),
            market(2, 1.0, &[(1, 100, 500), (2, 100, 500)], &[(0, 900, 500)]),
            market(3, 2.0, &[(2, 100, 500)], &[(1, 900, 500)]),
        ];
        let graph = build(&markets);
        let found = best_with_min_stops(&graph, &limits(), 3, Watch::unlimited()).expect("a loop");
        assert_eq!(found.expansions, 0);
        assert_eq!(found.nodes.len(), 3);
        assert_eq!(found.rate, found.upper);
        assert!(found.bound_proved);
    }

    /// The two-cycle 0<->1 is the best rate; the three-cycle is worse but is
    /// the only thing that meets a floor of three.
    fn floor_forcing() -> [Market; 3] {
        [
            market(
                1,
                0.0,
                &[(0, 100, 500), (3, 100, 500)],
                &[(2, 900, 500), (1, 900, 500)],
            ),
            market(
                2,
                1.0,
                &[(1, 100, 500), (2, 100, 500)],
                &[(0, 900, 500), (3, 200, 500)],
            ),
            market(3, 2.0, &[(2, 100, 500)], &[(1, 200, 500)]),
        ]
    }

    #[test]
    fn a_floor_above_the_free_optimum_forces_the_longer_loop() {
        let markets = floor_forcing();
        let graph = build(&markets);
        let free = crate::ratio::max_ratio_cycle(&graph, Watch::unlimited()).expect("a cycle");
        assert_eq!(free.nodes.len(), 2);
        let found = best_with_min_stops(&graph, &limits(), 3, Watch::unlimited()).expect("a loop");
        assert_eq!(found.nodes.len(), 3);
        assert!(found.proved);
        assert!(found.bound_proved);
        assert!(found.rate < found.upper);
    }

    #[test]
    fn an_unproved_upper_bound_is_never_quoted_as_a_bounded_gap() {
        // This is the shape of the claim that was wrong before there was a
        // budget to expire: on the early-return path `upper == rate`, so
        // `BoundedGap { upper }` says "nothing better than this exists and this
        // route achieves it" — a proof of optimality, assembled out of a search
        // that proved nothing.
        let markets = floor_forcing();
        let graph = build(&markets);
        let spent = || true;
        let watch = Watch::unlimited().until(&spent);
        let found = best_with_min_stops(&graph, &limits(), 2, watch).expect("the warm start");
        assert!(!found.proved);
        assert!(!found.bound_proved);
        assert_eq!(
            found.rate, found.upper,
            "the early return quotes itself as its own bound"
        );

        let routes = super::solve(&graph, &geometry(&markets), &limits(), 2, watch);
        assert_eq!(
            routes[0].guarantee,
            Guarantee::Heuristic {
                reason: HeuristicReason::SearchBudgetExhausted
            }
        );
    }
}
