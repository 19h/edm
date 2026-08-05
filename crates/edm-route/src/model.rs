//! The instance: markets and their rows, the ship, and the search limits.
//!
//! Nothing here reads a file or a socket. The I/O layer decodes a Companion API
//! payload into [`RawCommodity`] rows and hands them to [`Market::from_rows`],
//! which is also where the ingest invariants are checked and counted.

use std::collections::HashMap;

use edm_core::domain::id64::Coordinates;

use crate::num::{Credits, Tons};

/// An index into a [`Commodities`] table.
///
/// Commodity names are interned because the graph build is commodity-major and
/// pivots on equality of commodity roughly a million times per sweep; comparing
/// a `u32` rather than a `String` is the difference between a build that is
/// dominated by arithmetic and one dominated by hashing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommodityId(pub u32);

/// The name table behind [`CommodityId`].
#[derive(Debug, Default)]
pub struct Commodities {
    names: Vec<String>,
    index: HashMap<String, u32>,
}

impl Commodities {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the id for `name`, assigning one if it is new.
    pub fn intern(&mut self, name: &str) -> CommodityId {
        if let Some(&id) = self.index.get(name) {
            return CommodityId(id);
        }
        let id = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.index.insert(name.to_owned(), id);
        CommodityId(id)
    }

    /// The name behind an id, or `None` if it came from another table.
    #[must_use]
    pub fn name(&self, id: CommodityId) -> Option<&str> {
        self.names.get(id.0 as usize).map(String::as_str)
    }

    /// How many distinct commodities have been interned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether nothing has been interned yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// A commodity a market sells, with the price it asks and the stock it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Supply {
    /// What is on the shelf.
    pub commodity: CommodityId,
    /// What the commander pays per ton. Strictly positive by ingest.
    pub buy_price: Credits,
    /// How many tons are on the shelf.
    pub stock: Tons,
}

/// How much a market will take.
///
/// The two cases are not a convenience: they are a data-driven finding. A row
/// with `demand == 0` and `demandBracket >= 1` is a market that buys *without
/// publishing a quantity* — measured on gold imports at sell prices of
/// 59,759–66,217, i.e. among the best targets in any pool. Taking the zero
/// literally zeroes `units` and silently deletes the best rows in the sweep.
/// This matters more here than in the sibling project, because every Companion
/// API market returns the same 391-entry commodity map with most rows priced
/// but idle, so the quantity fields are the only thing separating a real
/// trading partner from a row that exists because the schema has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemandQty {
    /// The market published a quantity, and it is this.
    Published(Tons),
    /// The market publishes a bracket but no quantity. Treated as
    /// cargo-limited, which is the smallest assumption that keeps the row.
    Unpublished,
}

/// A commodity a market buys, with the price it pays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Demand {
    /// What it wants.
    pub commodity: CommodityId,
    /// What it pays per ton. Strictly positive by ingest.
    pub sell_price: Credits,
    /// How much it will take.
    pub qty: DemandQty,
}

/// One market, as the optimiser sees it.
///
/// The identity fields are carried through so a result can name a station
/// without the caller holding a second table, and `market_id` is the absolute
/// tie-break that makes the ranking independent of input order.
#[derive(Clone, Debug)]
pub struct Market {
    /// Frontier's market id — unique, and the tie-break of last resort.
    pub market_id: i64,
    /// Station name, for the report.
    pub station: String,
    /// System name, for the report.
    pub system: String,
    /// System address, used only to recognise a same-system pair.
    pub system_address: i64,
    /// Galactic position of the system, for the leg distance.
    pub coords: Coordinates,
    /// Distance from the arrival star, in light seconds, for supercruise time.
    pub arrival_ls: f64,
    /// Rows this market sells.
    pub supply: Vec<Supply>,
    /// Rows this market buys.
    pub demand: Vec<Demand>,
}

/// One row of a decoded Companion API commodity map.
///
/// Deliberately close to the wire shape so the I/O layer does no interpreting.
/// The Companion API's `buyPrice` is what the commander pays to take a ton off
/// the station, and `sellPrice` is what the station pays to take one on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawCommodity {
    /// The commodity's internal name.
    pub name: String,
    /// What the station charges per ton.
    pub buy_price: i64,
    /// What the station pays per ton.
    pub sell_price: i64,
    /// Tons on the shelf.
    pub stock: i64,
    /// Supply bracket, 0–3.
    pub stock_bracket: i64,
    /// Tons wanted.
    pub demand: i64,
    /// Demand bracket, 0–3.
    pub demand_bracket: i64,
}

