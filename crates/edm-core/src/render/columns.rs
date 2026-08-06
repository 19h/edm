//! The eight column sets, transcribed from `game-internal-api.ts`.
//!
//! Field order inside each column matters twice over: it is the left-to-right
//! order on screen, and — because ties in the drop rule go to the leftmost
//! column [R27] — it decides which of two equally expendable columns survives a
//! narrow terminal. Do not reorder them to look tidier.
//!
//! The `key` of each column is inert here; rows are matched to columns
//! positionally. It is kept so that the row-building code has one place to
//! name a cell after.

use super::table::Column;

/// `FIELD_COLUMNS` (`game-internal-api.ts:497`) — the two-column label/value table
/// used for request and response summaries. Nothing is droppable, so this table
/// only ever squeezes.
pub const FIELD_COLUMNS: &[Column] = &[
    Column::new("field", "Field").min_width(8).max_width(22),
    Column::new("value", "Value").min_width(12),
];

/// The same two-column shape as [`FIELD_COLUMNS`], with a wider label.
///
/// A separate constant rather than a widened `FIELD_COLUMNS` because that one
/// is pinned to `game-internal-api.ts:497` and every request and response table in
/// the program is drawn with it. The route plan's labels are indented exclusion
/// lines — `  - Odyssey settlements` is 23 characters — and eliding the very
/// words that explain what was filtered out would defeat the table.
pub const ROUTE_FIELD_COLUMNS: &[Column] = &[
    Column::new("field", "Field").min_width(8).max_width(30),
    Column::new("value", "Value").min_width(12),
];

/// A ranked route per row. Nothing here is ported; `edm route` is a new
/// command \[C25\], and these widths were chosen for its own content.
///
/// `Route` and `Cargo` are the two squeezable columns, which is the most any
/// set here may declare (see `fitting_terminates_within_two_steps_per_column`).
/// They are the two a reader cannot act without: **where to go, and what to
/// carry.** The first live radius-100 run printed neither legibly — the stops
/// cell was elided mid-system-name and the commodity appeared only under
/// `--detail` — which made twenty proved-optimal routes unusable.
///
/// **`Rate` and `Claim` both have priority zero, so neither is ever dropped.**
/// That is the same rule `Route::rate` enforces in Rust — the number is only
/// reachable together with the guarantee that qualifies it — carried into the
/// table, where it is otherwise very easy to lose. The first live run of this
/// command dropped `Claim` to fit an ordinary 100-column terminal and printed
/// twenty rates with no statement of what was proved about any of them, which
/// is the failure this arrangement exists to prevent. `Cr/h lap 1` goes first
/// because it is the same quantity measured differently.
pub const ROUTE_COLUMNS: &[Column] = &[
    Column::new("rank", "#").right(),
    Column::new("route", "Route").min_width(24),
    Column::new("cargo", "Cargo").min_width(14),
    // Total distance flown per lap. A round trip's is the out-and-back, which
    // is what you actually fly; `--detail` and `--json` carry it per leg.
    Column::new("distance", "Ly").right().priority(1),
    Column::new("profit", "Profit").right().priority(3),
    Column::new("rate", "Cr/h").right(),
    // No `Cr/h lap 1` column. It is the same quantity measured over the
    // approach as well, useful and secondary, and at any real terminal width it
    // was thirteen characters taken from `Cargo` — which is the thing a reader
    // cannot act without. It is in `--json`.
    Column::new("time", "Lap").right().priority(2),
    Column::new("claim", "Claim"),
];

/// One leg per row, for `--detail`.
///
/// There is no `From` column. The legs are in flying order and the block names
/// the origin, so a `From` would repeat every station twice — and, more to the
/// point, a third squeezable column would cost a squeeze round after every drop
/// (see `fitting_terminates_within_two_steps_per_column`, whose bound holds
/// because no set here declares more than two floors).
pub const LEG_COLUMNS: &[Column] = &[
    Column::new("to", "To").min_width(12),
    Column::new("commodity", "Commodity").min_width(10),
    Column::new("units", "Tons").right(),
    Column::new("buy", "Buy").right().priority(3),
    Column::new("sell", "Sell").right().priority(3),
    Column::new("profit", "Profit").right(),
    Column::new("limiter", "Limited by").priority(2),
    Column::new("distance", "Distance").right().priority(5),
    Column::new("time", "Time").right().priority(4),
];

/// `COMMODITY_COLUMNS` (`game-internal-api.ts:621`) — the market listing.
///
/// Only `Commodity` declares a floor, so it is the only column that is ever
/// squeezed; the other ten are dropped whole, heaviest priority first.
pub const COMMODITY_COLUMNS: &[Column] = &[
    Column::new("id", "ID").right().priority(4),
    Column::new("name", "Commodity").min_width(12).max_width(30),
    Column::new("stock", "Stock").right().priority(1),
    Column::new("stockMeter", "Stk").priority(2),
    Column::new("buyPrice", "Buy").right(),
    Column::new("sellPrice", "Sell").right(),
    Column::new("fencePrice", "Fence").right().priority(4),
    Column::new("demand", "Demand").right().priority(1),
    Column::new("demandMeter", "Dmd").priority(2),
    Column::new("meanPrice", "Mean").right().priority(3),
    Column::new("flags", "CPRI").priority(1),
];

