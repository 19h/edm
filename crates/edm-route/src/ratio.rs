//! The maximum ratio cycle, by Dinkelbach's method. The centrepiece.
//!
//! A route flown repeatedly earns `Σw / Σt` over its cycle, so the best
//! repeatable route is the cycle maximising that ratio — a problem that is
//! exactly solvable in polynomial time, and whose optimum is always a *simple*
//! cycle. A closed walk decomposes into simple cycles, and the mediant
//! inequality `(a+c)/(b+d) <= max(a/b, c/d)` holds for positive denominators,
//! so no walk beats the best simple cycle and the unconstrained optimum never
//! revisits a station on its own.
//!
//! # The iteration
//!
//! ```text
//! λ := the best round trip's ratio          (exact, achievable, already known)
//! loop:
//!     g(e) := λ.millis·w(e) − λ.credits·t(e)
//!     if no cycle has Σg > 0:  return the witness in hand — PROVED OPTIMAL
//!     C := the witness cycle
//!     λ := Ratio { Σ_C w, Σ_C t }                        (strictly greater)
//! ```
//!
//! `Σ_C g > 0` says exactly `Σw / Σt > λ`, so each round replaces `λ` with the
//! exact ratio of a real cycle that beats it. The values of `λ` are ratios of
//! simple cycles and strictly increase, and there are finitely many, so the
//! loop terminates — and it terminates **with a certificate**, because the
//! stopping condition *is* the optimality proof: no cycle has positive reduced
//! weight at `λ`, and `λ` is achieved by the cycle in hand.
//!
//! That is the whole reason for choosing this method. Bisection converges
//! toward the optimum without ever reaching it and has to be stopped by an
//! epsilon that has no principled value. Howard's policy iteration is faster in
//! practice, but its correctness rests on a policy-improvement argument and a
//! subtly wrong implementation returns a *plausible number* rather than
//! crashing; it belongs behind a cross-checked flag if profiling ever demands
//! it. Megiddo's parametric search carries a large constant and a
//! symbolic-simulation implementation whose bugs are silent.
//!
//! # Overflow
//!
//! At instance bounds — cargo ≤ 2^15, unit margin ≤ 2^20, leg time ≤ 2^23 ms,
//! markets ≤ 2^13 — a cycle's profit reaches 2^48 and its time 2^36, so `λ`'s
//! two fields are that size. A reduced weight is then
//! `λ.millis · w − λ.credits · t` ≈ 2^36 · 2^35 = **2^71**, and a Bellman-Ford
//! distance accumulates those along a path to **2^84**. Both are far outside
//! `i64`, and the bound shrinks with the *instance*, not with the cycle — so
//! there is no narrow fast path even for a three-cycle. All of it is `i128`,
//! unconditionally.

use crate::graph::TradeGraph;
use crate::model::Limits;
use crate::num::{Credits, Millis, Ratio};
use crate::report::{Guarantee, HeuristicReason, Route};
use crate::round;
use crate::time::Geometry;

/// How many rounds the iteration may take before it is treated as a bug.
///
/// Each round strictly increases `λ` over a finite set, so this can only fire
/// if the arithmetic below is wrong. It exists so that such a bug surfaces as a
/// downgraded guarantee rather than as a program that never returns.
const MAX_ROUNDS: u32 = 4_096;

/// The reduced weight of an edge at a rate.
///
/// Positive exactly when the edge earns better than `rate`; the sum over a
/// cycle is positive exactly when the cycle does.
#[must_use]
pub fn reduced(rate: Ratio, weight: Credits, millis: Millis) -> i128 {
    i128::from(rate.millis) * i128::from(weight.0)
        - i128::from(rate.credits) * i128::from(millis.0)
}

