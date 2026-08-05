//! Exact round trips, and the warm start for the ratio solver.
//!
//! A round trip is a two-cycle, so it is a special case of the loop search and
//! not a different question. It is solved separately anyway for two reasons:
//! it is exhaustive at a cost of one binary search per edge, and its optimum is
//! an *achievable* rate, which is exactly what Dinkelbach needs to start from.
//! Starting the ratio iteration at a real cycle's ratio rather than at zero
//! removes the iterations that would otherwise be spent climbing out of the
//! shallows.
//!
//! The outbound and return commodities are independent argmaxes and differ
//! naturally: a market's buy price always exceeds its sell price, so buying
//! back what you have just sold is a loss and the argmax never selects it.

use crate::graph::TradeGraph;
use crate::model::Limits;
use crate::num::{Credits, Millis, Ratio};
use crate::report::{Guarantee, HeuristicReason, RankKey, Route};
use crate::time::Geometry;
use crate::topn::TopN;

/// The best round trips, best first.
#[must_use]
pub fn solve(graph: &TradeGraph, geometry: &Geometry<'_>, limits: &Limits) -> Vec<Route> {
    let capacity = limits.top_n.saturating_mul(limits.shortlist_factor.max(1));
    let mut heap: TopN<RankKey, Route> = TopN::new(capacity);

    for (from, to, out) in graph.edges() {
        // Each unordered pair is considered once: a two-cycle read from the
        // other end is the same route, and the ranking key would collapse them
        // anyway, but generating one of them is cheaper than deduplicating two.
        if from >= to {
            continue;
        }
        let Some(back) = graph.find(to, from) else { continue };
        let route = Route::cycle(
            geometry,
            &[from, to],
            &[graph.choice(out), graph.choice(back)],
        );
        heap.offer(route.rank.clone(), route);
    }

    heap.drain()
}

/// A two-cycle and its exact rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Best {
    /// The two stations, in flying order.
    pub nodes: [u32; 2],
    /// The edges joining them, in the same order.
    pub edges: [usize; 2],
    /// The rate the cycle earns when flown repeatedly.
    pub rate: Ratio,
}

/// The best round trip's rate, as a warm start for the ratio solver.
///
/// Ties are broken by station index so the warm start does not depend on the
/// order the graph happened to be built in — the whole iteration below it is
/// deterministic, and beginning it from a coin flip would undo that.
#[must_use]
pub fn best_ratio(graph: &TradeGraph) -> Option<Best> {
    let mut best: Option<Best> = None;
    for (from, to, out) in graph.edges() {
        if from >= to {
            continue;
        }
        let Some(back) = graph.find(to, from) else { continue };
        let profit = graph.weight(out) + graph.weight(back);
        let millis = graph.millis(out) + graph.millis(back);
        let rate = Ratio::new(profit, millis);
        let candidate = Best { nodes: [from, to], edges: [out, back], rate };
        let better = match &best {
            None => true,
            Some(current) => match rate.cmp(&current.rate) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => candidate.nodes < current.nodes,
                std::cmp::Ordering::Less => false,
            },
        };
        if better {
            best = Some(candidate);
        }
    }
    best
}

/// Profit and time of a cycle given as a node list, or `None` if some leg of it
/// is not an edge.
///
/// Shared by every loop solver so that a witness cycle is always priced the
/// same way, whichever search produced it.
#[must_use]
pub fn price_cycle(graph: &TradeGraph, nodes: &[u32]) -> Option<(Credits, Millis, Vec<usize>)> {
    if nodes.len() < 2 {
        return None;
    }
    let mut profit = Credits::ZERO;
    let mut millis = Millis::ZERO;
    let mut edges = Vec::with_capacity(nodes.len());
    for (i, &from) in nodes.iter().enumerate() {
        let to = nodes[(i + 1) % nodes.len()];
        let edge = graph.find(from, to)?;
        profit += graph.weight(edge);
        millis += graph.millis(edge);
        edges.push(edge);
    }
    Some((profit, millis, edges))
}

/// Turns a cycle of nodes into a route, or `None` if it is not a cycle in this
/// graph.
///
/// Every loop solver ends here, so a witness is priced the same way whichever
/// search produced it and two solvers cannot disagree about what a cycle earns.
#[must_use]
pub fn route_of(graph: &TradeGraph, geometry: &Geometry<'_>, nodes: &[u32]) -> Option<Route> {
    let (_, _, edges) = price_cycle(graph, nodes)?;
    let choices: Vec<crate::weight::LegChoice> =
        edges.iter().map(|&edge| graph.choice(edge)).collect();
    Some(Route::cycle(geometry, nodes, &choices))
}

