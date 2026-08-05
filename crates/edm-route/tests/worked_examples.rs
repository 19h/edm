//! Instances whose answers are written down as literals, and the two failure
//! modes that a property test would never find on its own.

mod support;

use edm_route::num::{Credits, Millis, Ratio, Tons};
use edm_route::report::{Caveat, Guarantee, RouteKind};
use edm_route::time::TimeModel;
use edm_route::watch::Watch;
use edm_route::{bounded, distinct, model, ratio, round, solve};
use support::{best_cycle_rate, graph_of, limits, market, ship};

/// Three markets on a line, one light year apart, every station at its star.
///
/// Under the default model each leg is one jump (45 s) plus the approach
/// (20 s supercruise + 60 s docking + 30 s market) — **155 seconds exactly**,
/// whichever pair, because a jump count ceils and every distance here is under
/// the 30 Ly range.
///
/// ```text
///   A sells food   at 100      A buys metal at   900
///   B sells metal  at 100      B buys ore   at   900
///   C sells ore    at 100      C buys food  at   900
/// ```
///
/// With a 1,000 ton hold and enough credits, every leg moves 1,000 tons at a
/// margin of 800: **800,000 credits a leg**.
///
/// - The three-cycle A→B→C→A earns 2,400,000 over 465,000 ms.
/// - There is no two-cycle: each station buys only what the *previous* one
///   sells, so every pair is one-way.
/// - A single hop earns 800,000, and its first lap costs the cycle's leg time
///   plus the 110,000 ms of reaching and loading at A: 265,000 ms.
fn three_markets() -> Vec<model::Market> {
    vec![
        market(101, 0.0, &[(0, 100, 1_000)], &[(1, 900, 1_000)]),
        market(102, 1.0, &[(1, 100, 1_000)], &[(2, 900, 1_000)]),
        market(103, 2.0, &[(2, 100, 1_000)], &[(0, 900, 1_000)]),
    ]
}

#[test]
fn three_markets_by_hand() {
    let markets = three_markets();
    let graph = graph_of(&markets, &limits());

    assert_eq!(graph.edge_count(), 3);
    for (_, _, edge) in graph.edges() {
        assert_eq!(graph.weight(edge), Credits(800_000));
        assert_eq!(graph.millis(edge), Millis(155_000));
    }

    let best = ratio::max_ratio_cycle(&graph, Watch::unlimited()).expect("the triangle is a cycle");
    assert!(best.proved);
    assert_eq!(best.nodes.len(), 3);
    assert_eq!(best.rate, Ratio::new(Credits(2_400_000), Millis(465_000)));
    // 2,400,000 credits per 465 seconds is 18,580,645.16… per hour, and the
    // reported figure floors.
    assert_eq!(best.rate.credits_per_hour_floor(), 18_580_645);

    assert_eq!(round::best_ratio(&graph), None, "every pair here is one-way");
}

#[test]
fn three_markets_end_to_end() {
    let markets = three_markets();
    let solution = solve(&markets, TimeModel::default(), &ship(), &limits(), Watch::unlimited());

    assert!(solution.round_trip.is_empty());

    let loop_route = &solution.loops[0];
    assert_eq!(loop_route.kind, RouteKind::Loop { stops: 3 });
    assert_eq!(loop_route.profit, Credits(2_400_000));
    assert_eq!(loop_route.cycle_millis, Millis(465_000));
    // The first lap additionally pays for reaching and loading at the start:
    // 20 s supercruise + 60 s docking + 30 s market.
    assert_eq!(loop_route.first_lap_millis, Millis(575_000));
    let claim = loop_route.rate();
    assert_eq!(claim.steady, Some(Ratio::new(Credits(2_400_000), Millis(465_000))));
    assert_eq!(claim.guarantee, Guarantee::ProvedOptimal);

    let hop = &solution.single[0];
    assert_eq!(hop.kind, RouteKind::SingleHop);
    assert_eq!(hop.profit, Credits(800_000));
    assert_eq!(hop.first_lap_millis, Millis(265_000));
    let claim = hop.rate();
    assert_eq!(claim.steady, None, "a single hop has no steady state");
    assert!(claim.caveats.contains(&Caveat::SingleHopNotRepeatable));
}

