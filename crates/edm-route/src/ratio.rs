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
//! # What a round costs, and why there is a budget
//!
//! One round is one Bellman-Ford probe, and a probe is `n` sweeps of the whole
//! edge list. The early exit fires only when nothing relaxed, which is to say
//! only when there is **no positive cycle** — so every round except the last
//! runs all `n` sweeps by construction, and the last one is the cheap one.
//!
//! On a radius-100 sweep that is not a theoretical remark. Measured 2026-08-06
//! over 5,049 cached game-internal API markets: the graph holds 24,292,232 legs in
//! one component of 5,045 nodes, so a probe that cannot exit early is
//! **1.2e11 relaxations**. On a comparable 5,000-market instance where the
//! iteration did improve, one such round took **205 seconds** — in silence,
//! after a two-minute graph build. Five to seven rounds put that in the tens of
//! minutes, and the round count is a property of the data: the same 5,049
//! markets converge in one round, and their first 1,500 take two.
//!
//! Two things follow, and both are in the code below. The iteration reports
//! each round through the caller's [`Watch`], carrying the rate in hand so the
//! answer is visibly improving; and it asks the caller's deadline predicate
//! once per sweep, so a budget can end a probe that is not going to finish.
//! When that happens the result is the witness in hand and `proved` is false —
//! there is no arrangement of the code in which an abandoned probe can be
//! mistaken for a proof, because the probe does not answer with an `Option`.
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
use crate::watch::{Event, Watch};

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
    i128::from(rate.millis) * i128::from(weight.0) - i128::from(rate.credits) * i128::from(millis.0)
}

/// What one probe established.
///
/// Three outcomes and not two, because the difference between the second and
/// the third is the difference between an optimality proof and no claim at all.
/// An `Option` would collapse them, and the collapse is silent: `None` reads as
/// "no cycle beats this rate", which is precisely the stopping condition, so an
/// abandoned probe would return an optimum it never proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// A cycle with strictly positive total weight.
    Found(Vec<u32>),
    /// No cycle has positive total weight. This is the optimality proof.
    Proved,
    /// The caller's budget ran out first. Nothing was established.
    Abandoned,
}

/// Finds a cycle of strictly positive total weight, or proves there is none.
///
/// The unbudgeted form, for callers with an exact claim to make. See
/// [`positive_cycle_within`] for what it does; `None` here means `Proved`,
/// because with no budget there is nothing to abandon.
#[must_use]
pub fn positive_cycle(n: usize, edges: &[(u32, u32)], weights: &[i128]) -> Option<Vec<u32>> {
    match positive_cycle_within(n, edges, weights, Watch::unlimited()) {
        Probe::Found(cycle) => Some(cycle),
        Probe::Proved | Probe::Abandoned => None,
    }
}