/// How many stops a runner-up listing enumerates when nothing caps the shape.
///
/// The proved head can be any length; the *rest* of a listing is a convenience,
/// and enumerating short cycles exhaustively is worth more than enumerating
/// long ones partially.
pub const DEFAULT_RUNNER_UP_STOPS: usize = 4;

/// How many partial paths a runner-up listing may expand.
pub const RUNNER_UP_BUDGET: u64 = 200_000;

/// Simple cycles of at most `max_stops` stops, in a permutation-invariant order.
///
/// This exists because a listing has to be reproducible. The cycles Dinkelbach
/// passes through on its way to the optimum depend on the order the sweep
/// returned markets in — sixteen workers finish in a different order every run
/// — so using them as the runner-up list would make the second row of the
/// report depend on network timing. Enumerating instead, with stations and
/// their neighbours visited in **market id** order, gives the same set of
/// cycles for any permutation of the same instance, including when the budget
/// cuts the search short.
#[must_use]
pub fn enumerate_cycles(
    graph: &TradeGraph,
    geometry: &Geometry<'_>,
    max_stops: usize,
    budget: u64,
) -> Vec<Vec<u32>> {
    if max_stops < 2 || graph.is_empty() {
        return Vec::new();
    }
    let id = |node: u32| geometry.markets[node as usize].market_id;

    let mut order: Vec<u32> = (0..graph.len() as u32).collect();
    order.sort_by_key(|&node| id(node));
    let mut rank = vec![0u32; graph.len()];
    for (position, &node) in order.iter().enumerate() {
        rank[node as usize] = position as u32;
    }
    let neighbours: Vec<Vec<u32>> = (0..graph.len() as u32)
        .map(|from| {
            let mut targets: Vec<u32> = graph.row(from).map(|edge| graph.target(edge)).collect();
            targets.sort_by_key(|&to| id(to));
            targets
        })
        .collect();

    let mut found = Vec::new();
    let mut spent = 0u64;
    for &start in &order {
        let mut visited = vec![false; graph.len()];
        visited[start as usize] = true;
        let mut path = vec![start];
        extend(
            &Enumeration { neighbours: &neighbours, rank: &rank, start, max_stops, budget },
            &mut path,
            &mut visited,
            &mut found,
            &mut spent,
        );
        if spent >= budget {
            break;
        }
    }
    found
}

/// Assembles a loop listing: the proved head, then a ranked enumeration.
///
/// Only the head's optimality was ever established, so everything after it says
/// so. That is not a hedge — a listing of flyable alternatives is genuinely
/// useful — but a row that has not been proved must not sit under the same
/// column heading as one that has.
#[must_use]
pub fn listing(
    graph: &TradeGraph,
    geometry: &Geometry<'_>,
    limits: &Limits,
    head: Route,
    stops: std::ops::RangeInclusive<usize>,
) -> Vec<Route> {
    let capacity = limits.top_n.saturating_mul(limits.shortlist_factor.max(1));
    let mut runners_up: TopN<RankKey, Route> = TopN::new(capacity.saturating_sub(1));
    // A set, not a `Vec`. `seen` grows to `RUNNER_UP_BUDGET` — two hundred
    // thousand entries — and a linear `contains` over it is ~2e10 `Vec<i64>`
    // comparisons: a **fixed forty-second stall, in complete silence, on every
    // run large enough to saturate the budget**, which is about two hundred
    // markets upward. It does not shrink with a narrower search, because the
    // budget is a constant. Measured at 40.4 s for n=200 and 43.6 s for n=800.
    //
    // `rank.stations` is already canonically rotated by `RankKey::build`
    // (report.rs) — a cycle has no start, so the smallest market id is rotated
    // to the front — which is exactly what makes it a sound hash key.
    let mut seen: std::collections::HashSet<Vec<i64>> =
        std::collections::HashSet::from([head.rank.stations.clone()]);
    let mut routes = vec![head];

    for nodes in enumerate_cycles(graph, geometry, *stops.end(), RUNNER_UP_BUDGET) {
        if !stops.contains(&nodes.len()) {
            continue;
        }
        let Some(route) = route_of(graph, geometry, &nodes) else { continue };
        if !seen.insert(route.rank.stations.clone()) {
            continue;
        }
        let key = route.rank.clone();
        runners_up.offer(
            key,
            route.with_guarantee(Guarantee::Heuristic { reason: HeuristicReason::RunnerUp }),
        );
    }

    routes.extend(runners_up.drain());
    routes
}

