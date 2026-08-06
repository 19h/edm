//! A search that ran out of time must say so in the data, not only in a
//! progress line.
//!
//! The head of an abandoned `ratio::solve` is the round-trip warm start, which
//! is the *worst* loop in the listing. `Threading::rethread` re-ranks and then
//! truncates to `top_n`, so on any instance with enough runners-up the head —
//! the only route carrying `SearchBudgetExhausted` — was dropped. Every
//! surviving row then read `Heuristic { RunnerUp }`, documented as "only the
//! head of the list's optimality was established", about a head no longer in
//! the list.
//!
//! Under `--json` this was the whole of the evidence: the progress sink is
//! `None` there, so the withdrawal line does not exist either. A consumer would
//! have read a route 19% below the optimum as an ordinary runner-up.

use edm_core::domain::id64::Coordinates;
use edm_route::model::{
    Commodities, IngestCounts, Limits, Market, MarketIdentity, RawCommodity, RowFloors, ShipConfig,
};
use edm_route::report::{Guarantee, HeuristicReason};
use edm_route::time::TimeModel;
use edm_route::watch::Watch;
use edm_route::Wanted;

/// Twenty-four stations in a ring, each supplying its own commodity and paying
/// most for its successor's — so the optimum is a long cycle the warm start
/// cannot reach, and there are far more than `top_n` runners-up to bury it.
fn ring() -> Vec<Market> {
    const N: i64 = 24;
    let mut commodities = Commodities::new();
    let mut counts = IngestCounts::default();
    (0..N)
        .map(|i| {
            let mut rows: Vec<RawCommodity> = vec![RawCommodity {
                name: format!("c{i}"),
                buy_price: 100,
                sell_price: 90,
                candidate_sell_price: None,
                mean_price: 0,
                stock: 500,
                stock_bracket: 3,
                demand: 0,
                demand_bracket: 0,
                category: String::new(),
                illegal: false,
            }];
            rows.extend((0..N).filter(|c| *c != i).map(|c| RawCommodity {
                name: format!("c{c}"),
                buy_price: 0,
                sell_price: if c == (i + 1) % N { 900 } else { 300 },
                candidate_sell_price: None,
                mean_price: 0,
                stock: 0,
                stock_bracket: 0,
                demand: 500,
                demand_bracket: 3,
                category: String::new(),
                illegal: false,
            }));
            Market::from_rows(
                MarketIdentity {
                    market_id: i,
                    station: format!("Station {i}"),
                    system: format!("System {i}"),
                    system_address: i,
                    coords: Coordinates { x: i as f64 * 0.01, y: 0.0, z: 0.0 },
                    arrival_ls: 0.0,
                },
                &rows,
                &mut commodities,
                &RowFloors::default(),
                &mut counts,
            )
        })
        .collect()
}

fn ship() -> ShipConfig {
    ShipConfig { cargo: edm_route::num::Tons(500), credits: edm_route::num::Credits(1_000_000_000_000) }
}

#[test]
fn an_abandoned_search_says_so_on_every_route_it_returns() {
    let markets = ring();

    // Unbounded, the search proves an optimum.
    let proved = edm_route::solve(
        &markets,
        TimeModel::default(),
        &ship(),
        &Limits::default(),
        Wanted::all(),
        Watch::unlimited(),
    );
    assert!(!proved.loops.is_empty(), "the ring must produce loops");
    // `ProvedOptimal` rather than `OptimalForStartingCredits`: the balance here
    // is far above `cargo * max_buy_price`, so the credit cap never binds and
    // threading upgrades the claim — which is the stronger statement and the
    // one this instance earns.
    assert_eq!(
        proved.loops[0].guarantee,
        Guarantee::ProvedOptimal,
        "unbounded credits, so the search's optimum is the whole optimum"
    );

    // Expired before the first probe, so nothing was proved at all.
    let always = || true;
    let stopped = edm_route::solve(
        &markets,
        TimeModel::default(),
        &ship(),
        &Limits::default(),
        Wanted::all(),
        Watch::unlimited().until(&always),
    );
    assert!(!stopped.loops.is_empty(), "an abandoned search still reports what it holds");

    // The claim survives re-ranking and truncation, on *every* row — not just
    // on a head that the sort has since buried.
    for (index, route) in stopped.loops.iter().enumerate() {
        assert_eq!(
            route.guarantee,
            Guarantee::Heuristic { reason: HeuristicReason::SearchBudgetExhausted },
            "route {index} of an abandoned search claims {:?}",
            route.guarantee
        );
    }

    // And it is a weaker answer than the proved one, which is the whole reason
    // the label has to survive: without it this reads as an ordinary ranking.
    let best_proved = proved.loops[0].rate().steady.expect("a loop has a steady rate");
    let best_stopped = stopped.loops[0].rate().steady.expect("a loop has a steady rate");
    assert!(
        best_stopped < best_proved,
        "the abandoned search should be worse: {best_stopped:?} vs {best_proved:?}"
    );
}