/// Finds a cycle of strictly positive total weight, or proves there is none.
///
/// Bellman-Ford with **every distance initialised to zero** and no source.
/// That is the whole trick: a zero start makes every vertex its own origin, so
/// a relaxation surviving the `n`-th round means some vertex is reachable from
/// a positive cycle, and walking predecessors `n` times from it lands inside
/// one. The predecessor graph then hands back the cycle itself, distinct
/// vertices and all.
///
/// Nodes are `0..n` and `weights[i]` belongs to `edges[i]`.
///
/// Kept deliberately small and free of the surrounding problem: this is the
/// primitive every claim in this module rests on, and it is worth being able to
/// test it against hand-written graphs with no economics anywhere near it.
#[must_use]
pub fn positive_cycle(n: usize, edges: &[(u32, u32)], weights: &[i128]) -> Option<Vec<u32>> {
    debug_assert_eq!(edges.len(), weights.len());
    let mut dist = vec![0i128; n];
    let mut pred = vec![u32::MAX; n];
    let mut relaxed = None;

    for _ in 0..n {
        relaxed = None;
        for (i, &(from, to)) in edges.iter().enumerate() {
            let candidate = dist[from as usize] + weights[i];
            if candidate > dist[to as usize] {
                dist[to as usize] = candidate;
                pred[to as usize] = from;
                relaxed = Some(to);
            }
        }
        // Distances stopped moving, so every walk value is final and no cycle
        // can pay for itself.
        relaxed?;
    }

    // `node` is reachable from a positive cycle; `n` steps back along the
    // predecessor chain is inside the cycle rather than merely on the way to it.
    let mut node = relaxed?;
    for _ in 0..n {
        node = *pred.get(node as usize)?;
        if node == u32::MAX {
            return None;
        }
    }

    let mut cycle = vec![node];
    let mut walk = pred[node as usize];
    while walk != node {
        if walk == u32::MAX || cycle.len() > n {
            return None;
        }
        cycle.push(walk);
        walk = pred[walk as usize];
    }
    // Collected backwards along predecessors, so reversing puts it in flying
    // order with the last node closing back onto the first.
    cycle.reverse();
    Some(cycle)
}

/// An edge inside a component, in the component's own numbering.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LocalEdge {
    /// Where it goes.
    pub(crate) to: u32,
    /// What it earns.
    pub(crate) weight: Credits,
    /// What it costs in wall-clock.
    pub(crate) millis: Millis,
}

/// One strongly connected component, re-indexed so a search runs over just its
/// own nodes.
///
/// Every loop solver in this crate works component by component, because a
/// cycle lies wholly inside one and the decomposition turns a single large
/// graph into several small independent ones.
#[derive(Debug)]
pub(crate) struct Component {
    global: Vec<u32>,
    edges: Vec<(u32, u32)>,
    weights: Vec<Credits>,
    times: Vec<Millis>,
}

impl Component {
    /// Every component that can hold a cycle: more than one node, since a
    /// market never trades with itself.
    pub(crate) fn all(graph: &TradeGraph) -> Vec<Self> {
        graph
            .sccs()
            .into_iter()
            .filter(|component| component.len() > 1)
            .map(|component| Self::new(graph, &component))
            .collect()
    }

    /// The component's nodes, in the outer graph's numbering.
    pub(crate) fn nodes(&self) -> &[u32] {
        &self.global
    }

    /// The component's edges, in its own numbering.
    pub(crate) fn edge_list(&self) -> &[(u32, u32)] {
        &self.edges
    }

    /// Out-edges per node, in the component's own numbering.
    pub(crate) fn adjacency(&self) -> Vec<Vec<LocalEdge>> {
        let mut out = vec![Vec::new(); self.global.len()];
        for (i, &(from, to)) in self.edges.iter().enumerate() {
            out[from as usize].push(LocalEdge {
                to,
                weight: self.weights[i],
                millis: self.times[i],
            });
        }
        out
    }

    /// Reduced weights for every edge at a rate, in edge order.
    pub(crate) fn reduced_weights(&self, rate: Ratio) -> Vec<i128> {
        self.weights
            .iter()
            .zip(&self.times)
            .map(|(&weight, &millis)| reduced(rate, weight, millis))
            .collect()
    }

    /// Translates a node list back into the outer graph's numbering.
    pub(crate) fn to_global(&self, local: &[u32]) -> Vec<u32> {
        local.iter().map(|&node| self.global[node as usize]).collect()
    }