/// Finds a cycle of strictly positive total weight, or proves there is none, or
/// gives up because the caller's budget is spent.
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
/// The budget is consulted once per sweep, not once per relaxation: a sweep is
/// the smallest unit whose cost is bounded, and at 24 million legs one of them
/// is tens of milliseconds — fine granularity for a wall clock and far too
/// coarse to notice in the arithmetic.
///
/// Kept deliberately small and free of the surrounding problem: this is the
/// primitive every claim in this module rests on, and it is worth being able to
/// test it against hand-written graphs with no economics anywhere near it.
#[must_use]
pub fn positive_cycle_within(
    n: usize,
    edges: &[(u32, u32)],
    weights: &[i128],
    watch: Watch<'_>,
) -> Probe {
    debug_assert_eq!(edges.len(), weights.len());
    let mut dist = vec![0i128; n];
    let mut pred = vec![u32::MAX; n];
    let mut relaxed = None;

    for _ in 0..n {
        if watch.expired() {
            return Probe::Abandoned;
        }
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
        // can pay for itself. This is the only early exit there is, and it is
        // why a round that *does* find a cycle always costs the full n sweeps.
        if relaxed.is_none() {
            return Probe::Proved;
        }
    }

    // `node` is reachable from a positive cycle; `n` steps back along the
    // predecessor chain is inside the cycle rather than merely on the way to it.
    //
    // The three bail-outs below are unreachable — a chain of n predecessor
    // steps from a node relaxed in the n-th round has a repeat in it, so the
    // walk cannot run off the end — but they answer `Abandoned` rather than
    // `Proved` because of what is known at that point: a relaxation survived
    // the n-th sweep, so a positive cycle *does* exist and failing to extract
    // it is not evidence that it does not.
    let Some(mut node) = relaxed else {
        return Probe::Proved;
    };
    for _ in 0..n {
        let Some(&previous) = pred.get(node as usize) else {
            return Probe::Abandoned;
        };
        node = previous;
        if node == u32::MAX {
            return Probe::Abandoned;
        }
    }

    let mut cycle = vec![node];
    let mut walk = pred[node as usize];
    while walk != node {
        if walk == u32::MAX || cycle.len() > n {
            return Probe::Abandoned;
        }
        cycle.push(walk);
        walk = pred[walk as usize];
    }
    // Collected backwards along predecessors, so reversing puts it in flying
    // order with the last node closing back onto the first.
    cycle.reverse();
    Probe::Found(cycle)
}

/// An edge inside a component, in the component's own numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
///
/// **On game-internal API data it turns one large graph into one large graph.** 89%
/// of ordered market pairs can trade, so the decomposition of a radius-100
/// sweep is one component holding all but a handful of stations plus a
/// singleton for each of those — 5,045 of 5,049 nodes in one piece, measured
/// 2026-08-06. A component is not a small independent problem here, and the
/// copy that re-indexes one is the size of the graph.
///
/// So it does not copy what it can address. An edge's profit and time stay in
/// the graph and the component keeps a [`Membership`]: either "these graph
/// edges, in this order" at four bytes an edge or, when it covers every node,
/// nothing at all. On that instance the copy falls from 556 MiB to 278 MiB,
/// and the search phase's measured peak from **2,561 MiB to 2,299 MiB**.
#[derive(Debug)]
pub(crate) struct Component<'a> {
    graph: &'a TradeGraph,
    global: Vec<u32>,
    edges: Vec<(u32, u32)>,
    membership: Membership,
}

/// Which of the graph's edges a component holds.
///
/// Two shapes because the difference is measurable, not because it is tidy: a
/// component covering every node holds the graph's edges in the graph's own
/// order, so recording *which* ones would be recording the numbers 0, 1, 2, …
/// at four bytes each.
#[derive(Debug)]
enum Membership {
    /// Every edge of the graph, in graph order.
    Whole,
    /// A subset, as graph edge indices, ascending.
    Subset(Vec<u32>),
}