#[test]
fn a_full_hold_and_a_full_purse_prove_the_result_outright() {
    let markets = three_markets();
    let solution = solve(&markets, TimeModel::default(), &ship(), &limits(), Watch::unlimited());
    for route in solution.loops.iter().chain(&solution.single) {
        // Every route in the list is either proved or explicitly labelled;
        // there is no third state where a rate reads as proved by omission.
        match route.rate().guarantee {
            Guarantee::ProvedOptimal | Guarantee::Heuristic { .. } => {}
            other => panic!("unexpected guarantee {other:?}"),
        }
    }
    let threaded = solution.loops[0].threaded.expect("finalists are threaded");
    assert_eq!(threaded.profit, solution.loops[0].profit, "an unbindable cap changes nothing");
}

#[test]
fn a_thinner_purse_says_so_instead() {
    let markets = three_markets();
    // 50,000 credits buys 500 tons at 100 each: the cap binds on every leg.
    let poor = model::ShipConfig { cargo: Tons(1_000), credits: Credits(50_000) };
    let solution = solve(&markets, TimeModel::default(), &poor, &limits(), Watch::unlimited());
    let route = &solution.loops[0];
    assert_eq!(route.legs[0].choice.units, Tons(500));
    assert_eq!(route.rate().guarantee, Guarantee::OptimalForStartingCredits);
    assert!(route.rate().caveats.contains(&Caveat::CreditCapBinds));
    // The first leg's 400,000 credits of profit unbind the second and third.
    let threaded = route.threaded.expect("finalists are threaded");
    assert!(threaded.profit > route.profit);
}

/// Two disjoint three-cycles and one two-cycle, arranged so a profit-per-hour
/// bound taken over the *maximum* leg count prunes the true winner.
///
/// - stations 0,1,2 form a three-cycle worth 290,000 a leg;
/// - stations 3,4,5 form a three-cycle worth 300,000 a leg — the winner;
/// - stations 6,7 form a two-cycle worth 341,000 a leg, the best rate in the
///   graph but only two stops, so a floor of three excludes it.
///
/// Every leg is one jump and every station is at its star, so every leg costs
/// exactly 155,000 ms.
fn two_triangles_and_a_shuttle() -> Vec<model::Market> {
    vec![
        market(1, 0.0, &[(0, 100, 1_000)], &[(2, 390, 1_000)]),
        market(2, 1.0, &[(1, 100, 1_000)], &[(0, 390, 1_000)]),
        market(3, 2.0, &[(2, 100, 1_000)], &[(1, 390, 1_000)]),
        market(4, 3.0, &[(3, 100, 1_000)], &[(5, 400, 1_000)]),
        market(5, 4.0, &[(4, 100, 1_000)], &[(3, 400, 1_000)]),
        market(6, 5.0, &[(5, 100, 1_000)], &[(4, 400, 1_000)]),
        market(7, 6.0, &[(6, 100, 1_000)], &[(7, 441, 1_000)]),
        market(8, 7.0, &[(7, 100, 1_000)], &[(6, 441, 1_000)]),
    ]
}

#[test]
fn the_completion_bound_does_not_prune_a_short_winner() {
    // This test exists to fail if anyone replaces the parametric bound in
    // `distinct` with a profit-per-hour bound. `edtrade/src/solve/circuit.ts:99`
    // converts an optimistic profit into a rate by dividing by the time floor
    // of the *maximum* leg count — a divisor too large for any shorter circuit
    // in the same subtree, so the bound comes out too tight and can prune the
    // true winner. Folding numerator and denominator into one quantity before
    // bounding, as `beyond_reach` does, leaves nothing to get backwards.
    let markets = two_triangles_and_a_shuttle();
    let limits = model::Limits { max_stops: Some(5), min_distinct: Some(3), ..limits() };
    let graph = graph_of(&markets, &limits);

    let free = ratio::max_ratio_cycle(&graph, Watch::unlimited()).expect("a cycle");
    assert_eq!(free.nodes, vec![6, 7], "the unconstrained optimum is the two-stop shuttle");

    let winner = Ratio::new(Credits(900_000), Millis(465_000));
    let runner_up = Ratio::new(Credits(870_000), Millis(465_000));

    // The trap, stated as arithmetic. The winner's whole profit divided by the
    // five-leg time floor lands *below* the incumbent, so a bound of that shape
    // discards the winner's entire subtree before a single leg of it is flown.
    let time_floor = TimeModel::default().min_leg_millis();
    let naive = Ratio::new(Credits(900_000), Millis(time_floor.0 * 5));
    assert!(
        naive < runner_up,
        "the instance is not exercising the trap: {naive:?} should fall below {runner_up:?}"
    );
    assert!(winner > runner_up, "the winner must actually be the winner");

    let found = distinct::best_with_min_stops(&graph, &limits, 3, Watch::unlimited())
        .expect("a three-stop loop");
    assert_eq!(found.rate, winner);
    assert_eq!(found.nodes, vec![3, 4, 5]);
    assert!(found.proved);
    assert_eq!(found.upper, free.rate, "the free optimum is what bounds the constrained one");

    // And the brute force agrees, which is what makes the two assertions above
    // a statement about the answer rather than about this implementation.
    assert_eq!(best_cycle_rate(&graph, 3..=5), Some(winner));
}

