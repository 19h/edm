//! Every exact solver, against a brute-force reference, compared as rationals.
//!
//! The comparison is the whole test. Two rates that differ in the twentieth
//! significant digit are different rates, and a floating-point comparison would
//! report them equal — hiding exactly the bug these tests exist to find. So
//! nothing here divides.

mod support;

use edm_route::bounded;
use edm_route::distinct;
use edm_route::num::Ratio;
use edm_route::ratio;
use edm_route::round;
use edm_route::single;
use support::{
    Rng, all_simple_cycles, best_cycle_rate, brute_force_single_hops, geometry, graph_of,
    limits, random_markets, ranking, ship,
};

/// Instances small enough that every simple cycle can be enumerated.
fn instances(seeds: std::ops::Range<u64>, markets: usize, commodities: u32) -> Vec<Vec<edm_route::model::Market>> {
    seeds
        .map(|seed| {
            let mut rng = Rng::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5bf0_3635);
            random_markets(&mut rng, markets, commodities)
        })
        .collect()
}

#[test]
fn the_free_loop_matches_brute_force_over_every_simple_cycle() {
    let mut checked = 0;
    for markets in instances(0..60, 7, 4) {
        let graph = graph_of(&markets, &limits());
        let oracle = best_cycle_rate(&graph, 2..=markets.len());
        let found = ratio::max_ratio_cycle(&graph).map(|best| best.rate);
        assert_eq!(found, oracle, "instance of {} markets", markets.len());
        if let Some(best) = ratio::max_ratio_cycle(&graph) {
            assert!(best.proved, "the free solver always terminates with a proof");
            checked += 1;
        }
    }
    assert!(checked > 10, "the generator produced too few cyclic instances to prove anything");
}

#[test]
fn the_bounded_loop_matches_brute_force_at_every_cap() {
    for markets in instances(100..140, 7, 4) {
        let graph = graph_of(&markets, &limits());
        for k in 2..=5usize {
            let oracle = best_cycle_rate(&graph, 2..=k);
            let found = bounded::best_bounded(&graph, k).map(|best| best.rate);
            assert_eq!(found, oracle, "cap of {k}");
        }
    }
}

#[test]
fn a_cap_of_two_is_exactly_the_best_round_trip() {
    for markets in instances(200..240, 7, 4) {
        let graph = graph_of(&markets, &limits());
        let bounded = bounded::best_bounded(&graph, 2).map(|best| best.rate);
        let round_trip = round::best_ratio(&graph).map(|best| best.rate);
        assert_eq!(bounded, round_trip);
    }
}

#[test]
fn a_cap_at_the_market_count_is_exactly_the_free_optimum() {
    for markets in instances(300..340, 7, 4) {
        let graph = graph_of(&markets, &limits());
        let free = ratio::max_ratio_cycle(&graph).map(|best| best.rate);
        let capped = bounded::best_bounded(&graph, markets.len()).map(|best| best.rate);
        assert_eq!(free, capped);
    }
}

#[test]
fn the_bounded_optimum_is_monotone_in_the_cap_and_never_beats_the_free_one() {
    for markets in instances(400..440, 7, 4) {
        let graph = graph_of(&markets, &limits());
        let Some(free) = ratio::max_ratio_cycle(&graph).map(|best| best.rate) else { continue };
        let mut previous: Option<Ratio> = None;
        for k in 2..=markets.len() {
            let Some(rate) = bounded::best_bounded(&graph, k).map(|best| best.rate) else {
                continue;
            };
            assert!(rate <= free, "a capped loop beat the uncapped optimum at k = {k}");
            if let Some(previous) = previous {
                assert!(rate >= previous, "raising the cap lowered the optimum at k = {k}");
            }
            previous = Some(rate);
        }
    }
}

#[test]
fn the_bounded_walk_optimum_equals_the_bounded_simple_cycle_optimum() {
    // The mediant decomposition argument, checked exhaustively rather than
    // trusted. If a closed walk of at most k legs could ever beat every simple
    // cycle of at most k legs, the dynamic program would return a rate the
    // enumeration cannot match — and this is where that would show up.
    for markets in instances(500..560, 7, 4) {
        let graph = graph_of(&markets, &limits());
        for k in 2..=5usize {
            let walk = bounded::best_bounded(&graph, k).map(|best| best.rate);
            let simple = best_cycle_rate(&graph, 2..=k);
            assert_eq!(walk, simple, "cap of {k}");
        }
    }
}