/// `INVENTORY_COLUMNS` (`game-internal-api.ts:681`) — the commander's hold.
pub const INVENTORY_COLUMNS: &[Column] = &[
    Column::new("commodity", "Commodity").min_width(10).max_width(30),
    Column::new("qty", "Qty").right(),
    Column::new("value", "Value").right(),
    Column::new("stolen", "S"),
    Column::new("marked", "Marked").right().priority(3),
    Column::new("owner", "Owner").right().priority(2),
    Column::new("origin", "Origin").right().priority(2),
    Column::new("position", "Position (x / y / z)").priority(1),
];

/// `SWEEP_COLUMNS` (`game-internal-api.ts:1358`) — one row per market visited.
pub const SWEEP_COLUMNS: &[Column] = &[
    Column::new("marketId", "Market ID").right(),
    Column::new("name", "Name").min_width(12).max_width(32),
    Column::new("status", "HTTP").right(),
    Column::new("commodities", "Comm").right(),
    Column::new("supplied", "Sup").right().priority(2),
    Column::new("demanded", "Dem").right().priority(2),
    Column::new("eddn", "EDDN").priority(1),
    Column::new("attempts", "Try").right().priority(3),
];

/// `PLAN_COLUMNS` (`game-internal-api.ts:1731`) — a resolved trade, with the
/// provenance of each field.
pub const PLAN_COLUMNS: &[Column] = &[
    Column::new("field", "Field").min_width(8).max_width(20),
    Column::new("value", "Value").min_width(10),
    Column::new("source", "From").priority(1),
];

/// `TRADE_LOG_COLUMNS` (`game-internal-api.ts:2024`) — one row per executed trade.
pub const TRADE_LOG_COLUMNS: &[Column] = &[
    Column::new("round", "#").right(),
    Column::new("commodity", "Commodity").min_width(10).max_width(28),
    Column::new("qty", "Qty").right(),
    Column::new("unitPrice", "Unit").right(),
    Column::new("total", "Total").right(),
    Column::new("status", "HTTP").right().priority(1),
    Column::new("cargo", "Cargo").right().priority(2),
];

/// `POI_COLUMNS` (`game-internal-api.ts:2613`) — the structural scan's fallback
/// listing, used when `starsystem.polities` yields no markets.
pub const POI_COLUMNS: &[Column] = &[
    Column::new("marketId", "Market ID").right(),
    Column::new("name", "Name").min_width(12).max_width(34),
    Column::new("type", "Type").max_width(18).priority(1),
    Column::new("economy", "Economy").max_width(16).priority(2),
    Column::new("faction", "Faction").max_width(24).priority(3),
    Column::new("path", "Found at").max_width(28).priority(4),
];

/// `MARKET_POINT_COLUMNS` (`game-internal-api.ts:2758`) — the markets of a system.
pub const MARKET_POINT_COLUMNS: &[Column] = &[
    Column::new("marketId", "Market ID").right(),
    Column::new("name", "Name").min_width(14).max_width(36),
    Column::new("services", "CBOYF").priority(1),
    Column::new("imports", "Imp").right().priority(1),
    Column::new("exports", "Exp").right().priority(1),
    Column::new("distance", "Dist (Ls)").right().priority(2),
    Column::new("economy", "Economy").max_width(14).priority(3),
    Column::new("faction", "Faction").max_width(26).priority(4),
    Column::new("body", "Body").max_width(22).priority(5),
];

/// Every column set, by the TypeScript's own name for it.
///
/// Exists so that a test or a fixture can address a column set by name without
/// a second, drifting copy of the list.
pub const ALL: &[(&str, &[Column])] = &[
    ("FIELD", FIELD_COLUMNS),
    ("ROUTE_FIELD", ROUTE_FIELD_COLUMNS),
    ("ROUTE", ROUTE_COLUMNS),
    ("LEG", LEG_COLUMNS),
    ("COMMODITY", COMMODITY_COLUMNS),
    ("INVENTORY", INVENTORY_COLUMNS),
    ("SWEEP", SWEEP_COLUMNS),
    ("PLAN", PLAN_COLUMNS),
    ("TRADE_LOG", TRADE_LOG_COLUMNS),
    ("POI", POI_COLUMNS),
    ("MARKET_POINT", MARKET_POINT_COLUMNS),
];

/// Looks a column set up by the name it has in `game-internal-api.ts`.
#[must_use]
pub fn by_name(name: &str) -> Option<&'static [Column]> {
    ALL.iter().find(|(candidate, _)| *candidate == name).map(|(_, columns)| *columns)
}