/// Why a row was not ingested, or was ingested under protest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestCounts {
    /// Rows offering no stock, or a non-positive ask.
    pub no_supply: u32,
    /// Rows wanting nothing, and not merely wanting an unstated amount.
    pub no_demand: u32,
    /// Rows where the station's own bid was at or above its ask.
    ///
    /// A market's buy price always exceeds its sell price — that spread is the
    /// station's margin — so for any one commodity at most one direction of a
    /// station pair can be profitable, and the two legs of a round trip
    /// necessarily carry different cargo. That is a property of the data and
    /// not an axiom, so it is checked and counted rather than assumed. The row
    /// is kept: the search does not depend on the invariant, only the
    /// explanation does, and silently dropping live data to protect a sentence
    /// is the wrong trade.
    pub bid_not_below_ask: u32,
    /// Rows kept as [`DemandQty::Unpublished`].
    pub demand_unpublished: u32,
}

impl Market {
    /// Builds a market from decoded rows, interning commodity names.
    ///
    /// `min_stock` and `min_demand` are applied here rather than in the graph
    /// build because a row that cannot fill a hold is noise in every search,
    /// and dropping it once is cheaper than skipping it in three inner loops.
    pub fn from_rows(
        identity: MarketIdentity,
        rows: &[RawCommodity],
        commodities: &mut Commodities,
        floors: RowFloors,
        counts: &mut IngestCounts,
    ) -> Self {
        let mut supply = Vec::new();
        let mut demand = Vec::new();

        for row in rows {
            let id = commodities.intern(&row.name);

            if row.buy_price > 0 && row.sell_price > 0 && row.sell_price >= row.buy_price {
                counts.bid_not_below_ask += 1;
            }

            if row.buy_price > 0 && row.stock >= floors.min_stock.0 && row.stock > 0 {
                supply.push(Supply {
                    commodity: id,
                    buy_price: Credits(row.buy_price),
                    stock: Tons(row.stock),
                });
            } else {
                counts.no_supply += 1;
            }

            match effective_qty(row, floors.min_demand) {
                Some(qty) => {
                    if qty == DemandQty::Unpublished {
                        counts.demand_unpublished += 1;
                    }
                    demand.push(Demand { commodity: id, sell_price: Credits(row.sell_price), qty });
                }
                None => counts.no_demand += 1,
            }
        }

        Self {
            market_id: identity.market_id,
            station: identity.station,
            system: identity.system,
            system_address: identity.system_address,
            coords: identity.coords,
            arrival_ls: identity.arrival_ls,
            supply,
            demand,
        }
    }
}

fn effective_qty(row: &RawCommodity, min_demand: Tons) -> Option<DemandQty> {
    if row.sell_price <= 0 {
        return None;
    }
    if row.demand > 0 {
        return if row.demand >= min_demand.0 {
            Some(DemandQty::Published(Tons(row.demand)))
        } else {
            None
        };
    }
    // The published-quantity floor cannot be applied to a market that publishes
    // no quantity; the bracket is all the evidence there is.
    if row.demand_bracket >= 1 {
        return Some(DemandQty::Unpublished);
    }
    None
}

/// Everything about a market that is not a price.
#[derive(Clone, Debug)]
pub struct MarketIdentity {
    /// Frontier's market id.
    pub market_id: i64,
    /// Station name.
    pub station: String,
    /// System name.
    pub system: String,
    /// System address.
    pub system_address: i64,
    /// Galactic position.
    pub coords: Coordinates,
    /// Distance from the arrival star, in light seconds.
    pub arrival_ls: f64,
}

/// Row floors applied at ingest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowFloors {
    /// Stock below this is not worth a station visit.
    pub min_stock: Tons,
    /// Published demand below this is not worth a station visit. Not applied to
    /// unpublished demand, which has no quantity to compare.
    pub min_demand: Tons,
}

impl Default for RowFloors {
    fn default() -> Self {
        Self { min_stock: Tons(1), min_demand: Tons(1) }
    }
}

/// The hold and the balance the search plans against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShipConfig {
    /// Hold capacity in tons.
    pub cargo: Tons,
    /// Starting balance.
    ///
    /// Leg weights use the *starting* balance throughout. That is a
    /// conservative simplification and not an optimistic one — more credits can
    /// only raise `units`, which can only raise profit — and it is what keeps
    /// the weight matrix independent of where in the route you are, which is
    /// what makes the graph algorithms valid at all. Finalists are re-evaluated
    /// with credits accumulating, and re-ranked afterwards.
    pub credits: Credits,
}

impl Default for ShipConfig {
    fn default() -> Self {
        Self { cargo: Tons(1232), credits: Credits(1_000_000_000) }
    }
}

/// How a hold is filled on one leg.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FillPolicy {
    /// One commodity, chosen as the argmax over the pair. Exact, and the
    /// objective every solver in this crate optimises.
    #[default]
    BestCommodity,
    /// Fill the remaining hold with the next best commodities.
    ///
    /// Under a binding credit cap this is a two-resource knapsack and no bound
    /// was found for it, so it is offered only as a post-hoc re-evaluation of
    /// finalists and it downgrades the guarantee to `Heuristic`. It never
    /// participates in the search itself.
    GreedyFill,
}

