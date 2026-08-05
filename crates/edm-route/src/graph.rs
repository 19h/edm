//! The trade graph: nodes are markets, an edge is the best laden hop between
//! two of them, and the whole thing is built commodity-major.
//!
//! **Never pair-major.** Pairing markets first and asking what they can trade
//! costs `n² · |C|` and spends nearly all of it discovering emptiness. Pivoting
//! on the commodity costs `Σ_c |suppliers_c| · |buyers_c|` instead, and every
//! Companion API market returns the same 391-entry commodity map with most rows
//! priced but holding nothing and wanting nothing — so the pivot is what makes
//! the constant factor bearable. Measured 2026-08-06 over the 22 real market
//! payloads the first live run cached: `Σ_c |sup| · |buy|` is 7,038 against a
//! pair-major 180,642, a factor of 25.
//!
//! **The result is nevertheless a dense graph, and an earlier version of this
//! comment claimed otherwise.** "Most market pairs share no tradeable commodity"
//! is false for Companion API data. Over those 22 markets, **410 of the 462
//! ordered pairs — 89% — have a profitable trade between them**; over a cached
//! 5,049-market sweep it is **95%**. So the compressed sparse row layout below
//! is a memory layout and not a sparsity claim, the strongly connected
//! component decomposition yields one component holding all but the few
//! stations that trade with nobody, and the build is quadratic in the market
//! count with no help from the data: **24,292,232 legs and 127 seconds at
//! 5,049 markets**, with a transient peak of 4.1 GiB. That is why it reports
//! progress.
//!
//! Three admissible break bounds come from `edtrade/src/solve/singlehop.ts`,
//! and one subtlety it documents must survive the port: **`stock` is excluded
//! from the supplier bound.** Stock is not monotone in the price sort, so a
//! bound that included it would not be non-increasing in `i` and the `break`
//! would be unsound. A thin supply row is therefore a `continue`, never a
//! `break`.

use std::collections::HashMap;

use crate::model::{CommodityId, Demand, Limits, Market, ShipConfig, Supply};
use crate::num::{Credits, Millis, Tons};
use crate::time::Geometry;
use crate::watch::{Event, Watch};
use crate::weight::{LegChoice, affordable, leg_weight};

/// How many supply rows the build gets through between progress reports.
///
/// The pools are wildly uneven — one commodity's pool can be most of the build
/// — so reporting only at pool boundaries would leave the longest stretch of a
/// wide sweep silent, which is the thing being fixed. 4,096 rows is a report
/// every few tens of milliseconds at 5,000 markets and never more than one per
/// pool at fixture sizes.
const ROWS_PER_REPORT: usize = 4_096;

/// A market's supply row, tagged with the node that holds it.
#[derive(Clone, Copy, Debug)]
pub struct SupplyRow {
    /// Index of the market.
    pub node: u32,
    /// The row.
    pub row: Supply,
}

/// A market's demand row, tagged with the node that holds it.
#[derive(Clone, Copy, Debug)]
pub struct DemandRow {
    /// Index of the market.
    pub node: u32,
    /// The row.
    pub row: Demand,
}

/// Everyone selling and everyone buying one commodity.
#[derive(Clone, Debug)]
pub struct Pool {
    /// Which commodity.
    pub commodity: CommodityId,
    /// Sellers, cheapest ask first.
    pub suppliers: Vec<SupplyRow>,
    /// Buyers, highest bid first.
    pub buyers: Vec<DemandRow>,
}

/// The instance, pivoted by commodity.
#[derive(Clone, Debug, Default)]
pub struct Pools {
    /// One entry per commodity that has at least one seller and one buyer.
    pub pools: Vec<Pool>,
}

