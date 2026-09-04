//! Owned, display-ready views of a route and its markets \[C53\].
//!
//! A [`Route`] addresses its markets by index and its commodities by an
//! interner id, so it cannot be shown without the instance it was solved in.
//! A card can: it carries names, prices and the trade commands as strings, so
//! the screen keeps drawing while the instance is away being re-priced.

use edm_core::ardent::ArdentStation;
use edm_core::domain::id64::Coordinates;
use edm_core::render::Block;
use edm_route::model::{Commodities, Market};
use edm_route::pin::PinKey;
use edm_route::report::Route;
use edm_route::view;

use crate::route::acquire::Listing;

/// One leg, in words.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LegCard {
    pub from: String,
    pub from_system: String,
    pub to: String,
    pub to_system: String,
    pub commodity: String,
    pub units: i64,
    pub buy: i64,
    pub sell: i64,
    pub profit: i64,
    pub distance_ly: f64,
    pub limiter: &'static str,
}

/// A route, in words.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RouteCard {
    pub key: PinKey,
    pub legs: Vec<LegCard>,
    pub profit: i64,
    /// The rate the ranking used, in credits per hour.
    pub per_hour: i64,
    /// The steady-state rate, when the shape has one.
    pub steady_per_hour: Option<i64>,
    pub lap_millis: i64,
    pub first_lap_millis: i64,
    /// Light years from the ship to the first station, when the ship is known.
    pub approach_ly: Option<f64>,
    pub guarantee: String,
    pub caveats: Vec<String>,
    /// The `edm trade` lines, one per leg side.
    pub commands: Vec<String>,
    /// `view::legs`, for the detail pane.
    pub legs_blocks: Vec<Block<'static>>,
    /// Every market the route touches, in flying order.
    pub market_ids: Vec<f64>,
}

impl RouteCard {
    pub(crate) fn of(
        route: &Route,
        markets: &[Market],
        commodities: &Commodities,
        origin: Option<Coordinates>,
        cargo: Option<i64>,
    ) -> Self {
        let market = |index: u32| &markets[index as usize];
        let legs = route
            .legs
            .iter()
            .map(|leg| {
                let from = market(leg.from);
                let to = market(leg.to);
                LegCard {
                    from: from.station.clone(),
                    from_system: from.system.clone(),
                    to: to.station.clone(),
                    to_system: to.system.clone(),
                    commodity: commodities
                        .name(leg.choice.commodity)
                        .map_or_else(|| "?".to_owned(), view::readable),
                    units: leg.choice.units.0,
                    buy: leg.choice.buy_price.0,
                    sell: leg.choice.sell_price.0,
                    profit: leg.choice.profit.0,
                    distance_ly: leg.distance_ly,
                    limiter: view::limiter(leg.choice.limiter),
                }
            })
            .collect();
        let claim = route.rate();
        let approach_ly = origin.and_then(|at| {
            route
                .legs
                .first()
                .map(|leg| edm_route::time::distance_ly(at, market(leg.from).coords))
        });
        let commands = view::trade_commands(std::slice::from_ref(route), markets, commodities, cargo)
            .into_iter()
            .filter_map(|block| match block {
                Block::Raw(line) => Some(edm_core::js::text::js_trim(&line).to_owned()),
                _ => None,
            })
            .collect();
        let mut market_ids: Vec<f64> = route
            .legs
            .iter()
            .map(|leg| market(leg.from).market_id as f64)
            .collect();
        if let Some(last) = route.legs.last()
            && !route.kind.is_cycle()
        {
            market_ids.push(market(last.to).market_id as f64);
        }
        Self {
            key: PinKey::of(route, markets, commodities),
            legs,
            profit: route.profit.0,
            per_hour: route.rank.rate.credits_per_hour_floor(),
            steady_per_hour: claim.steady.map(edm_route::num::Ratio::credits_per_hour_floor),
            lap_millis: route.cycle_millis.0,
            first_lap_millis: route.first_lap_millis.0,
            approach_ly,
            guarantee: view::claim(claim.guarantee),
            caveats: claim.caveats.iter().map(|caveat| view::explain(*caveat).to_owned()).collect(),
            commands,
            legs_blocks: view::legs(route, markets, commodities),
            market_ids,
        }
    }

    /// `Station A > Station B` in flying order.
    pub(crate) fn path(&self) -> String {
        let mut names: Vec<&str> = self.legs.iter().map(|leg| leg.from.as_str()).collect();
        if let Some(last) = self.legs.last() {
            names.push(&last.to);
        }
        names.join(" > ")
    }

    /// The commodities carried, distinct, in leg order.
    pub(crate) fn cargo(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        for leg in &self.legs {
            if !names.contains(&leg.commodity.as_str()) {
                names.push(&leg.commodity);
            }
        }
        names.join(", ")
    }
}

/// One commodity row of a market, as read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InventoryRow {
    pub name: String,
    pub stock: f64,
    pub stock_bracket: f64,
    pub demand: f64,
    pub demand_bracket: f64,
    pub buy: f64,
    pub sell: f64,
    pub mean: f64,
}

/// A market, as read, with what a pinned route needs from it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MarketCard {
    pub market_id: f64,
    pub station: String,
    pub system: String,
    pub station_type: Option<String>,
    pub pad: Option<f64>,
    pub arrival_ls: Option<f64>,
    /// When the listing was read, in the run's clock.
    pub read_at_ms: Option<f64>,
    pub observed_at_ms: Option<f64>,
    /// `read live`, `from the cache`, or why it was not read.
    pub status: String,
    /// The docking verdict, for a carrier.
    pub access: Option<String>,
    /// What this commander's own ship learned at the door, for a carrier.
    pub door: Option<String>,
    /// The route's commodities at this market.
    pub rows: Vec<InventoryRow>,
}

impl MarketCard {
    /// A card for `station`, from whatever the sweep read of it.
    pub(crate) fn of(
        station: &ArdentStation,
        listing: Option<&Listing>,
        unreached: Option<&str>,
        commodities: &[String],
    ) -> Self {
        let wanted: Vec<String> = commodities
            .iter()
            .map(|name| edm_core::ardent::normalise_commodity_name(name))
            .collect();
        let mut rows = Vec::new();
        let mut status = unreached.map_or_else(|| "not read".to_owned(), |why| format!("not read: {why}"));
        if let Some(listing) = listing {
            status = if listing.from_cache {
                "from the cache".to_owned()
            } else {
                "read live".to_owned()
            };
            if let Some(snapshot) = listing.snapshot() {
                for commodity in &snapshot.commodities {
                    let symbol = edm_core::ardent::normalise_commodity_name(commodity.name);
                    if !wanted.is_empty() && !wanted.contains(&symbol) {
                        continue;
                    }
                    rows.push(InventoryRow {
                        name: commodity.name.to_owned(),
                        stock: commodity.stock,
                        stock_bracket: commodity.stock_bracket,
                        demand: commodity.demand,
                        demand_bracket: commodity.demand_bracket,
                        buy: commodity.buy_price,
                        sell: commodity.sell_price,
                        mean: commodity.mean_price,
                    });
                }
            }
        }
        Self {
            market_id: station.market_id,
            station: station.station_name.clone(),
            system: station.system_name.clone(),
            station_type: station.station_type.clone(),
            pad: station.max_landing_pad_size,
            arrival_ls: station.distance_to_arrival,
            read_at_ms: listing.map(|l| l.read_at_ms),
            observed_at_ms: listing.and_then(|l| l.observed_at_ms),
            status,
            access: None,
            door: None,
            rows,
        }
    }
}