/// Everything that narrows the search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How many routes to report per shape.
    pub top_n: usize,
    /// How much wider than `top_n` the shortlist handed to credit rethreading
    /// is. Rethreading re-ranks, so truncating to `top_n` first would let a
    /// route that threading promotes past the cut fall off the list before it
    /// could.
    pub shortlist_factor: usize,
    /// An edge worth no more than this is not an edge.
    ///
    /// This drops *edges*, which changes the stated feasible set; the result
    /// stays exactly optimal for that set, and the report names the floor. It
    /// is categorically different from dropping *nodes*, which approximates the
    /// search itself.
    pub min_profit: Credits,
    /// A leg carrying fewer tons than this is not worth flying.
    pub min_units: Tons,
    /// Whether two markets in the same system may form a leg.
    pub exclude_same_system: bool,
    /// Cap on the number of stops in a loop, if any.
    pub max_stops: Option<usize>,
    /// Require a loop to visit at least this many distinct stations.
    ///
    /// This, and not "distinct stations", is the genuinely hard variant: a
    /// two-stop shuttle drains both ends in a single lap, and "give me a
    /// five-stop loop" is the request people actually have.
    pub min_distinct: Option<usize>,
    /// How a hold is filled when finalists are re-evaluated.
    pub fill: FillPolicy,
    /// How many search nodes the `min_distinct` branch and bound may expand
    /// before it gives up and reports a bounded gap instead of an optimum.
    pub search_budget: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            top_n: 20,
            shortlist_factor: 4,
            min_profit: Credits(0),
            min_units: Tons(1),
            exclude_same_system: false,
            max_stops: None,
            min_distinct: None,
            fill: FillPolicy::BestCommodity,
            search_budget: 20_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Commodities, DemandQty, IngestCounts, Market, MarketIdentity, RawCommodity, RowFloors,
    };
    use crate::num::Tons;
    use edm_core::domain::id64::Coordinates;

    fn identity() -> MarketIdentity {
        MarketIdentity {
            market_id: 1,
            station: "Galileo".to_owned(),
            system: "Sol".to_owned(),
            system_address: 10_477_373_803,
            coords: Coordinates { x: 0.0, y: 0.0, z: 0.0 },
            arrival_ls: 505.0,
        }
    }

    fn row(name: &str, buy: i64, sell: i64, stock: i64, demand: i64, bracket: i64) -> RawCommodity {
        RawCommodity {
            name: name.to_owned(),
            buy_price: buy,
            sell_price: sell,
            stock,
            stock_bracket: if stock > 0 { 2 } else { 0 },
            demand,
            demand_bracket: bracket,
        }
    }

    #[test]
    fn unpublished_demand_survives_ingest() {
        let mut commodities = Commodities::new();
        let mut counts = IngestCounts::default();
        let rows = [row("gold", 0, 59_759, 0, 0, 2)];
        let market = Market::from_rows(
            identity(),
            &rows,
            &mut commodities,
            RowFloors::default(),
            &mut counts,
        );
        assert_eq!(market.demand.len(), 1);
        assert_eq!(market.demand[0].qty, DemandQty::Unpublished);
        assert_eq!(counts.demand_unpublished, 1);
    }

    #[test]
    fn a_zero_bracket_zero_demand_row_is_not_a_buyer() {
        let mut commodities = Commodities::new();
        let mut counts = IngestCounts::default();
        // The shape of the great majority of the 391 rows every market returns.
        let rows = [row("tritium", 0, 40_000, 0, 0, 0)];
        let market = Market::from_rows(
            identity(),
            &rows,
            &mut commodities,
            RowFloors::default(),
            &mut counts,
        );
        assert!(market.demand.is_empty());
        assert_eq!(counts.no_demand, 1);
    }

    #[test]
    fn a_bid_at_or_above_the_ask_is_counted_and_kept() {
        let mut commodities = Commodities::new();
        let mut counts = IngestCounts::default();
        let rows = [row("painite", 500, 500, 10, 10, 3)];
        let market = Market::from_rows(
            identity(),
            &rows,
            &mut commodities,
            RowFloors::default(),
            &mut counts,
        );
        assert_eq!(counts.bid_not_below_ask, 1);
        assert_eq!(market.supply.len(), 1);
        assert_eq!(market.demand.len(), 1);
    }

    #[test]
    fn the_published_demand_floor_does_not_touch_an_unpublished_row() {
        let mut commodities = Commodities::new();
        let mut counts = IngestCounts::default();
        let rows = [row("gold", 0, 59_759, 0, 0, 2), row("silver", 0, 4_000, 0, 5, 1)];
        let floors = RowFloors { min_stock: Tons(1), min_demand: Tons(100) };
        let market =
            Market::from_rows(identity(), &rows, &mut commodities, floors, &mut counts);
        assert_eq!(market.demand.len(), 1);
        assert_eq!(market.demand[0].qty, DemandQty::Unpublished);
    }
}
