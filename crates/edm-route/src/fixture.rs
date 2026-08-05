//! Instance builders for the unit tests.
//!
//! These live outside the solving path on purpose. The exactness test greps the
//! seven files where the search happens for any mention of a floating-point
//! type, and a test fixture that placed a station in space would trip it — so
//! the fixtures live here and the gated files stay clean, tests included.

use crate::model::{
    CommodityId, Demand, DemandQty, Limits, Market, ShipConfig, Supply,
};
use crate::num::{Credits, Tons};
use crate::time::{Geometry, TimeModel};
use crate::weight::{LegChoice, Limiter};
use edm_core::domain::id64::Coordinates;

/// `(commodity, price, quantity)`.
pub(crate) type Row = (u32, i64, i64);

/// A market at `x` light years along the x axis, docked at its star.
pub(crate) fn market(id: i64, x: f64, supply: &[Row], demand: &[Row]) -> Market {
    at(id, x, 0.0, supply, demand)
}

/// A market at `x` light years, `arrival_ls` light seconds from its star.
pub(crate) fn at(id: i64, x: f64, arrival_ls: f64, supply: &[Row], demand: &[Row]) -> Market {
    Market {
        market_id: id,
        station: format!("Station {id}"),
        system: format!("System {id}"),
        system_address: id,
        coords: Coordinates { x, y: 0.0, z: 0.0 },
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
            })
            .collect(),
    }
}

/// The default time model over a market list.
pub(crate) fn geometry(markets: &[Market]) -> Geometry<'_> {
    Geometry::new(markets, TimeModel::default())
}

/// A ship with a big enough hold and balance that neither binds.
pub(crate) fn ship() -> ShipConfig {
    ShipConfig { cargo: Tons(1_000), credits: Credits(1_000_000_000_000) }
}

/// Limits that filter nothing and keep everything found.
pub(crate) fn limits() -> Limits {
    Limits { top_n: 64, shortlist_factor: 1, ..Limits::default() }
}

/// A leg carrying `profit` credits of one commodity, with nothing binding.
pub(crate) fn choice(commodity: u32, profit: i64) -> LegChoice {
    LegChoice {
        commodity: CommodityId(commodity),
        buy_price: Credits(1),
        sell_price: Credits(2),
        units: Tons(profit),
        profit: Credits(profit),
        limiter: Limiter::Cargo,
        demand_assumed: false,
    }
}