#[test]
fn the_shuttle_is_still_found_when_the_floor_allows_it() {
    let markets = two_triangles_and_a_shuttle();
    let limits = model::Limits { max_stops: Some(5), min_distinct: Some(2), ..limits() };
    let graph = graph_of(&markets, &limits);
    let found =
        distinct::best_with_min_stops(&graph, &limits, 2, Watch::unlimited()).expect("a loop");
    assert_eq!(found.nodes, vec![6, 7]);
    assert_eq!(found.rate, Ratio::new(Credits(682_000), Millis(310_000)));
}

#[test]
fn reduced_weights_do_not_overflow_at_instance_bounds() {
    // The stated bounds: a cycle's profit reaches 2^48 and its time 2^36, so a
    // reduced weight reaches 2^36 * 2^35 = 2^71 and a Bellman-Ford distance
    // accumulates those to 2^84. Both are far outside `i64`, which is why every
    // reduced weight in this crate is `i128` with no narrow fast path.
    let rate = Ratio { credits: 1 << 48, millis: 1 << 36 };
    let widest = ratio::reduced(rate, Credits(1 << 35), Millis(1 << 22));
    assert_eq!(widest, (1i128 << 36) * (1i128 << 35) - (1i128 << 48) * (1i128 << 22));
    assert_eq!(widest, 1i128 << 70);
    assert!(widest > i128::from(i64::MAX), "the bound is not being exercised");

    // A ring of the largest markets the design admits, with every edge at the
    // widest reduced weight. The primitive accumulates all of them.
    let stations = 2_048usize;
    let edges: Vec<(u32, u32)> =
        (0..stations as u32).map(|i| (i, (i + 1) % stations as u32)).collect();
    let weights = vec![1i128 << 71; stations];
    let cycle = ratio::positive_cycle(stations, &edges, &weights).expect("a positive ring");
    assert_eq!(cycle.len(), stations);

    // And the same ring, one unit short of paying for itself, must not be found.
    let mut losing = vec![1i128 << 71; stations];
    losing[0] = -(1i128 << 71) * (stations as i128 - 1) - 1;
    assert_eq!(ratio::positive_cycle(stations, &edges, &losing), None);
}

#[test]
fn a_maximum_scale_cycle_prices_without_overflow() {
    // 64 stations, each hop moving a 32,768 ton hold at a margin of 1,048,576:
    // 2^35 credits a leg, 2^41 over the ring. Nothing here may wrap.
    let stations = 64usize;
    let markets: Vec<model::Market> = (0..stations)
        .map(|i| {
            let sold = i as u32;
            let bought = ((i + stations - 1) % stations) as u32;
            market(
                i as i64 + 1,
                i as f64,
                &[(sold, 1, 1 << 15)],
                &[(bought, 1 << 20, 1 << 15)],
            )
        })
        .collect();
    let ship = model::ShipConfig { cargo: Tons(1 << 15), credits: Credits(1 << 40) };
    let graph = edm_route::graph::TradeGraph::build(
        &edm_route::graph::Pools::from_markets(&markets),
        &edm_route::time::Geometry::new(&markets, TimeModel::default()),
        &ship,
        &limits(),
        Watch::unlimited(),
    );
    let best = ratio::max_ratio_cycle(&graph, Watch::unlimited()).expect("the ring is a cycle");
    assert!(best.proved);
    assert_eq!(best.nodes.len(), stations);
    let (profit, _, _) = round::price_cycle(&graph, &best.nodes).expect("a priced ring");
    assert_eq!(profit, Credits(stations as i64 * (1 << 15) * ((1 << 20) - 1)));
    // The same instance through the bounded solver, which multiplies the
    // largest quantities of the two.
    assert_eq!(
        bounded::best_bounded(&graph, stations, Watch::unlimited()).map(|b| b.rate),
        Some(best.rate)
    );
}
