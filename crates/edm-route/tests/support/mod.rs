//! Brute-force oracles and instance generators.
//!
//! The oracles here are deliberately stupid: they enumerate everything and take
//! the maximum. That is the point — a reference implementation is only worth
//! anything if it is obviously correct, and "enumerate every simple cycle" is
//! obviously correct in a way that "Dinkelbach's iteration over a layered
//! parametric dynamic program" is not.
//!
//! Everything compares **rationals**. A floating-point comparison would hide
//! exactly the class of near-tie these searches exist to resolve, so the word
//! `f64` does not appear below except where a station has to be placed in
//! space.

#![allow(dead_code, reason = "one support module shared by three test binaries")]

use edm_route::graph::{Pools, TradeGraph};
use edm_route::model::{
    CommodityId, Demand, DemandQty, Limits, Market, ShipConfig, Supply,
};
use edm_route::num::{Credits, Ratio, Tons};
use edm_route::report::{RankKey, Route};
use edm_route::round;
use edm_route::time::{Geometry, TimeModel};
use edm_route::watch::Watch;
use edm_route::topn::TopN;
use edm_route::weight::leg_weight;

/// `(commodity, price, quantity)`.
pub(crate) type Row = (u32, i64, i64);

/// A market at `x` light years along the x axis, docked at its star.
pub(crate) fn market(id: i64, x: f64, supply: &[Row], demand: &[Row]) -> Market {
    at(id, x, 0.0, supply, demand)
}

/// A market at an arbitrary position and star distance.
pub(crate) fn at(id: i64, x: f64, arrival_ls: f64, supply: &[Row], demand: &[Row]) -> Market {
    Market {
        market_id: id,
        station: format!("Station {id}"),
        system: format!("System {id}"),
        system_address: id,
        coords: edm_core::domain::id64::Coordinates { x, y: 0.0, z: 0.0 },
        arrival_ls,
        supply: supply
            .iter()
            .map(|&(commodity, price, stock)| Supply {
                commodity: CommodityId(commodity),
                buy_price: Credits(price),
                stock: Tons(stock),
            })
            .collect(),
        demand: demand
            .iter()
            .map(|&(commodity, price, qty)| Demand {
                commodity: CommodityId(commodity),
                sell_price: Credits(price),
                qty: DemandQty::Published(Tons(qty)),
                bulk: None,
            })
            .collect(),
    }
}

/// A ship whose hold and balance never bind.
pub(crate) fn ship() -> ShipConfig {
    ShipConfig { cargo: Tons(1_000), credits: Credits(1_000_000_000_000) }
}

/// Limits that filter nothing.
pub(crate) fn limits() -> Limits {
    Limits { top_n: 64, shortlist_factor: 1, ..Limits::default() }
}

/// Builds the graph for an instance under the default model.
pub(crate) fn graph_of(markets: &[Market], limits: &Limits) -> TradeGraph {
    TradeGraph::build(
        &Pools::from_markets(markets),
        &Geometry::new(markets, TimeModel::default()),
        &ship(),
        limits,
        Watch::unlimited(),
    )
}

/// The default geometry for an instance.
pub(crate) fn geometry(markets: &[Market]) -> Geometry<'_> {
    Geometry::new(markets, TimeModel::default())
}

/// Every simple cycle in the graph, each listed once from its lowest node.
pub(crate) fn all_simple_cycles(graph: &TradeGraph) -> Vec<Vec<u32>> {
    let mut found = Vec::new();
    for start in 0..graph.len() as u32 {
        let mut path = vec![start];
        let mut visited = vec![false; graph.len()];
        visited[start as usize] = true;
        walk(graph, start, start, &mut path, &mut visited, &mut found);
    }
    found
}