    fn new(graph: &TradeGraph, nodes: &[u32]) -> Self {
        let mut local = vec![u32::MAX; graph.len()];
        for (i, &node) in nodes.iter().enumerate() {
            local[node as usize] = i as u32;
        }
        let mut edges = Vec::new();
        let mut weights = Vec::new();
        let mut times = Vec::new();
        for &node in nodes {
            for edge in graph.row(node) {
                let to = graph.target(edge);
                if local[to as usize] == u32::MAX {
                    continue;
                }
                edges.push((local[node as usize], local[to as usize]));
                weights.push(graph.weight(edge));
                times.push(graph.millis(edge));
            }
        }
        Self { global: nodes.to_vec(), edges, weights, times }
    }

    fn beating(&self, rate: Ratio) -> Option<Vec<u32>> {
        let cycle =
            positive_cycle(self.global.len(), &self.edges, &self.reduced_weights(rate))?;
        Some(self.to_global(&cycle))
    }
}

/// The maximum ratio cycle, and the evidence for the claim.
#[derive(Clone, Debug)]
pub struct MaxRatioCycle {
    /// The cycle, in flying order.
    pub nodes: Vec<u32>,
    /// Its exact rate, which is the optimum when `proved`.
    pub rate: Ratio,
    /// How many rounds the iteration took.
    pub rounds: u32,
    /// Whether the iteration reached its stopping condition, which is the
    /// optimality proof.
    pub proved: bool,
}

/// Runs Dinkelbach's iteration over the graph.
///
/// Returns `None` when the graph holds no cycle at all — a sweep of markets
/// that only buy, or only sell, is a legitimate answer and not an error.
#[must_use]
pub fn max_ratio_cycle(graph: &TradeGraph) -> Option<MaxRatioCycle> {
    // A cycle lies wholly inside one strongly connected component, so the
    // search decomposes into small independent problems. Components of one node
    // cannot hold a cycle: a market never trades with itself.
    let components = Component::all(graph);
    if components.is_empty() {
        return None;
    }

    let warm = round::best_ratio(graph);
    let mut rate = warm.map_or(Ratio::ZERO, |best| best.rate);
    let mut witness: Option<Vec<u32>> = warm.map(|best| best.nodes.to_vec());

    for round_index in 1..=MAX_ROUNDS {
        let found = components.iter().find_map(|component| component.beating(rate));
        let Some(cycle) = found else {
            return Some(MaxRatioCycle { nodes: witness?, rate, rounds: round_index, proved: true });
        };
        let (profit, millis, _) = round::price_cycle(graph, &cycle)?;
        let next = Ratio::new(profit, millis);
        debug_assert!(
            next > rate,
            "a cycle with positive reduced weight must beat the rate that reduced it"
        );
        rate = next;
        witness = Some(cycle);
    }

    Some(MaxRatioCycle { nodes: witness?, rate, rounds: MAX_ROUNDS, proved: false })
}

/// The best repeatable loops, best first.
///
/// The head is the maximum ratio cycle and carries the strongest claim the
/// search can make. The rest of the list is the cycles the iteration passed
/// through plus every round trip, ranked — real routes, each one flyable, but
/// only the head's optimality was ever established, so they say so.
#[must_use]
pub fn solve(graph: &TradeGraph, geometry: &Geometry<'_>, limits: &Limits) -> Vec<Route> {
    let Some(best) = max_ratio_cycle(graph) else { return Vec::new() };
    let head_guarantee = if best.proved {
        Guarantee::OptimalForStartingCredits
    } else {
        Guarantee::Heuristic { reason: HeuristicReason::SearchBudgetExhausted }
    };

    let Some(head) = round::route_of(graph, geometry, &best.nodes) else { return Vec::new() };
    round::listing(
        graph,
        geometry,
        limits,
        head.with_guarantee(head_guarantee),
        2..=round::DEFAULT_RUNNER_UP_STOPS,
    )
}