struct Enumeration<'a> {
    neighbours: &'a [Vec<u32>],
    rank: &'a [u32],
    start: u32,
    max_stops: usize,
    budget: u64,
}

fn extend(
    scope: &Enumeration<'_>,
    path: &mut Vec<u32>,
    visited: &mut [bool],
    found: &mut Vec<Vec<u32>>,
    spent: &mut u64,
) {
    if *spent >= scope.budget {
        return;
    }
    *spent += 1;
    let node = *path.last().expect("a path always has a head");
    for &to in &scope.neighbours[node as usize] {
        if to == scope.start {
            if path.len() >= 2 {
                found.push(path.clone());
            }
            continue;
        }
        // Each cycle is generated once, from whichever of its stations has the
        // lowest market id.
        if scope.rank[to as usize] < scope.rank[scope.start as usize]
            || visited[to as usize]
            || path.len() >= scope.max_stops
        {
            continue;
        }
        visited[to as usize] = true;
        path.push(to);
        extend(scope, path, visited, found, spent);
        path.pop();
        visited[to as usize] = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{best_ratio, enumerate_cycles, price_cycle, solve};
    use crate::fixture::{geometry, limits, market, ship};
    use crate::graph::{Pools, TradeGraph};
    use crate::num::Ratio;
    use crate::report::RouteKind;

    #[test]
    fn a_round_trip_carries_different_cargo_each_way() {
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[(1, 900, 500)]),
            market(2, 5.0, &[(1, 100, 500)], &[(0, 900, 500)]),
        ];
        let geometry = geometry(&markets);
        let graph =
            TradeGraph::build(&Pools::from_markets(&markets), &geometry, &ship(), &limits());
        let routes = solve(&graph, &geometry, &limits());
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].kind, RouteKind::RoundTrip);
        assert_ne!(routes[0].legs[0].choice.commodity, routes[0].legs[1].choice.commodity);
        assert_eq!(routes[0].profit, crate::num::Credits(2 * 500 * 800));
    }

    #[test]
    fn a_one_way_pair_makes_no_round_trip() {
        let markets =
            [market(1, 0.0, &[(0, 100, 500)], &[]), market(2, 5.0, &[], &[(0, 900, 500)])];
        let geometry = geometry(&markets);
        let graph =
            TradeGraph::build(&Pools::from_markets(&markets), &geometry, &ship(), &limits());
        assert!(solve(&graph, &geometry, &limits()).is_empty());
        assert_eq!(best_ratio(&graph), None);
    }

    #[test]
    fn the_enumeration_is_ordered_by_market_id_not_by_arrival_order() {
        // The same three stations, presented in two orders. The set of cycles
        // and the order they come back in must not know the difference.
        let forward = [
            market(30, 0.0, &[(0, 100, 500)], &[(2, 900, 500)]),
            market(10, 1.0, &[(1, 100, 500)], &[(0, 900, 500)]),
            market(20, 2.0, &[(2, 100, 500)], &[(1, 900, 500)]),
        ];
        let backward = [forward[2].clone(), forward[0].clone(), forward[1].clone()];

        let named = |markets: &[crate::model::Market]| {
            let geometry = geometry(markets);
            let graph =
                TradeGraph::build(&Pools::from_markets(markets), &geometry, &ship(), &limits());
            enumerate_cycles(&graph, &geometry, 4, 10_000)
                .into_iter()
                .map(|cycle| {
                    cycle
                        .into_iter()
                        .map(|node| markets[node as usize].market_id)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(named(&forward), vec![vec![10, 20, 30]]);
        assert_eq!(named(&forward), named(&backward));
    }

    #[test]
    fn the_warm_start_is_the_best_two_cycles_exact_rate() {
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[(1, 900, 500)]),
            market(2, 5.0, &[(1, 100, 500)], &[(0, 900, 500)]),
        ];
        let geometry = geometry(&markets);
        let graph =
            TradeGraph::build(&Pools::from_markets(&markets), &geometry, &ship(), &limits());
        let best = best_ratio(&graph).expect("a round trip");
        let (profit, millis, edges) = price_cycle(&graph, &best.nodes).expect("a priced cycle");
        assert_eq!(best.rate, Ratio::new(profit, millis));
        assert_eq!(edges.len(), 2);
    }
}
