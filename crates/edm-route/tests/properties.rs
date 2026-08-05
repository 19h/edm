//! Properties that must hold for every instance, not just the ones written by
//! hand.
//!
//! The one worth reading twice is
//! [`a_shuffled_instance_produces_an_identical_ranking`]. The ranking key ends
//! in an absolute tie-break derived from the route's own market ids, so the
//! answer cannot depend on the order the sweep happened to return markets in —
//! and a sweep of a thousand markets across sixteen workers returns them in a
//! different order every run.

mod support;

use edm_route::model::{Limits, ShipConfig};
use edm_route::num::{Credits, Ratio, Tons};
use edm_route::report::Guarantee;
use edm_route::Wanted;
use edm_route::time::TimeModel;
use edm_route::{model, ratio, solve};
use proptest::prelude::*;
use support::{Rng, graph_of, limits, random_markets, ranking, ship};

/// A market list and a permutation of it, both describing the same instance.
fn shuffled(markets: &[model::Market], rng: &mut Rng) -> Vec<model::Market> {
    let mut order: Vec<usize> = (0..markets.len()).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i as u64 + 1) as usize);
    }
    order
        .into_iter()
        .map(|i| {
            let mut market = markets[i].clone();
            // The rows inside a market are shuffled too: the commodity-major
            // build sorts them, and the sort has to be total for that to mean
            // anything.
            let rows = market.supply.len();
            for j in (1..rows).rev() {
                market.supply.swap(j, rng.below(j as u64 + 1) as usize);
            }
            let rows = market.demand.len();
            for j in (1..rows).rev() {
                market.demand.swap(j, rng.below(j as u64 + 1) as usize);
            }
            market
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn a_shuffled_instance_produces_an_identical_ranking(seed in 0u64..100_000) {
        let mut rng = Rng::new(seed);
        let markets = random_markets(&mut rng, 8, 5);
        let mut shuffler = Rng::new(seed ^ 0xa5a5_a5a5);
        let permuted = shuffled(&markets, &mut shuffler);

        let straight = solve(&markets, TimeModel::default(), &ship(), &limits(), Wanted::all());
        let crooked = solve(&permuted, TimeModel::default(), &ship(), &limits(), Wanted::all());

        prop_assert_eq!(ranking(&straight.single), ranking(&crooked.single));
        prop_assert_eq!(ranking(&straight.round_trip), ranking(&crooked.round_trip));
        prop_assert_eq!(ranking(&straight.loops), ranking(&crooked.loops));
    }

    #[test]
    fn threading_never_lowers_a_route(seed in 0u64..100_000) {
        let mut rng = Rng::new(seed);
        let markets = random_markets(&mut rng, 7, 4);
        // A balance tight enough that the cap binds somewhere.
        let ship = ShipConfig { cargo: Tons(1_000), credits: Credits(30_000) };
        let solution = solve(&markets, TimeModel::default(), &ship, &limits(), Wanted::all());
        for route in solution.single.iter().chain(&solution.round_trip).chain(&solution.loops) {
            let threaded = route.threaded.expect("finalists are threaded");
            prop_assert!(threaded.profit >= route.profit);
        }
    }

    #[test]
    fn an_unbindable_credit_cap_proves_the_result(seed in 0u64..100_000) {
        let mut rng = Rng::new(seed);
        let markets = random_markets(&mut rng, 7, 4);
        let solution = solve(&markets, TimeModel::default(), &ship(), &limits(), Wanted::all());
        for route in solution.single.iter().chain(&solution.round_trip).chain(&solution.loops) {
            let threaded = route.threaded.expect("finalists are threaded");
            prop_assert_eq!(threaded.profit, route.profit);
            match route.rate().guarantee {
                Guarantee::ProvedOptimal | Guarantee::Heuristic { .. } => {}
                other => prop_assert!(false, "unexpected guarantee {:?}", other),
            }
        }
    }

    #[test]
    fn a_ranked_list_descends(seed in 0u64..100_000) {
        let mut rng = Rng::new(seed);
        let markets = random_markets(&mut rng, 7, 4);
        let solution = solve(&markets, TimeModel::default(), &ship(), &limits(), Wanted::all());
        for list in [&solution.single, &solution.round_trip, &solution.loops] {
            for pair in list.windows(2) {
                prop_assert!(pair[0].rank >= pair[1].rank);
            }
        }
    }

    #[test]
    fn every_reported_loop_is_a_real_simple_cycle(seed in 0u64..100_000) {
        let mut rng = Rng::new(seed);
        let markets = random_markets(&mut rng, 7, 4);
        let solution = solve(&markets, TimeModel::default(), &ship(), &limits(), Wanted::all());
        for route in &solution.loops {
            let mut stations = route.rank.stations.clone();
            let stops = stations.len();
            stations.sort_unstable();
            stations.dedup();
            prop_assert_eq!(stations.len(), stops);
            // Consecutive legs must actually join up.
            for pair in route.legs.windows(2) {
                prop_assert_eq!(pair[0].to, pair[1].from);
            }
            prop_assert_eq!(route.legs[route.legs.len() - 1].to, route.legs[0].from);
        }
    }

    #[test]
    fn a_profit_floor_only_ever_removes_routes(seed in 0u64..100_000) {
        let mut rng = Rng::new(seed);
        let markets = random_markets(&mut rng, 7, 4);
        let open = limits();
        let floored = Limits { min_profit: Credits(100_000), ..open };
        let wide = solve(&markets, TimeModel::default(), &ship(), &open, Wanted::all());
        let narrow = solve(&markets, TimeModel::default(), &ship(), &floored, Wanted::all());
        // Dropping edges changes the stated feasible set, so the answer may get
        // worse — but it may never get better, and it must say what happened.
        if let (Some(a), Some(b)) = (wide.loops.first(), narrow.loops.first()) {
            prop_assert!(b.rank.rate <= a.rank.rate);
            // And the narrowing is stated rather than silent.
            prop_assert!(b.caveats.contains(&edm_route::report::Caveat::EdgesBelowFloorDropped));
            prop_assert!(!a.caveats.contains(&edm_route::report::Caveat::EdgesBelowFloorDropped));
        }
        prop_assert!(narrow.loops.len() <= wide.loops.len() || wide.loops.is_empty());
    }

    #[test]
    fn the_free_optimum_bounds_every_constrained_one(seed in 0u64..100_000) {
        let mut rng = Rng::new(seed);
        let markets = random_markets(&mut rng, 7, 4);
        let graph = graph_of(&markets, &limits());
        let Some(free) = ratio::max_ratio_cycle(&graph) else { return Ok(()) };
        for k in 2..=7usize {
            if let Some(capped) = edm_route::bounded::best_bounded(&graph, k) {
                prop_assert!(capped.rate <= free.rate);
            }
            if let Some(floored) =
                edm_route::distinct::best_with_min_stops(&graph, &limits(), k)
            {
                prop_assert!(floored.rate <= free.rate);
                prop_assert!(floored.rate <= floored.upper);
                prop_assert!(floored.nodes.len() >= k);
            }
        }
    }
}

#[test]
fn a_rate_cannot_be_read_without_its_guarantee() {
    // Structural, not behavioural: `Route` has a private field, so the only way
    // to a steady rate is `Route::rate`, which returns the guarantee and the
    // caveats with it. If someone makes the rate a public field this stops
    // compiling, which is the point.
    let markets = [
        support::market(1, 0.0, &[(0, 100, 500)], &[(1, 900, 500)]),
        support::market(2, 5.0, &[(1, 100, 500)], &[(0, 900, 500)]),
    ];
    let solution = solve(&markets, TimeModel::default(), &ship(), &limits(), Wanted::all());
    let claim = solution.round_trip[0].rate();
    assert!(claim.steady.is_some());
    assert!(!claim.caveats.is_empty());
    assert_eq!(
        claim.steady,
        Some(Ratio::new(solution.round_trip[0].profit, solution.round_trip[0].cycle_millis))
    );
}