impl Pools {
    /// Pivots a market list into commodity pools, sorting each side by price.
    ///
    /// Ties in price are broken by node index so the sort is total; the break
    /// bounds only need the price order, but a total order makes the whole
    /// build reproducible from a shuffled input.
    #[must_use]
    pub fn from_markets(markets: &[Market]) -> Self {
        let mut suppliers: HashMap<u32, Vec<SupplyRow>> = HashMap::new();
        let mut buyers: HashMap<u32, Vec<DemandRow>> = HashMap::new();

        for (node, market) in markets.iter().enumerate() {
            let node = node as u32;
            for &row in &market.supply {
                suppliers.entry(row.commodity.0).or_default().push(SupplyRow { node, row });
            }
            for &row in &market.demand {
                buyers.entry(row.commodity.0).or_default().push(DemandRow { node, row });
            }
        }

        let mut pools: Vec<Pool> = suppliers
            .into_iter()
            .filter_map(|(commodity, mut sell)| {
                let mut buy = buyers.remove(&commodity)?;
                sell.sort_by(|a, b| {
                    a.row.buy_price.cmp(&b.row.buy_price).then(a.node.cmp(&b.node))
                });
                buy.sort_by(|a, b| {
                    b.row.sell_price.cmp(&a.row.sell_price).then(a.node.cmp(&b.node))
                });
                Some(Pool { commodity: CommodityId(commodity), suppliers: sell, buyers: buy })
            })
            .collect();

        pools.sort_by_key(|pool| pool.commodity.0);
        Self { pools }
    }
}

/// How much work the build did, and how much it skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Pairs the naive product would have evaluated.
    pub pairs_total: u64,
    /// Pairs actually evaluated. The ratio is the evidence the bounds work.
    pub pairs_visited: u64,
    /// Commodities skipped whole by the outermost bound.
    pub commodities_pruned: u32,
    /// Edges in the finished graph.
    pub edges: u32,
    /// Whether a profit floor was in force.
    ///
    /// A floor drops *edges*, which narrows the stated feasible set: the answer
    /// stays exactly optimal, but for a different question, and the report has
    /// to name the floor. This records that the question changed rather than
    /// that a particular edge was discarded — the three break bounds abandon
    /// whole runs of candidates at once and never learn how many.
    pub profit_floor_applied: bool,
}

/// Markets and the best hop between each ordered pair, stored compressed by row.
///
/// Compressed sparse row rather than a dense matrix: two dense `i64` matrices
/// would be 400 MB at five thousand markets, and real trade graphs are sparse
/// because most pairs have nothing to trade.
#[derive(Clone, Debug)]
pub struct TradeGraph {
    n: usize,
    row_start: Vec<u32>,
    edge_to: Vec<u32>,
    edge_weight: Vec<Credits>,
    edge_millis: Vec<Millis>,
    edge_choice: Vec<LegChoice>,
    max_out: Vec<Credits>,
    max_in: Vec<Credits>,
    global_max: Credits,
    max_millis: Millis,
    /// What the build cost.
    pub stats: BuildStats,
}

struct EdgeRecord {
    from: u32,
    to: u32,
    weight: Credits,
    millis: Millis,
    choice: LegChoice,
}

