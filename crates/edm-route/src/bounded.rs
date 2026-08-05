//! The best repeatable loop of at most `k` stops.
//!
//! Same Dinkelbach iteration as the unbounded solver, wrapped around a layered
//! `(max, +)` dynamic program instead of Bellman-Ford. At a fixed rate the
//! question "is there a closed walk of at most `k` legs with positive reduced
//! weight?" is answered by `k` relaxation sweeps from each origin, which is
//! `O(k·m)` per origin, and the answer improves the rate exactly as before.
//!
//! # Why a dynamic program is enough, where a depth-first search was not
//!
//! The DP optimises over closed **walks**, which may revisit a station, and a
//! route that revisits a station is not a route anyone wants. Under a *ratio*
//! objective that costs nothing: a closed walk decomposes into simple cycles,
//! the mediant inequality says one of the pieces is at least as good as the
//! whole, and — the part that matters here — **that piece is also no longer**.
//! So the bounded-`k` walk optimum equals the bounded-`k` *simple cycle*
//! optimum, and "distinct stations" comes free rather than costing an
//! exponential search.
//!
//! `edtrade/src/solve/circuit.ts` needed a depth-first search precisely because
//! it maximised *profit*, where a DP happily emits `u→v→u→v` as a four-leg
//! circuit worth twice the two-leg one. Under a ratio that walk can never beat
//! the two-cycle it decomposes into, so the same DP becomes exact.
//!
//! The witness the DP returns may still be a walk, so it is decomposed before
//! anything is reported: a route this crate hands back always visits each of
//! its stations once.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::graph::TradeGraph;
use crate::model::Limits;
use crate::num::Ratio;
use crate::ratio::Component;
use crate::report::{Guarantee, HeuristicReason, Route};
use crate::round;
use crate::time::Geometry;
use crate::watch::{Event, Watch};

/// See `ratio::MAX_ROUNDS`; the same argument applies to this iteration.
const MAX_ROUNDS: u32 = 4_096;

/// A value no walk can reach, used as the empty cell of the dynamic program.
///
/// Far below any reachable reduced weight (which is bounded by 2^84) and far
/// above `i128::MIN`, so adding an edge weight to it cannot wrap.
const UNREACHABLE: i128 = i128::MIN / 4;

/// The best loop of at most `k` stops, and the evidence for the claim.
#[derive(Clone, Debug)]
pub struct BoundedCycle {
    /// The cycle, in flying order. Always simple, and never longer than `k`.
    pub nodes: Vec<u32>,
    /// Its exact rate.
    pub rate: Ratio,
    /// Whether the iteration reached its stopping condition.
    pub proved: bool,
}

/// What one sweep over every origin established.
///
/// Three outcomes for the same reason `ratio::Probe` has three: `Exhausted` is
/// the stopping condition and therefore the optimality proof, `Abandoned` is
/// the absence of one, and an `Option` cannot tell them apart.
enum Step {
    /// A simple cycle that beats the rate, and the rate it achieves.
    Improved(Vec<u32>, Ratio),
    /// Every origin was searched and nothing beats the rate.
    Exhausted,
    /// The caller's budget ran out first.
    Abandoned,
}

/// Runs the bounded-length search.
///
/// `k` is a cap on the number of stops, which for a cycle is also the number of
/// legs. A `k` below two admits nothing: a cycle needs two stations.
///
/// The dynamic program is `O(k·m)` per origin and there are `n` origins, so one
/// round is `O(n·k·m)` — worse than the unbounded solver's probe, not better.
/// At the 5,049 markets and 24 million legs a radius-100 sweep produces that is
/// hours, which is what the budget is for; when it fires, `proved` is false.
#[must_use]
pub fn best_bounded(graph: &TradeGraph, k: usize, watch: Watch<'_>) -> Option<BoundedCycle> {
    if k < 2 {
        return None;
    }
    let components = Component::all(graph);
    if components.is_empty() {
        return None;
    }

    // The warm start is only usable if it fits under the cap, which a two-cycle
    // always does.
    let warm = round::best_ratio(graph);
    let mut rate = warm.map_or(Ratio::ZERO, |best| best.rate);
    let mut witness: Option<Vec<u32>> = warm.map(|best| best.nodes.to_vec());
    // Hoisted out of the round loop; see `Component::reduce_into`.
    let mut reduced = Vec::new();

    for round_index in 1..=MAX_ROUNDS {
        watch.report(Event::Round {
            round: round_index,
            rate,
            stops: witness.as_ref().map_or(0, Vec::len),
        });
        match improve(graph, &components, rate, Shape { k, watch }, &mut reduced) {
            Step::Exhausted => return Some(BoundedCycle { nodes: witness?, rate, proved: true }),
            Step::Abandoned => {
                // See `ratio::max_ratio_cycle`: the withdrawal is reported only
                // when there is a route to withdraw the claim about.
                let nodes = witness?;
                watch.report(Event::Abandoned);
                return Some(BoundedCycle { nodes, rate, proved: false });
            }
            Step::Improved(nodes, improved) => {
                debug_assert!(improved > rate, "an improving walk must beat the rate it found");
                debug_assert!(nodes.len() <= k, "decomposition never lengthens a walk");
                rate = improved;
                witness = Some(nodes);
            }
        }
    }

    Some(BoundedCycle { nodes: witness?, rate, proved: false })
}

