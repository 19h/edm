//! "Reporting the best route it had" must not be followed by no route.
//!
//! Every abandoned path in the optimiser reaches its return through
//! `nodes: witness?`, and `witness` is the round-trip warm start, which only
//! ever finds **two**-cycles. On a graph whose only cycles are longer — a
//! directed ring — `witness` is `None`, so the `?` propagates out of
//! `max_ratio_cycle`, whose documented meaning for `None` is "the graph holds
//! no cycle at all".
//!
//! The `Abandoned` event had already been sent by then, so the run printed
//! *"the search ran out of time; reporting the best route it had, unproved"*
//! and then reported nothing. Two contradictory statements, one of them false,
//! about the same search.
//!
//! The same contradiction is reachable on ordinary data through
//! `--min-distinct`, where an unproved free optimum skips the search entirely
//! and leaves `best` empty.

use std::cell::RefCell;

use edm_core::domain::id64::Coordinates;
use edm_route::model::{
    Commodities, IngestCounts, Limits, Market, MarketIdentity, RawCommodity, RowFloors, ShipConfig,
};
use edm_route::time::TimeModel;
use edm_route::watch::{Event, Watch};
use edm_route::Wanted;

/// A directed three-ring: M0 supplies c0 and buys c2, M1 supplies c1 and buys
/// c0, M2 supplies c2 and buys c1. Three edges, no reverse edge anywhere, so
/// there is no two-cycle for the warm start to find.
fn ring() -> Vec<Market> {
    let mut commodities = Commodities::new();
    let mut counts = IngestCounts::default();
    (0..3i64)
        .map(|i| {
            let mine = i;
            let wanted = (i + 2) % 3;
            let rows = vec![
                RawCommodity {
                    name: format!("c{mine}"),
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
                },
                RawCommodity {
                    name: format!("c{wanted}"),
                    buy_price: 0,
                    sell_price: 900,
                    candidate_sell_price: None,
                    mean_price: 0,
                    stock: 0,
                    stock_bracket: 0,
                    demand: 500,
                    demand_bracket: 3,
                    category: String::new(),
                    illegal: false,
                },
            ];
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
    ShipConfig {
        cargo: edm_route::num::Tons(500),
        credits: edm_route::num::Credits(1_000_000_000_000),
    }
}

/// The invariant, stated once and checked over several shapes: a run that
/// announces a withdrawal must have something to withdraw the claim about.
fn withdrawal_matches_output(limits: &Limits, markets: &[Market]) {
    let events: RefCell<Vec<Event>> = RefCell::new(Vec::new());
    let sink = |event: Event| events.borrow_mut().push(event);
    let always = || true;

    let solution = edm_route::solve(
        markets,
        TimeModel::default(),
        &ship(),
        limits,
        Wanted::all(),
        Watch::unlimited().until(&always).reporting(&sink),
    );

    let withdrew = events.borrow().iter().any(|event| matches!(event, Event::Abandoned));
    assert!(
        !(withdrew && solution.loops.is_empty()),
        "announced 'reporting the best route it had' and then reported none \
         (limits: max_stops {:?}, min_distinct {:?})",
        limits.max_stops,
        limits.min_distinct,
    );
}

#[test]
fn an_abandoned_ratio_search_with_no_two_cycle_does_not_announce_a_route() {
    withdrawal_matches_output(&Limits::default(), &ring());
}

#[test]
fn an_abandoned_bounded_search_with_no_two_cycle_does_not_announce_a_route() {
    withdrawal_matches_output(&Limits { max_stops: Some(4), ..Limits::default() }, &ring());
}

/// The `--min-distinct` path, which reaches the same contradiction on data that
/// has plenty of two-cycles: an unproved free optimum skips the search, so
/// `best` is empty however ordinary the markets are.
#[test]
fn an_abandoned_distinct_search_does_not_announce_a_route_it_has_not_got() {
    withdrawal_matches_output(&Limits { min_distinct: Some(3), ..Limits::default() }, &ring());
}