#[test]
fn both_loop_solvers_return_a_simple_cycle() {
    for markets in instances(600..640, 7, 4) {
        let graph = graph_of(&markets, &limits());
        let mut answers = Vec::new();
        if let Some(best) = ratio::max_ratio_cycle(&graph) {
            answers.push(best.nodes);
        }
        for k in 2..=5usize {
            if let Some(best) = bounded::best_bounded(&graph, k) {
                assert!(best.nodes.len() <= k);
                answers.push(best.nodes);
            }
        }
        for nodes in answers {
            let mut unique = nodes.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), nodes.len(), "{nodes:?} revisits a station");
        }
    }
}

#[test]
fn the_minimum_distinct_search_matches_brute_force() {
    for markets in instances(700..760, 7, 4) {
        let graph = graph_of(&markets, &limits());
        for k_min in 2..=4usize {
            let oracle = best_cycle_rate(&graph, k_min..=markets.len());
            let found =
                distinct::best_with_min_stops(&graph, &limits(), k_min).map(|best| best.rate);
            assert_eq!(found, oracle, "floor of {k_min}");
            if let Some(best) = distinct::best_with_min_stops(&graph, &limits(), k_min) {
                assert!(best.proved, "the budget is far above these instances");
                assert!(best.nodes.len() >= k_min);
                assert!(best.rate <= best.upper);
            }
        }
    }
}

#[test]
fn the_single_hop_search_produces_the_same_ranking_as_no_search_at_all() {
    // Identical rankings, not merely identical bests: the pruned search must
    // agree with the exhaustive one about the order of everything it kept, or
    // the total order is not doing its job.
    for markets in instances(800..860, 8, 5) {
        let graph_limits = limits();
        let pruned = single::solve(
            &edm_route::graph::Pools::from_markets(&markets),
            &geometry(&markets),
            &ship(),
            &graph_limits,
        );
        let exhaustive = brute_force_single_hops(&markets, &graph_limits);
        assert_eq!(ranking(&pruned), ranking(&exhaustive));
    }
}

#[test]
fn the_mediant_sandwich_holds() {
    // A cycle's rate is a weighted mediant of its legs' rates, so the optimum
    // lies between the worst and the best single edge in the graph.
    for markets in instances(900..960, 7, 4) {
        let graph = graph_of(&markets, &limits());
        let Some(best) = ratio::max_ratio_cycle(&graph) else { continue };
        let mut lowest: Option<Ratio> = None;
        let mut highest: Option<Ratio> = None;
        for (_, _, edge) in graph.edges() {
            let rate = Ratio::new(graph.weight(edge), graph.millis(edge));
            if lowest.is_none_or(|current| rate < current) {
                lowest = Some(rate);
            }
            if highest.is_none_or(|current| rate > current) {
                highest = Some(rate);
            }
        }
        assert!(best.rate >= lowest.expect("a graph with a cycle has an edge"));
        assert!(best.rate <= highest.expect("a graph with a cycle has an edge"));
    }
}

#[test]
fn the_enumeration_oracle_finds_what_it_should() {
    // The oracle is only worth anything if it is right, so it gets a case
    // small enough to count by hand: a triangle plus one chord gives the
    // three-cycle, and the two-cycle the chord closes.
    let markets = [
        support::market(1, 0.0, &[(0, 100, 500), (3, 100, 500)], &[(2, 900, 500)]),
        support::market(2, 1.0, &[(1, 100, 500), (2, 100, 500)], &[(0, 900, 500), (3, 900, 500)]),
        support::market(3, 2.0, &[(2, 100, 500)], &[(1, 900, 500)]),
    ];
    let graph = graph_of(&markets, &limits());
    let mut cycles = all_simple_cycles(&graph);
    cycles.sort();
    assert_eq!(cycles, vec![vec![0, 1], vec![0, 1, 2]]);
}