impl TradeGraph {
    /// Builds the graph, commodity-major.
    ///
    /// The build has no deadline, only a voice. It is the one phase whose cost
    /// the plan already prices — it is quadratic in a market count the user was
    /// shown and agreed to before a request was sent — and abandoning it half
    /// way leaves a graph missing exactly the legs the bounds ranked highest,
    /// which is a wrong answer rather than a partial one. So it says where it
    /// has got to and finishes.
    #[must_use]
    pub fn build(
        pools: &Pools,
        geometry: &Geometry<'_>,
        ship: &ShipConfig,
        limits: &Limits,
        watch: Watch<'_>,
    ) -> Self {
        let markets = geometry.markets;
        let mut stats =
            BuildStats { profit_floor_applied: limits.min_profit.0 > 0, ..BuildStats::default() };
        let mut best: HashMap<u64, usize> = HashMap::new();
        let mut records: Vec<EdgeRecord> = Vec::new();
        let mut millis_cache: HashMap<u64, Millis> = HashMap::new();

        // Bound 0, per commodity: the widest spread in the pool, taken at the
        // largest hold that spread can be bought at. Nothing in the pool can
        // beat it, so the pools are visited in descending bound order and the
        // first one that cannot clear the floor ends the loop.
        let mut ordered: Vec<(&Pool, Credits)> =
            pools.pools.iter().map(|pool| (pool, commodity_bound(pool, ship))).collect();
        ordered.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.commodity.0.cmp(&b.0.commodity.0)));

        for pool in &pools.pools {
            stats.pairs_total += pool.suppliers.len() as u64 * pool.buyers.len() as u64;
        }

        let total = ordered.len();
        let say = |done: usize, edges: usize| watch.report(Event::Building { done, total, edges });

        let mut rows_since_report = 0usize;
        for (seen, &(pool, bound)) in ordered.iter().enumerate() {
            say(seen, records.len());
            if bound <= limits.min_profit {
                // Bound 0. The pools are in descending bound order, so no later
                // one can clear the floor either.
                stats.commodities_pruned = (ordered.len() - seen) as u32;
                break;
            }
            let Some(best_buyer) = pool.buyers.first() else { continue };
            let best_sell = best_buyer.row.sell_price;

            for supplier in &pool.suppliers {
                rows_since_report += 1;
                if rows_since_report >= ROWS_PER_REPORT {
                    rows_since_report = 0;
                    say(seen, records.len());
                }
                let buyable = min_tons(ship.cargo, affordable(ship.credits, supplier.row.buy_price));
                // Bound 1. Both factors are non-increasing in the supply sort,
                // so this is a `break`. `stock` is deliberately absent: it is
                // not monotone in the sort, and including it here would prune
                // rows that a later, thinner supplier cannot rule out.
                let outer = (best_sell - supplier.row.buy_price) * buyable;
                if outer <= limits.min_profit {
                    break;
                }

                let units_cap = min_tons(buyable, supplier.row.stock);
                // A thin supply row is skipped, not terminal.
                if units_cap < limits.min_units {
                    continue;
                }

                for buyer in &pool.buyers {
                    // Bound 2, non-increasing in the demand sort.
                    let inner = (buyer.row.sell_price - supplier.row.buy_price) * units_cap;
                    if inner <= limits.min_profit {
                        break;
                    }
                    if buyer.node == supplier.node {
                        continue;
                    }
                    if limits.exclude_same_system
                        && markets[buyer.node as usize].system_address
                            == markets[supplier.node as usize].system_address
                    {
                        continue;
                    }

                    stats.pairs_visited += 1;
                    let Some(choice) = leg_weight(
                        &supplier.row,
                        &buyer.row,
                        ship,
                        ship.credits,
                        limits.min_units,
                    ) else {
                        continue;
                    };
                    if choice.profit <= limits.min_profit {
                        continue;
                    }

                    let key = edge_key(supplier.node, buyer.node);
                    let millis = *millis_cache
                        .entry(key)
                        .or_insert_with(|| geometry.leg_millis(supplier.node, buyer.node));
                    match best.get(&key) {
                        Some(&at) if records[at].weight >= choice.profit => {}
                        Some(&at) => {
                            records[at].weight = choice.profit;
                            records[at].choice = choice;
                        }
                        None => {
                            best.insert(key, records.len());
                            records.push(EdgeRecord {
                                from: supplier.node,
                                to: buyer.node,
                                weight: choice.profit,
                                millis,
                                choice,
                            });
                        }
                    }
                }
            }
        }

        stats.edges = records.len() as u32;
        // The closing report, so a watcher sees the phase end rather than a
        // count that stopped moving.
        say(total, records.len());
        Self::from_records(markets.len(), records, stats)
    }

    fn from_records(n: usize, mut records: Vec<EdgeRecord>, stats: BuildStats) -> Self {
        // Sorted by target within a row so the reverse edge of a round trip is
        // a binary search rather than a scan.
        records.sort_by(|a, b| a.from.cmp(&b.from).then(a.to.cmp(&b.to)));

        let mut row_start = vec![0u32; n + 1];
        for record in &records {
            row_start[record.from as usize + 1] += 1;
        }
        for i in 0..n {
            row_start[i + 1] += row_start[i];
        }

        let mut max_out = vec![Credits::ZERO; n];
        let mut max_in = vec![Credits::ZERO; n];
        let mut global_max = Credits::ZERO;
        let mut max_millis = Millis::ZERO;
        for record in &records {
            if record.weight > max_out[record.from as usize] {
                max_out[record.from as usize] = record.weight;
            }
            if record.weight > max_in[record.to as usize] {
                max_in[record.to as usize] = record.weight;
            }
            if record.weight > global_max {
                global_max = record.weight;
            }
            if record.millis > max_millis {
                max_millis = record.millis;
            }
        }

        Self {
            n,
            row_start,
            edge_to: records.iter().map(|r| r.to).collect(),
            edge_weight: records.iter().map(|r| r.weight).collect(),
            edge_millis: records.iter().map(|r| r.millis).collect(),
            edge_choice: records.iter().map(|r| r.choice).collect(),
            max_out,
            max_in,
            global_max,
            max_millis,
            stats,
        }
    }

    /// How many markets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.n
    }

    /// Whether there are no markets at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// How many edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edge_to.len()
    }

    /// The half-open range of edge indices leaving `node`.
    #[must_use]
    pub fn row(&self, node: u32) -> std::ops::Range<usize> {
        self.row_start[node as usize] as usize..self.row_start[node as usize + 1] as usize
    }

    /// The destination of an edge.
    #[must_use]
    pub fn target(&self, edge: usize) -> u32 {
        self.edge_to[edge]
    }

    /// The profit an edge earns.
    #[must_use]
    pub fn weight(&self, edge: usize) -> Credits {
        self.edge_weight[edge]
    }

    /// The wall-clock an edge costs.
    #[must_use]
    pub fn millis(&self, edge: usize) -> Millis {
        self.edge_millis[edge]
    }

    /// Every edge's profit, in edge order.
    ///
    /// Exists so a component can address these rather than copy them. At
    /// 24 million legs each array is 185 MiB, which is not a rounding error on
    /// a machine already holding 1.6 GiB of graph.
    #[must_use]
    pub fn weights(&self) -> &[Credits] {
        &self.edge_weight
    }

    /// Every edge's wall-clock, in edge order. See [`TradeGraph::weights`].
    #[must_use]
    pub fn times(&self) -> &[Millis] {
        &self.edge_millis
    }

    /// The trade an edge performs.
    #[must_use]
    pub fn choice(&self, edge: usize) -> LegChoice {
        self.edge_choice[edge]
    }

    /// The edge `from -> to`, if there is one.
    #[must_use]
    pub fn find(&self, from: u32, to: u32) -> Option<usize> {
        let range = self.row(from);
        self.edge_to[range.clone()].binary_search(&to).ok().map(|at| range.start + at)
    }

    /// The best profit leaving a node.
    #[must_use]
    pub fn max_out(&self, node: u32) -> Credits {
        self.max_out[node as usize]
    }

    /// The best profit arriving at a node.
    #[must_use]
    pub fn max_in(&self, node: u32) -> Credits {
        self.max_in[node as usize]
    }

    /// The best profit anywhere in the graph.
    #[must_use]
    pub fn global_max(&self) -> Credits {
        self.global_max
    }

    /// The slowest single leg, which upper-bounds any completion's time.
    #[must_use]
    pub fn max_millis(&self) -> Millis {
        self.max_millis
    }

    /// Every edge, as `(from, to, index)`.
    pub fn edges(&self) -> impl Iterator<Item = (u32, u32, usize)> + '_ {
        (0..self.n as u32).flat_map(move |from| {
            self.row(from).map(move |edge| (from, self.edge_to[edge], edge))
        })
    }

    /// Strongly connected components, each as a list of nodes.
    ///
    /// A cycle lies wholly inside one component, so this decomposition is what
    /// lets the ratio solver run Bellman-Ford over a handful of small graphs
    /// instead of one large one. Only components with more than one node can
    /// hold a cycle: there are no self-loops, because a market cannot trade
    /// with itself.
    ///
    /// Tarjan's algorithm, iterative. Recursion would be fine at the sizes
    /// reached today and would be a stack overflow at the sizes a wider radius
    /// reaches tomorrow.
    #[must_use]
    pub fn sccs(&self) -> Vec<Vec<u32>> {
        let mut index = vec![u32::MAX; self.n];
        let mut low = vec![0u32; self.n];
        let mut on_stack = vec![false; self.n];
        let mut stack: Vec<u32> = Vec::new();
        let mut next = 0u32;
        let mut out: Vec<Vec<u32>> = Vec::new();
        // (node, next edge to examine)
        let mut frames: Vec<(u32, usize)> = Vec::new();

        for root in 0..self.n as u32 {
            if index[root as usize] != u32::MAX {
                continue;
            }
            frames.push((root, self.row(root).start));
            index[root as usize] = next;
            low[root as usize] = next;
            next += 1;
            stack.push(root);
            on_stack[root as usize] = true;

            while !frames.is_empty() {
                let top = frames.len() - 1;
                let (node, cursor) = frames[top];
                let row = self.row(node);
                if cursor < row.end {
                    frames[top].1 = cursor + 1;
                    let to = self.edge_to[cursor];
                    if index[to as usize] == u32::MAX {
                        index[to as usize] = next;
                        low[to as usize] = next;
                        next += 1;
                        stack.push(to);
                        on_stack[to as usize] = true;
                        frames.push((to, self.row(to).start));
                    } else if on_stack[to as usize] {
                        low[node as usize] = low[node as usize].min(index[to as usize]);
                    }
                    continue;
                }

                frames.pop();
                if low[node as usize] == index[node as usize] {
                    let mut component = Vec::new();
                    while let Some(top) = stack.pop() {
                        on_stack[top as usize] = false;
                        component.push(top);
                        if top == node {
                            break;
                        }
                    }
                    component.sort_unstable();
                    out.push(component);
                }
                if let Some(&(parent, _)) = frames.last() {
                    low[parent as usize] = low[parent as usize].min(low[node as usize]);
                }
            }
        }

        out.sort_by(|a, b| a[0].cmp(&b[0]));
        out
    }
}