fn walk(
    graph: &TradeGraph,
    start: u32,
    node: u32,
    path: &mut Vec<u32>,
    visited: &mut [bool],
    found: &mut Vec<Vec<u32>>,
) {
    for edge in graph.row(node) {
        let to = graph.target(edge);
        if to == start {
            if path.len() >= 2 {
                found.push(path.clone());
            }
            continue;
        }
        // Canonical: the start is the lowest-numbered station of the cycle.
        if to < start || visited[to as usize] {
            continue;
        }
        visited[to as usize] = true;
        path.push(to);
        walk(graph, start, to, path, visited, found);
        path.pop();
        visited[to as usize] = false;
    }
}

/// The best rate over every simple cycle whose length lies in `stops`.
pub(crate) fn best_cycle_rate(
    graph: &TradeGraph,
    stops: std::ops::RangeInclusive<usize>,
) -> Option<Ratio> {
    let mut best: Option<Ratio> = None;
    for cycle in all_simple_cycles(graph) {
        if !stops.contains(&cycle.len()) {
            continue;
        }
        let Some((profit, millis, _)) = round::price_cycle(graph, &cycle) else { continue };
        let rate = Ratio::new(profit, millis);
        if best.is_none_or(|current| rate > current) {
            best = Some(rate);
        }
    }
    best
}

/// Every single hop in the instance, ranked, with no pruning whatever.
pub(crate) fn brute_force_single_hops(markets: &[Market], limits: &Limits) -> Vec<Route> {
    let geometry = geometry(markets);
    let ship = ship();
    let capacity = limits.top_n.saturating_mul(limits.shortlist_factor.max(1));
    let mut heap: TopN<RankKey, Route> = TopN::new(capacity);

    for (from, origin) in markets.iter().enumerate() {
        for (to, destination) in markets.iter().enumerate() {
            if from == to {
                continue;
            }
            if limits.exclude_same_system
                && origin.system_address == destination.system_address
            {
                continue;
            }
            // Every trade, not every station pair: a hop is identified by its
            // cargo as well as its endpoints, and the searched version says the
            // same, so the two lists are comparable row for row.
            for supply in &origin.supply {
                for demand in &destination.demand {
                    if supply.commodity != demand.commodity {
                        continue;
                    }
                    let Some(choice) =
                        leg_weight(supply, demand, &ship, ship.credits, limits.min_units)
                    else {
                        continue;
                    };
                    if choice.profit <= limits.min_profit {
                        continue;
                    }
                    let route = Route::single_hop(&geometry, from as u32, to as u32, choice);
                    heap.offer(route.rank.clone(), route);
                }
            }
        }
    }
    heap.drain()
}

/// A small, fast, reproducible generator. Not cryptography; a test fixture.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub(crate) fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }
}

/// A random instance dense enough to hold cycles but small enough to enumerate.
pub(crate) fn random_markets(rng: &mut Rng, count: usize, commodities: u32) -> Vec<Market> {
    let mut markets = Vec::with_capacity(count);
    for i in 0..count {
        let mut supply = Vec::new();
        let mut demand = Vec::new();
        for commodity in 0..commodities {
            if rng.below(3) > 0 {
                let price = 100 + rng.below(400) as i64;
                supply.push((commodity, price, 100 + rng.below(900) as i64));
            }
            if rng.below(3) > 0 {
                let price = 100 + rng.below(900) as i64;
                demand.push((commodity, price, 100 + rng.below(900) as i64));
            }
        }
        let x = (rng.below(400) as f64) / 10.0;
        markets.push(at(i as i64 + 1, x, (rng.below(2_000)) as f64, &supply, &demand));
    }
    markets
}

/// The identity of a ranked list: what it claims, in order, with no indices.
pub(crate) fn ranking(routes: &[Route]) -> Vec<(Vec<i64>, Vec<u32>, i64, i64)> {
    routes
        .iter()
        .map(|route| {
            (
                route.rank.stations.clone(),
                route.rank.commodities.clone(),
                route.rank.rate.credits,
                route.rank.rate.millis,
            )
        })
        .collect()
}