#[cfg(test)]
mod tests {
    use super::{max_ratio_cycle, positive_cycle, reduced};
    use crate::fixture::{geometry, limits, market, ship};
    use crate::graph::{Pools, TradeGraph};
    use crate::num::{Credits, Millis, Ratio};

    #[test]
    fn no_positive_cycle_in_an_acyclic_graph() {
        let edges = [(0u32, 1u32), (1, 2), (2, 3)];
        assert_eq!(positive_cycle(4, &edges, &[10, 10, 10]), None);
    }

    #[test]
    fn no_positive_cycle_when_the_only_cycle_loses() {
        let edges = [(0u32, 1u32), (1, 2), (2, 0)];
        assert_eq!(positive_cycle(3, &edges, &[5, 5, -11]), None);
    }

    #[test]
    fn a_zero_sum_cycle_is_not_positive() {
        // The boundary case the stopping condition turns on: at the optimum
        // every cycle sums to at most zero, and a cycle summing to exactly zero
        // is the optimum itself, not an improvement on it.
        let edges = [(0u32, 1u32), (1, 0)];
        assert_eq!(positive_cycle(2, &edges, &[7, -7]), None);
    }

    #[test]
    fn finds_the_cycle_and_returns_it_in_flying_order() {
        let edges = [(0u32, 1u32), (1, 2), (2, 0), (3, 0)];
        let cycle = positive_cycle(4, &edges, &[5, 5, 5, 1]).expect("a positive cycle");
        assert_eq!(cycle.len(), 3);
        // Every consecutive pair, and the wrap, must be a real edge.
        for i in 0..cycle.len() {
            let pair = (cycle[i], cycle[(i + 1) % cycle.len()]);
            assert!(edges.contains(&pair), "{pair:?} is not an edge of {cycle:?}");
        }
    }

    #[test]
    fn finds_a_cycle_that_is_only_reachable_from_elsewhere() {
        // The positive cycle 1->2->1 is not reachable from 0, and 0 has an edge
        // into it. A single-source Bellman-Ford started at the wrong vertex
        // would miss it; initialising every distance to zero cannot.
        let edges = [(0u32, 1u32), (1, 2), (2, 1)];
        let cycle = positive_cycle(3, &edges, &[1, 5, 5]).expect("a positive cycle");
        assert_eq!(cycle.len(), 2);
        assert!(cycle.contains(&1) && cycle.contains(&2));
    }

    #[test]
    fn reduced_weight_changes_sign_at_the_rate_the_edge_earns() {
        let weight = Credits(1_000);
        let millis = Millis(500);
        let below = Ratio { credits: 1, millis: 1 };
        let equal = Ratio { credits: 2, millis: 1 };
        let above = Ratio { credits: 3, millis: 1 };
        assert!(reduced(below, weight, millis) > 0);
        assert_eq!(reduced(equal, weight, millis), 0);
        assert!(reduced(above, weight, millis) < 0);
    }

    #[test]
    fn a_three_cycle_beats_the_round_trip_it_contains() {
        // Three stations on a line. Each sells what the next one wants, and
        // 0 and 1 also trade back and forth at a thinner margin.
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[(2, 400, 500)]),
            market(2, 1.0, &[(1, 100, 500), (2, 100, 500)], &[(0, 900, 500)]),
            market(3, 2.0, &[(2, 100, 500)], &[(1, 900, 500)]),
        ];
        let geometry = geometry(&markets);
        let graph =
            TradeGraph::build(&Pools::from_markets(&markets), &geometry, &ship(), &limits());
        let best = max_ratio_cycle(&graph).expect("a cycle");
        assert!(best.proved);
        assert_eq!(best.nodes.len(), 3);
    }

    #[test]
    fn a_graph_with_no_cycle_has_no_answer() {
        let markets =
            [market(1, 0.0, &[(0, 100, 500)], &[]), market(2, 5.0, &[], &[(0, 900, 500)])];
        let geometry = geometry(&markets);
        let graph =
            TradeGraph::build(&Pools::from_markets(&markets), &geometry, &ship(), &limits());
        assert!(max_ratio_cycle(&graph).is_none());
    }
}