fn commodity_bound(pool: &Pool, ship: &ShipConfig) -> Credits {
    let (Some(cheapest), Some(dearest)) = (pool.suppliers.first(), pool.buyers.first()) else {
        return Credits::ZERO;
    };
    let spread = dearest.row.sell_price - cheapest.row.buy_price;
    if spread.0 <= 0 {
        return Credits::ZERO;
    }
    spread * min_tons(ship.cargo, affordable(ship.credits, cheapest.row.buy_price))
}

fn min_tons(a: Tons, b: Tons) -> Tons {
    if a < b { a } else { b }
}

fn edge_key(from: u32, to: u32) -> u64 {
    (u64::from(from) << 32) | u64::from(to)
}

#[cfg(test)]
mod tests {
    use super::{Event, Pools, TradeGraph, Watch};
    use crate::fixture::{geometry, limits, market, ship};
    use crate::model::{CommodityId, Limits};
    use crate::num::{Credits, Tons};

    fn build(markets: &[crate::model::Market], limits: &Limits) -> TradeGraph {
        TradeGraph::build(
            &Pools::from_markets(markets),
            &geometry(markets),
            &ship(),
            limits,
            Watch::unlimited(),
        )
    }

    #[test]
    fn a_pair_with_no_shared_commodity_produces_no_edge() {
        let markets =
            [market(1, 0.0, &[(0, 100, 500)], &[]), market(2, 5.0, &[], &[(1, 900, 500)])];
        assert_eq!(build(&markets, &limits()).edge_count(), 0);
    }