impl<'a> Component<'a> {
    /// Every component that can hold a cycle: more than one node, since a
    /// market never trades with itself.
    pub(crate) fn all(graph: &'a TradeGraph) -> Vec<Self> {
        let sccs = graph.sccs();
        // The whole-graph shortcut, which is worth four bytes an edge over the
        // general path and — measured 2026-08-06 — did not fire on a real
        // 5,049-market sweep at all, because four of those stations trade with
        // nobody and each is a component of its own. The general path is what
        // carries the saving; this is the last four bytes of it.
        if sccs.len() == 1 && sccs[0].len() == graph.len() && graph.len() > 1 {
            return vec![Self::whole(graph)];
        }
        sccs.into_iter()
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
            let (weight, millis) = self.leg(i);
            out[from as usize].push(LocalEdge { to, weight, millis });
        }
        out
    }

    /// Reduced weights for every edge at a rate, into a buffer the caller owns.
    ///
    /// The buffer rather than a fresh `Vec`, because the size never changes and
    /// the loop above it runs once per Dinkelbach round: at 24 million legs
    /// that is 371 MiB allocated, filled and dropped per round to hold exactly
    /// what the previous round held. Clearing keeps the capacity, so only the
    /// first round allocates. It is churn rather than peak — the allocator
    /// hands the same pages back — which is why the measured saving above is
    /// the `Membership` change and not this one.
    ///
    /// The `Membership` is matched once, outside the loop, not once per edge.
    pub(crate) fn reduce_into(&self, rate: Ratio, buffer: &mut Vec<i128>) {
        buffer.clear();
        buffer.reserve(self.edges.len());
        match &self.membership {
            Membership::Whole => buffer.extend(
                self.graph
                    .weights()
                    .iter()
                    .zip(self.graph.times())
                    .map(|(&weight, &millis)| reduced(rate, weight, millis)),
            ),
            Membership::Subset(index) => buffer.extend(index.iter().map(|&edge| {
                reduced(
                    rate,
                    self.graph.weight(edge as usize),
                    self.graph.millis(edge as usize),
                )
            })),
        }
    }

    /// Translates a node list back into the outer graph's numbering.
    pub(crate) fn to_global(&self, local: &[u32]) -> Vec<u32> {
        local
            .iter()
            .map(|&node| self.global[node as usize])
            .collect()
    }

    /// What the component's `i`-th edge earns and costs.
    fn leg(&self, i: usize) -> (Credits, Millis) {
        let edge = match &self.membership {
            Membership::Whole => i,
            Membership::Subset(index) => index[i] as usize,
        };
        (self.graph.weight(edge), self.graph.millis(edge))
    }

    /// The whole graph as one component, recording no membership at all.
    ///
    /// Sound because the identity is exactly what [`Component::new`] computes
    /// here: it visits nodes in ascending order and, when every node is a
    /// member, emits their rows in ascending order too — which is compressed
    /// sparse row order, which is edge-index order.
    /// `the_whole_graph_shortcut_agrees_with_the_general_path` asserts that
    /// against the general constructor rather than leaving it to this
    /// paragraph.
    ///
    /// The edge pairs are still built: the graph stores targets and row
    /// offsets, not `(from, to)` pairs, and the probe below takes pairs.
    fn whole(graph: &'a TradeGraph) -> Self {
        Self {
            graph,
            global: (0..graph.len() as u32).collect(),
            edges: graph.edges().map(|(from, to, _)| (from, to)).collect(),
            membership: Membership::Whole,
        }
    }

    fn new(graph: &'a TradeGraph, nodes: &[u32]) -> Self {
        let mut local = vec![u32::MAX; graph.len()];
        for (i, &node) in nodes.iter().enumerate() {
            local[node as usize] = i as u32;
        }
        let mut edges = Vec::new();
        let mut index = Vec::new();
        for &node in nodes {
            for edge in graph.row(node) {
                let to = graph.target(edge);
                if local[to as usize] == u32::MAX {
                    continue;
                }
                edges.push((local[node as usize], local[to as usize]));
                index.push(edge as u32);
            }
        }
        Self {
            graph,
            global: nodes.to_vec(),
            edges,
            membership: Membership::Subset(index),
        }
    }

    fn beating(&self, rate: Ratio, reduced: &mut Vec<i128>, watch: Watch<'_>) -> Probe {
        self.reduce_into(rate, reduced);
        match positive_cycle_within(self.global.len(), &self.edges, reduced, watch) {
            Probe::Found(cycle) => Probe::Found(self.to_global(&cycle)),
            other => other,
        }
    }

    /// Whether this component took the whole-graph shortcut. Tests only:
    /// nothing else can see the difference, which is the point.
    #[cfg(test)]
    pub(crate) fn covers_the_graph(&self) -> bool {
        matches!(self.membership, Membership::Whole)
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
///
/// `proved` is false, and the caller must degrade its claim accordingly, in
/// exactly three cases: the round cap fired, the caller's budget expired
/// between rounds, or a probe was abandoned part way. There is no fourth.
#[must_use]
pub fn max_ratio_cycle(graph: &TradeGraph, watch: Watch<'_>) -> Option<MaxRatioCycle> {
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
    // Hoisted: one buffer for the whole iteration, sized once. See
    // `Component::reduce_into`.
    let mut reduced = Vec::new();

    for round_index in 1..=MAX_ROUNDS {
        watch.report(Event::Round {
            round: round_index,
            rate,
            stops: witness.as_ref().map_or(0, Vec::len),
        });
        let mut found = None;
        let mut abandoned = watch.expired();
        for component in &components {
            if abandoned {
                break;
            }
            match component.beating(rate, &mut reduced, watch) {
                Probe::Found(cycle) => {
                    found = Some(cycle);
                    break;
                }
                Probe::Proved => {}
                Probe::Abandoned => abandoned = true,
            }
        }
        if abandoned {
            // Withdraw a claim only when there is one to withdraw. `witness` is
            // `None` when the warm start found no two-cycle, and `None` out of
            // this function means "the graph holds no cycle at all" — so
            // reporting `Abandoned` before that `?` told the caller two
            // contradictory things and printed "reporting the best route it
            // had" for a route that was never going to appear.
            let nodes = witness?;
            watch.report(Event::Abandoned);
            return Some(MaxRatioCycle {
                nodes,
                rate,
                rounds: round_index,
                proved: false,
            });
        }
        let Some(cycle) = found else {
            return Some(MaxRatioCycle {
                nodes: witness?,
                rate,
                rounds: round_index,
                proved: true,
            });
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

    Some(MaxRatioCycle {
        nodes: witness?,
        rate,
        rounds: MAX_ROUNDS,
        proved: false,
    })
}

/// The best repeatable loops, best first.
///
/// The head is the maximum ratio cycle and carries the strongest claim the
/// search can make. The rest of the list is the cycles the iteration passed
/// through plus every round trip, ranked — real routes, each one flyable, but
/// only the head's optimality was ever established, so they say so.
#[must_use]
pub fn solve(
    graph: &TradeGraph,
    geometry: &Geometry<'_>,
    limits: &Limits,
    watch: Watch<'_>,
) -> Vec<Route> {
    let Some(best) = max_ratio_cycle(graph, watch) else {
        return Vec::new();
    };
    let head_guarantee = if best.proved {
        Guarantee::OptimalForStartingCredits
    } else {
        Guarantee::Heuristic {
            reason: HeuristicReason::SearchBudgetExhausted,
        }
    };

    let Some(head) = round::route_of(graph, geometry, &best.nodes) else {
        return Vec::new();
    };
    let mut listing = round::listing(
        graph,
        geometry,
        limits,
        head.with_guarantee(head_guarantee),
        2..=round::DEFAULT_RUNNER_UP_STOPS,
    );

    round::taint_unfinished(&mut listing, best.proved);
    listing
}

#[cfg(test)]
mod tests {
    use super::{
        Component, Probe, max_ratio_cycle, positive_cycle, positive_cycle_within, reduced,
    };
    use crate::fixture::{geometry, limits, market, ship};
    use crate::graph::{Pools, TradeGraph};
    use crate::model::Market;
    use crate::num::{Credits, Millis, Ratio};
    use crate::report::{Guarantee, HeuristicReason};
    use crate::watch::{Event, Watch};

    /// Three stations, each selling what the next one wants, with 0 and 1 also
    /// trading back and forth at a thinner margin. Dinkelbach needs a second
    /// round here: the round-trip warm start is not the optimum.
    fn triangle() -> [Market; 3] {
        [
            market(1, 0.0, &[(0, 100, 500)], &[(2, 400, 500)]),
            market(2, 1.0, &[(1, 100, 500), (2, 100, 500)], &[(0, 900, 500)]),
            market(3, 2.0, &[(2, 100, 500)], &[(1, 900, 500)]),
        ]
    }

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
            assert!(
                edges.contains(&pair),
                "{pair:?} is not an edge of {cycle:?}"
            );
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
        let below = Ratio {
            credits: 1,
            millis: 1,
        };
        let equal = Ratio {
            credits: 2,
            millis: 1,
        };
        let above = Ratio {
            credits: 3,
            millis: 1,
        };
        assert!(reduced(below, weight, millis) > 0);
        assert_eq!(reduced(equal, weight, millis), 0);
        assert!(reduced(above, weight, millis) < 0);
    }

    #[test]
    fn a_three_cycle_beats_the_round_trip_it_contains() {
        let markets = triangle();
        let best = max_ratio_cycle(&build(&markets), Watch::unlimited()).expect("a cycle");
        assert!(best.proved);
        assert_eq!(best.nodes.len(), 3);
    }

    #[test]
    fn a_graph_with_no_cycle_has_no_answer() {
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[]),
            market(2, 5.0, &[], &[(0, 900, 500)]),
        ];
        assert!(max_ratio_cycle(&build(&markets), Watch::unlimited()).is_none());
    }

    #[test]
    fn an_abandoned_probe_is_not_a_proof_that_no_cycle_beats_the_rate() {
        // The tri-state exists for exactly this: `Proved` and `Abandoned` are
        // both "no cycle came back", and only one of them is a claim.
        let edges = [(0u32, 1u32), (1, 0)];
        let spent = || true;
        assert_eq!(
            positive_cycle_within(2, &edges, &[5, 5], Watch::unlimited().until(&spent)),
            Probe::Abandoned
        );
        assert_eq!(
            positive_cycle_within(2, &edges, &[5, -5], Watch::unlimited()),
            Probe::Proved
        );
    }

    #[test]
    fn an_exhausted_budget_returns_the_warm_start_and_claims_nothing() {
        let markets = triangle();
        let graph = build(&markets);
        let free = max_ratio_cycle(&graph, Watch::unlimited()).expect("a cycle");
        assert!(free.proved, "the same instance is provable with no budget");
        assert_eq!(free.nodes.len(), 3);

        let spent = || true;
        let stopped =
            max_ratio_cycle(&graph, Watch::unlimited().until(&spent)).expect("the warm start");
        assert!(
            !stopped.proved,
            "nothing was searched, so nothing was proved"
        );
        // The round trip the warm start found is a real route and is still
        // returned; it is the *claim* that is withdrawn, not the answer.
        assert_eq!(stopped.nodes.len(), 2);
        assert!(stopped.rate < free.rate);
    }

    #[test]
    fn an_exhausted_budget_downgrades_the_head_to_search_budget_exhausted() {
        let markets = triangle();
        let graph = build(&markets);
        let geometry = geometry(&markets);
        let spent = || true;
        let routes = super::solve(
            &graph,
            &geometry,
            &limits(),
            Watch::unlimited().until(&spent),
        );
        assert_eq!(
            routes[0].guarantee,
            Guarantee::Heuristic {
                reason: HeuristicReason::SearchBudgetExhausted
            }
        );
    }

    #[test]
    fn every_round_is_reported_with_a_rate_that_never_falls() {
        let markets = triangle();
        let graph = build(&markets);
        let seen = std::cell::RefCell::new(Vec::new());
        let sink = |event: Event| {
            if let Event::Round { round, rate, stops } = event {
                seen.borrow_mut().push((round, rate, stops));
            }
        };
        let best = max_ratio_cycle(&graph, Watch::unlimited().reporting(&sink)).expect("a cycle");
        let seen = seen.into_inner();
        assert_eq!(
            seen.len(),
            best.rounds as usize,
            "one report per round: {seen:?}"
        );
        // Round numbers count from one, and the rate a watcher is shown is the
        // rate of a cycle that exists — so it can only climb.
        for (i, &(round, rate, stops)) in seen.iter().enumerate() {
            assert_eq!(round, i as u32 + 1);
            assert!(stops >= 2, "the witness is a real cycle by round {round}");
            if i > 0 {
                assert!(rate > seen[i - 1].1, "{seen:?}");
            }
        }
        assert!(
            seen.len() >= 2,
            "this instance improves on its warm start: {seen:?}"
        );
    }

    #[test]
    fn the_whole_graph_shortcut_agrees_with_the_general_path() {
        // Recording no membership at all is sound only because the identity
        // re-indexing is exactly what the general constructor computes. Assert
        // that against the constructor rather than trusting the argument.
        let markets = triangle();
        let graph = build(&markets);
        let components = Component::all(&graph);
        assert_eq!(components.len(), 1);
        assert!(components[0].covers_the_graph());

        let nodes: Vec<u32> = (0..graph.len() as u32).collect();
        let general = Component::new(&graph, &nodes);
        assert!(!general.covers_the_graph());
        assert_eq!(components[0].nodes(), general.nodes());
        assert_eq!(components[0].edge_list(), general.edge_list());
        assert_eq!(components[0].adjacency(), general.adjacency());
        let rate = Ratio {
            credits: 1,
            millis: 3,
        };
        let (mut shortcut_weights, mut general_weights) = (Vec::new(), Vec::new());
        components[0].reduce_into(rate, &mut shortcut_weights);
        general.reduce_into(rate, &mut general_weights);
        assert_eq!(shortcut_weights, general_weights);
    }

    #[test]
    fn one_station_that_trades_with_nobody_takes_the_general_path() {
        // The shape a real sweep has, and the reason the shortcut is not the
        // fix: a single station that neither buys nor sells is a component of
        // its own, so the graph is no longer one component even though every
        // cycle in it lies in the same one. Measured 2026-08-06: 5,045 of 5,049
        // nodes in one piece and four singletons.
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[(1, 900, 500)]),
            market(2, 1.0, &[(1, 100, 500)], &[(0, 900, 500)]),
            market(3, 40.0, &[], &[]),
        ];
        let graph = build(&markets);
        let components = Component::all(&graph);
        assert_eq!(components.len(), 1);
        assert!(!components[0].covers_the_graph());
        assert_eq!(components[0].nodes(), [0, 1]);
        // And the answer is the same one the shortcut would have given.
        assert!(
            max_ratio_cycle(&graph, Watch::unlimited())
                .expect("a cycle")
                .proved
        );
    }

    #[test]
    fn a_disconnected_graph_still_splits_into_components() {
        // The shortcut must not swallow the case it is not for: two separate
        // two-cycles are two components, each smaller than the graph.
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[(1, 900, 500)]),
            market(2, 1.0, &[(1, 100, 500)], &[(0, 900, 500)]),
            market(3, 40.0, &[(2, 100, 500)], &[(3, 900, 500)]),
            market(4, 41.0, &[(3, 100, 500)], &[(2, 900, 500)]),
        ];
        let graph = build(&markets);
        let components = Component::all(&graph);
        assert_eq!(components.len(), 2);
        assert!(
            components
                .iter()
                .all(|component| !component.covers_the_graph())
        );
        assert!(
            components
                .iter()
                .all(|component| component.nodes().len() == 2)
        );
    }

    #[test]
    fn the_reduced_weight_buffer_is_refilled_and_not_appended_to() {
        let markets = triangle();
        let graph = build(&markets);
        let component = Component::all(&graph).pop().expect("a component");
        let edges = component.edge_list().len();
        let mut buffer = vec![i128::MIN; 99];
        component.reduce_into(Ratio::ZERO, &mut buffer);
        assert_eq!(buffer.len(), edges);
        let first = buffer.clone();
        component.reduce_into(
            Ratio {
                credits: 1,
                millis: 1,
            },
            &mut buffer,
        );
        assert_eq!(buffer.len(), edges, "the second fill replaced the first");
        assert_ne!(buffer, first, "and it used the new rate");
    }
}