/// The cap the search runs under, and the budget it runs against.
#[derive(Clone, Copy)]
struct Shape<'a> {
    k: usize,
    watch: Watch<'a>,
}

/// Finds a simple cycle of at most `k` stops that beats `rate`, if one exists.
fn improve(
    graph: &TradeGraph,
    components: &[Component<'_>],
    rate: Ratio,
    shape: Shape<'_>,
    reduced: &mut Vec<i128>,
) -> Step {
    let k = shape.k;
    for component in components {
        component.reduce_into(rate, reduced);
        let edges = component.edge_list();
        let n = component.nodes().len();

        for origin in 0..n as u32 {
            // Once per origin: one origin's dynamic program is `O(k·m)`, which
            // is the smallest unit here whose cost is bounded.
            if shape.watch.expired() {
                return Step::Abandoned;
            }
            let Some(walk) = best_closed_walk(n, edges, reduced, origin, k) else { continue };
            let global = component.to_global(&walk);
            // The walk beats `rate`; by the mediant inequality one of its
            // simple pieces does too, and no piece is longer than the walk.
            let mut best: Option<(Ratio, Vec<u32>)> = None;
            for piece in decompose(&global) {
                let Some((profit, millis, _)) = round::price_cycle(graph, &piece) else {
                    continue;
                };
                let candidate = Ratio::new(profit, millis);
                let better = match &best {
                    None => true,
                    Some((rate_so_far, nodes_so_far)) => match candidate.cmp(rate_so_far) {
                        Ordering::Greater => true,
                        Ordering::Equal => piece < *nodes_so_far,
                        Ordering::Less => false,
                    },
                };
                if better {
                    best = Some((candidate, piece));
                }
            }
            if let Some((candidate, piece)) = best {
                debug_assert!(
                    candidate > rate,
                    "the mediant inequality guarantees a piece at least as good as the walk"
                );
                if candidate > rate {
                    return Step::Improved(piece, candidate);
                }
            }
        }
    }
    Step::Exhausted
}

/// The best closed walk of at most `k` legs from `origin`, if it is positive.
///
/// Layered `(max, +)`: `layer[l][v]` is the best reduced value of a walk of
/// exactly `l` legs from the origin to `v`.
fn best_closed_walk(
    n: usize,
    edges: &[(u32, u32)],
    reduced: &[i128],
    origin: u32,
    k: usize,
) -> Option<Vec<u32>> {
    let mut layers: Vec<Vec<i128>> = vec![vec![UNREACHABLE; n]; k + 1];
    let mut parents: Vec<Vec<u32>> = vec![vec![u32::MAX; n]; k + 1];
    layers[0][origin as usize] = 0;

    let mut best: Option<(i128, usize)> = None;
    for legs in 1..=k {
        for (i, &(from, to)) in edges.iter().enumerate() {
            let previous = layers[legs - 1][from as usize];
            if previous == UNREACHABLE {
                continue;
            }
            let candidate = previous + reduced[i];
            if candidate > layers[legs][to as usize] {
                layers[legs][to as usize] = candidate;
                parents[legs][to as usize] = from;
            }
        }
        // A closed walk needs at least two legs; one leg back to the origin
        // would be a self-loop, which no market has.
        if legs >= 2 {
            let closed = layers[legs][origin as usize];
            if closed > 0 && best.is_none_or(|(value, _)| closed > value) {
                best = Some((closed, legs));
            }
        }
    }

    let (_, legs) = best?;
    let mut walk = Vec::with_capacity(legs);
    let mut node = origin;
    for layer in (1..=legs).rev() {
        walk.push(node);
        node = parents[layer][node as usize];
        if node == u32::MAX {
            return None;
        }
    }
    walk.reverse();
    Some(walk)
}

/// Splits a closed walk into simple cycles.
///
/// Consecutive entries of the input are edges, and the last closes back onto
/// the first. Every piece returned obeys the same convention, and no piece is
/// longer than the input — which is what makes the bounded search exact.
fn decompose(walk: &[u32]) -> Vec<Vec<u32>> {
    let mut position: HashMap<u32, usize> = HashMap::new();
    let mut stack: Vec<u32> = Vec::new();
    let mut pieces = Vec::new();

    for &node in walk {
        if let Some(&at) = position.get(&node) {
            let piece: Vec<u32> = stack[at..].to_vec();
            for member in &piece {
                position.remove(member);
            }
            stack.truncate(at);
            if piece.len() >= 2 {
                pieces.push(piece);
            }
        }
        position.insert(node, stack.len());
        stack.push(node);
    }
    if stack.len() >= 2 {
        pieces.push(stack);
    }
    pieces
}

/// The best bounded-length loops, best first.
///
/// As in the unbounded solver, the head carries the claim and the rest of the
/// list is labelled for what it is.
#[must_use]
pub fn solve(
    graph: &TradeGraph,
    geometry: &Geometry<'_>,
    limits: &Limits,
    k: usize,
    watch: Watch<'_>,
) -> Vec<Route> {
    let Some(best) = best_bounded(graph, k, watch) else { return Vec::new() };
    let head_guarantee = if best.proved {
        Guarantee::OptimalForStartingCredits
    } else {
        Guarantee::Heuristic { reason: HeuristicReason::SearchBudgetExhausted }
    };
    let Some(head) = round::route_of(graph, geometry, &best.nodes) else { return Vec::new() };
    let mut listing =
        round::listing(graph, geometry, limits, head.with_guarantee(head_guarantee), 2..=k);
    round::taint_unfinished(&mut listing, best.proved);
    listing
}

#[cfg(test)]
mod tests {
    use super::{best_bounded, decompose};
    use crate::fixture::{geometry, limits, market, ship};
    use crate::graph::{Pools, TradeGraph};
    use crate::model::Market;
    use crate::ratio;
    use crate::report::{Guarantee, HeuristicReason};
    use crate::watch::Watch;

    /// A three-cycle worth more per hour than either two-cycle inside it.
    fn triangle() -> [Market; 3] {
        [
            market(1, 0.0, &[(0, 100, 500)], &[(2, 900, 500)]),
            market(2, 1.0, &[(1, 100, 500), (2, 100, 500)], &[(0, 900, 500)]),
            market(3, 2.0, &[(2, 100, 500)], &[(1, 900, 500)]),
        ]
    }

    /// The same triangle with the closing leg thinned, so the round-trip warm
    /// start is *not* the answer and a round of the iteration has to happen for
    /// the three-cycle to be found. A budget that expires first therefore
    /// changes the answer as well as the claim, which is what makes the test
    /// below able to fail.
    fn improving_triangle() -> [Market; 3] {
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
    fn a_walk_that_repeats_a_station_splits_into_two_cycles() {
        // u -> a -> b -> a -> u: an inner two-cycle and an outer one.
        let pieces = decompose(&[0, 1, 2, 1]);
        assert_eq!(pieces.len(), 2);
        assert!(pieces.contains(&vec![1, 2]));
        assert!(pieces.contains(&vec![0, 1]));
    }

    #[test]
    fn a_simple_cycle_decomposes_to_itself() {
        assert_eq!(decompose(&[3, 1, 2]), vec![vec![3, 1, 2]]);
    }

    #[test]
    fn no_piece_is_longer_than_the_walk_it_came_from() {
        for walk in [vec![0u32, 1, 2, 1, 3], vec![0, 1, 0, 2, 0], vec![5, 5], vec![0, 1, 2, 3]] {
            for piece in decompose(&walk) {
                assert!(piece.len() <= walk.len(), "{piece:?} from {walk:?}");
            }
        }
    }

    #[test]
    fn a_cap_of_two_is_exactly_the_round_trip() {
        // Under a cap of two the three-cycle must not be found, and the
        // two-cycle must be.
        let markets = triangle();
        let graph = build(&markets);
        let two = best_bounded(&graph, 2, Watch::unlimited()).expect("a two-cycle");
        assert_eq!(two.nodes.len(), 2);
        assert!(two.proved);
        assert_eq!(two.rate, crate::round::best_ratio(&graph).expect("a round trip").rate);
    }

    #[test]
    fn a_cap_at_the_graph_size_agrees_with_the_unbounded_solver() {
        let markets = triangle();
        let graph = build(&markets);
        let free = ratio::max_ratio_cycle(&graph, Watch::unlimited()).expect("a cycle");
        let capped =
            best_bounded(&graph, markets.len(), Watch::unlimited()).expect("a cycle");
        assert_eq!(free.rate, capped.rate);
    }

    #[test]
    fn every_answer_is_a_simple_cycle() {
        let markets = triangle();
        let graph = build(&markets);
        for k in 2..=6 {
            let Some(found) = best_bounded(&graph, k, Watch::unlimited()) else { continue };
            let mut sorted = found.nodes.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), found.nodes.len(), "{:?} revisits a station", found.nodes);
        }
    }

    #[test]
    fn an_exhausted_budget_never_claims_the_bounded_optimum() {
        let markets = improving_triangle();
        let graph = build(&markets);
        let free = best_bounded(&graph, 3, Watch::unlimited()).expect("a cycle");
        assert!(free.proved, "the same instance is provable with no budget");
        assert_eq!(free.nodes.len(), 3);

        let spent = || true;
        let stopped =
            best_bounded(&graph, 3, Watch::unlimited().until(&spent)).expect("the warm start");
        assert!(!stopped.proved);
        assert_eq!(stopped.nodes.len(), 2, "the round-trip warm start, unimproved");

        let routes = super::solve(
            &graph,
            &geometry(&markets),
            &limits(),
            3,
            Watch::unlimited().until(&spent),
        );
        assert_eq!(
            routes[0].guarantee,
            Guarantee::Heuristic { reason: HeuristicReason::SearchBudgetExhausted }
        );
    }
}