    #[test]
    fn the_best_commodity_wins_the_edge() {
        let markets = [
            market(1, 0.0, &[(0, 100, 500), (1, 100, 500)], &[]),
            market(2, 5.0, &[], &[(0, 200, 500), (1, 900, 500)]),
        ];
        let graph = build(&markets, &limits());
        assert_eq!(graph.edge_count(), 1);
        let edge = graph.find(0, 1).expect("an edge");
        assert_eq!(graph.choice(edge).commodity, CommodityId(1));
        assert_eq!(graph.weight(edge), Credits(500 * 800));
    }

    #[test]
    fn a_thin_supply_row_does_not_end_the_supplier_scan() {
        // The cheapest supplier holds one ton, the next holds plenty. If the
        // thin row were a `break` rather than a `continue`, the profitable
        // second row would never be reached — and stock is not monotone in the
        // price sort, so nothing about the first row bounds the second.
        let markets = [
            market(1, 0.0, &[(0, 100, 1)], &[]),
            market(2, 1.0, &[(0, 101, 500)], &[]),
            market(3, 5.0, &[], &[(0, 900, 500)]),
        ];
        let graph = build(&markets, &Limits { min_units: Tons(2), ..limits() });
        assert!(graph.find(0, 2).is_none());
        assert_eq!(graph.weight(graph.find(1, 2).expect("an edge")), Credits(500 * 799));
    }

    #[test]
    fn sccs_separate_a_cycle_from_a_dead_end() {
        // 0 and 1 trade both ways; 2 only buys, so it is its own component.
        let markets = [
            market(1, 0.0, &[(0, 100, 500)], &[(1, 900, 500)]),
            market(2, 1.0, &[(1, 100, 500)], &[(0, 900, 500)]),
            market(3, 2.0, &[], &[(0, 950, 500)]),
        ];
        let sccs = build(&markets, &limits()).sccs();
        let big: Vec<&Vec<u32>> = sccs.iter().filter(|c| c.len() > 1).collect();
        assert_eq!(big, vec![&vec![0u32, 1]]);
        assert_eq!(sccs, vec![vec![0u32, 1], vec![2]]);
    }

    #[test]
    fn the_bounds_visit_far_fewer_pairs_than_the_product() {
        // One rich commodity and forty worthless ones across ten markets.
        let mut markets = Vec::new();
        for i in 0..10i64 {
            let mut supply = vec![(0u32, 100, 500)];
            let mut demand = vec![(0u32, 110, 500)];
            for c in 1..41u32 {
                supply.push((c, 1_000, 500));
                demand.push((c, 999, 500));
            }
            if i == 9 {
                demand[0] = (0, 5_000, 500);
            }
            markets.push(market(i + 1, i as f64, &supply, &demand));
        }
        let graph = build(&markets, &limits());
        assert!(graph.stats.pairs_visited * 4 < graph.stats.pairs_total, "{:?}", graph.stats);
        assert!(graph.stats.commodities_pruned >= 40, "{:?}", graph.stats);
    }

    #[test]
    fn the_build_reports_progress_that_ends_at_the_pool_count() {
        // Silence is the defect: 127 seconds of it at 5,049 markets, measured
        // 2026-08-06. The last report must say the phase finished, because a
        // counter that merely stops is indistinguishable from a stall.
        let markets = [
            market(1, 0.0, &[(0, 100, 500), (1, 100, 500)], &[]),
            market(2, 5.0, &[], &[(0, 900, 500), (1, 900, 500)]),
        ];
        let seen = std::cell::RefCell::new(Vec::new());
        let sink = |event: Event| seen.borrow_mut().push(event);
        let graph = TradeGraph::build(
            &Pools::from_markets(&markets),
            &geometry(&markets),
            &ship(),
            &limits(),
            Watch::unlimited().reporting(&sink),
        );
        let seen = seen.into_inner();
        let Some(&Event::Building { done, total, edges }) = seen.last() else {
            panic!("the build reported nothing: {seen:?}");
        };
        assert_eq!((done, total), (2, 2), "two commodities, both finished");
        assert_eq!(edges, graph.edge_count());
        // And it said something before it finished, so a long build moves.
        assert!(seen.len() > 1, "{seen:?}");
    }
}
