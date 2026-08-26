//! Tolerant, I/O-free reconstruction of the local Elite Dangerous commander state.
//!
//! Journal files and the four live sidecars are deliberately treated as
//! observations, not as a database.  Every externally supplied value that can
//! be used as a route default records where and when it was seen.  In
//! particular, Frontier ids are read directly from `serde_json::Number` as
//! `u64`; they never pass through `f64` and therefore retain values above
//! JavaScript's 2^53 precision boundary.
//!
//! This model intentionally does not retain `Commander`, `FID`, `ShipName` or
//! `ShipIdent`.  Those fields are not needed to plan a route and can contain
//! identifying or user-entered text.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The file or stream from which an observation came.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    Journal,
    StatusSidecar,
    CargoSidecar,
    NavRouteSidecar,
    MarketSidecar,
}

impl ObservationSource {
    const fn is_sidecar(self) -> bool {
        !matches!(self, Self::Journal)
    }
}

/// A value together with its provenance.
///
/// `ordinal` is local to one [`CommanderState`].  It makes observations with no
/// timestamp deterministic without pretending that they have a wall-clock
/// time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observed<T> {
    pub source: ObservationSource,
    pub timestamp: Option<String>,
    pub ordinal: u64,
    pub value: T,
}

/// How the commander arrived in the current system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemArrival {
    Location,
    FsdJump,
    CarrierJump,
}

/// Current star-system coordinates and address.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemLocation {
    pub name: String,
    pub address: Option<u64>,
    pub coordinates: Option<[f64; 3]>,
    pub arrival: SystemArrival,
}

/// Current station/docking information.
///
/// An undocking is retained as `docked == false` rather than represented by a
/// missing value.  This is a timestamped tombstone: an old `Market.json` must
/// not make the commander appear docked again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockingState {
    pub docked: bool,
    pub station_name: Option<String>,
    pub station_type: Option<String>,
    pub market_id: Option<u64>,
    pub services: Vec<String>,
    /// The timestamp of the actual station arrival, when known.  A later
    /// `Status.json` refresh does not change it.
    pub arrived_at: Option<String>,
}

/// One stack in the ship cargo hold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoItem {
    pub name: String,
    pub name_localised: Option<String>,
    pub count: u64,
    pub stolen: u64,
    pub mission_id: Option<u64>,
}

/// Cargo observations.  Capacity and contents have separate clocks because
/// they normally come from different events (`Loadout` and `Cargo`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CargoState {
    pub capacity: Option<Observed<u64>>,
    pub inventory: Option<Observed<Vec<CargoItem>>>,
    pub used: Option<Observed<u64>>,
    /// Derived from the latest capacity and used values.  `None` means the
    /// capacity is not known; overfull/corrupt input clamps this to zero.
    pub free: Option<u64>,
}

impl CargoState {
    #[must_use]
    pub fn capacity_value(&self) -> Option<u64> {
        self.capacity.as_ref().map(|value| value.value)
    }

    #[must_use]
    pub fn used_value(&self) -> Option<u64> {
        self.used.as_ref().map(|value| value.value)
    }
}

/// Current ship facts useful to route planning.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShipState {
    pub ship_type: Option<String>,
    pub ship_id: Option<u64>,
    pub max_jump_range: Option<f64>,
}

/// One waypoint in `NavRoute.json` or a `NavRoute` journal event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteHop {
    pub star_system: String,
    pub system_address: Option<u64>,
    pub star_position: Option<[f64; 3]>,
    pub star_class: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NavRoute {
    pub hops: Vec<RouteHop>,
}

/// The small, stable part of a `Market.json` item.  Unknown additions remain
/// harmless and missing numeric fields remain `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketCommodity {
    pub id: Option<u64>,
    pub name: String,
    pub name_localised: Option<String>,
    pub buy_price: Option<u64>,
    pub sell_price: Option<u64>,
    pub mean_price: Option<u64>,
    pub stock: Option<u64>,
    pub demand: Option<u64>,
}

/// Latest market snapshot.  `accessible == false` means it is retained only as
/// a diagnostic/last-seen snapshot and must not be used as a current market.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketState {
    pub market_id: Option<u64>,
    pub station_name: Option<String>,
    pub star_system: Option<String>,
    pub accessible: bool,
    pub items: Vec<MarketCommodity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeKind {
    Buy,
    Sell,
}

/// A trade which the game reported as executed (not a proposed order).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeExecution {
    pub kind: TradeKind,
    pub market_id: Option<u64>,
    pub commodity: String,
    pub commodity_localised: Option<String>,
    pub count: u64,
    pub unit_price: Option<u64>,
    pub total: Option<u64>,
    pub black_market: bool,
    pub stolen_goods: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningCode {
    MalformedJson,
    MissingSession,
    InvalidField,
    CargoCountMismatch,
    ArithmeticOverflow,
    ArithmeticUnderflow,
    CargoOverCapacity,
    StaleSidecarIgnored,
}

/// A non-fatal problem encountered during replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommanderWarning {
    pub code: WarningCode,
    pub message: String,
    pub line: Option<usize>,
    pub source: Option<ObservationSource>,
}

/// What this commander's own ship learned about one fleet carrier's door.
///
/// This is the only docking-access evidence in the program that is about *this*
/// commander rather than about the galaxy. A crowd-sourced index can say a
/// carrier admits "All" and be a day out of date, or say "Squadron" without
/// knowing which squadron the reader is in. A `DockingDenied` addressed to this
/// ship, or a `Docked` this ship completed, is neither guess nor average: it is
/// the door answering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CarrierDoor {
    /// A `Docked` at this carrier succeeded — whatever its published policy, it
    /// admits this commander. This is what rescues a squadron carrier the
    /// commander is actually in the squadron of.
    Admitted,
    /// `DockingDenied` with `Reason: "RestrictedAccess"`. Only that reason:
    /// `NoSpace` is a full pad, `Distance` is a bad approach and `TooLarge` is a
    /// fact about the ship, and none of the three is a statement about who the
    /// door opens for.
    Refused,
}

/// One observation of a carrier's door, and when it was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoorObservation {
    pub door: CarrierDoor,
    /// The journal timestamp, kept as written so the newest observation wins
    /// without this module needing a clock or a date parser — ISO-8601 UTC
    /// sorts correctly as text, which is the whole reason Frontier writes it
    /// that way.
    pub observed_at: Option<String>,
}

/// State reconstructed from one selected play session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommanderState {
    pub current_system: Option<Observed<SystemLocation>>,
    pub docking: Option<Observed<DockingState>>,
    pub cargo: CargoState,
    pub credits: Option<Observed<u64>>,
    pub ship: Option<Observed<ShipState>>,
    pub nav_route: Option<Observed<NavRoute>>,
    pub market: Option<Observed<MarketState>>,
    pub executed_trades: Vec<Observed<TradeExecution>>,
    pub warnings: Vec<CommanderWarning>,
    /// Every fleet carrier this commander has been admitted to or refused by,
    /// sorted by market id.
    ///
    /// A sorted `Vec` rather than a map: `BTreeMap` is banned here because
    /// lexicographic key order is never what this crate wants \[F1/R5\], a
    /// `HashMap` would iterate nondeterministically, and this collection is
    /// small, read far more often than written, and wants a stable order for
    /// exactly the reasons the ban exists.
    ///
    /// **Deliberately not cleared by `LoadGame`.** Everything else here is a
    /// session value — where the ship is, what is in the hold — and stale
    /// session state would produce a confidently wrong route. A door is not a
    /// session value: a carrier that refused this commander last Tuesday is
    /// still refusing them today unless its owner changed something, and
    /// forgetting that on every game start would throw away the only
    /// commander-specific evidence the program has.
    pub carrier_doors: Vec<(u64, DoorObservation)>,
    #[serde(skip)]
    next_ordinal: u64,
}

impl CommanderState {
    /// Apply one journal JSON line.  Bad JSON is a warning and returns `false`.
    pub fn apply_journal_json(&mut self, line: &str, line_number: usize) -> bool {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.warn(
                WarningCode::MalformedJson,
                "malformed or truncated JSON line ignored",
                Some(line_number),
                Some(ObservationSource::Journal),
            );
            return false;
        };
        self.apply_event(&value, ObservationSource::Journal, Some(line_number));
        true
    }

    /// Merge a live sidecar chosen by its source.
    ///
    /// Passing `Journal` is rejected because journal records must go through
    /// [`Self::apply_journal_json`] and participate in session reset semantics.
    pub fn merge_sidecar(&mut self, source: ObservationSource, json: &str) -> bool {
        if !source.is_sidecar() {
            self.warn(
                WarningCode::InvalidField,
                "journal input is not a sidecar",
                None,
                Some(source),
            );
            return false;
        }
        let Ok(value) = serde_json::from_str::<Value>(json) else {
            self.warn(
                WarningCode::MalformedJson,
                "malformed or truncated sidecar JSON ignored",
                None,
                Some(source),
            );
            return false;
        };
        self.apply_event(&value, source, None);
        true
    }

    pub fn merge_status(&mut self, json: &str) -> bool {
        self.merge_sidecar(ObservationSource::StatusSidecar, json)
    }

    pub fn merge_cargo(&mut self, json: &str) -> bool {
        self.merge_sidecar(ObservationSource::CargoSidecar, json)
    }

    pub fn merge_nav_route(&mut self, json: &str) -> bool {
        self.merge_sidecar(ObservationSource::NavRouteSidecar, json)
    }

    pub fn merge_market(&mut self, json: &str) -> bool {
        self.merge_sidecar(ObservationSource::MarketSidecar, json)
    }

    /// Convenience aliases named after Frontier's sidecar files.
    pub fn merge_status_sidecar(&mut self, json: &str) -> bool {
        self.merge_status(json)
    }

    pub fn merge_cargo_sidecar(&mut self, json: &str) -> bool {
        self.merge_cargo(json)
    }

    pub fn merge_nav_route_sidecar(&mut self, json: &str) -> bool {
        self.merge_nav_route(json)
    }

    pub fn merge_market_sidecar(&mut self, json: &str) -> bool {
        self.merge_market(json)
    }

    #[must_use]
    pub fn current_market_id(&self) -> Option<u64> {
        self.docking
            .as_ref()
            .filter(|docking| docking.value.docked)
            .and_then(|docking| docking.value.market_id)
    }

    #[must_use]
    pub fn is_docked(&self) -> bool {
        self.docking
            .as_ref()
            .is_some_and(|docking| docking.value.docked)
    }

    fn observe<T>(
        &mut self,
        source: ObservationSource,
        timestamp: Option<&str>,
        value: T,
    ) -> Observed<T> {
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        Observed {
            source,
            timestamp: timestamp.map(ToOwned::to_owned),
            ordinal: self.next_ordinal,
            value,
        }
    }

    fn warn(
        &mut self,
        code: WarningCode,
        message: impl Into<String>,
        line: Option<usize>,
        source: Option<ObservationSource>,
    ) {
        self.warnings.push(CommanderWarning {
            code,
            message: message.into(),
            line,
            source,
        });
    }

    /// A LoadGame starts a new commander session.  Diagnostics and the local
    /// ordinal survive, but every route-affecting value and executed trade is
    /// discarded.  Identity fields in the event are intentionally never read.
    fn reset_for_load_game(&mut self) {
        let warnings = std::mem::take(&mut self.warnings);
        let doors = std::mem::take(&mut self.carrier_doors);
        let next_ordinal = self.next_ordinal;
        *self = Self::default();
        self.warnings = warnings;
        // See the field: a door is not a session value.
        self.carrier_doors = doors;
        self.next_ordinal = next_ordinal;
    }

    fn apply_event(&mut self, value: &Value, source: ObservationSource, line: Option<usize>) {
        let Some(object) = value.as_object() else {
            self.warn(
                WarningCode::InvalidField,
                "JSON record is not an object",
                line,
                Some(source),
            );
            return;
        };
        let timestamp = string_field(object, "timestamp");
        let event = string_field(object, "event").unwrap_or(match source {
            ObservationSource::StatusSidecar => "Status",
            ObservationSource::CargoSidecar => "Cargo",
            ObservationSource::NavRouteSidecar => "NavRoute",
            ObservationSource::MarketSidecar => "Market",
            ObservationSource::Journal => "",
        });

        match event {
            "LoadGame" if source == ObservationSource::Journal => {
                self.reset_for_load_game();
                self.apply_load_game(object, timestamp, line);
            }
            "Location" | "FSDJump" | "CarrierJump" => {
                self.apply_location(object, event, source, timestamp, line);
            }
            "Docked" => {
                self.note_carrier_door(object, CarrierDoor::Admitted, timestamp);
                self.apply_docked(object, source, timestamp, line);
            }
            "DockingDenied" if source == ObservationSource::Journal => {
                if string_field(object, "Reason") == Some("RestrictedAccess") {
                    self.note_carrier_door(object, CarrierDoor::Refused, timestamp);
                }
            }
            "Undocked" => self.apply_undocked(source, timestamp, line),
            "Cargo" => self.apply_cargo_event(object, source, timestamp, line),
            "Loadout" => self.apply_loadout(object, source, timestamp, line),
            "NavRoute" => self.apply_nav_route_event(object, source, timestamp, line),
            "NavRouteClear" => self.apply_nav_route_clear(source, timestamp, line),
            "Market" => self.apply_market_event(object, source, timestamp, line),
            "MarketBuy" if source == ObservationSource::Journal => {
                self.apply_trade(object, TradeKind::Buy, timestamp, line);
            }
            "MarketSell" if source == ObservationSource::Journal => {
                self.apply_trade(object, TradeKind::Sell, timestamp, line);
            }
            "Status" if source == ObservationSource::StatusSidecar => {
                self.apply_status(object, timestamp, line);
            }
            _ => {}
        }
    }

    fn apply_load_game(
        &mut self,
        object: &Map<String, Value>,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        if let Some(credits) = read_u64_field(object, "Credits") {
            let observed = self.observe(ObservationSource::Journal, timestamp, credits);
            self.credits = Some(observed);
        } else if object.contains_key("Credits") {
            self.warn(
                WarningCode::InvalidField,
                "LoadGame Credits is not a non-negative integer",
                line,
                Some(ObservationSource::Journal),
            );
        }

        let ship_type = owned_nonempty(object, "Ship");
        let ship_id = read_u64_field(object, "ShipID");
        if ship_type.is_some() || ship_id.is_some() {
            let ship = ShipState {
                ship_type,
                ship_id,
                max_jump_range: None,
            };
            let observed = self.observe(ObservationSource::Journal, timestamp, ship);
            self.ship = Some(observed);
        }
    }

    fn apply_location(
        &mut self,
        object: &Map<String, Value>,
        event: &str,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let Some(name) = owned_nonempty(object, "StarSystem") else {
            self.warn(
                WarningCode::InvalidField,
                "location has no StarSystem",
                line,
                Some(source),
            );
            return;
        };
        let arrival = match event {
            "FSDJump" => SystemArrival::FsdJump,
            "CarrierJump" => SystemArrival::CarrierJump,
            _ => SystemArrival::Location,
        };
        let location = SystemLocation {
            name,
            address: read_u64_field(object, "SystemAddress"),
            coordinates: coordinates_field(object, "StarPos"),
            arrival,
        };
        let incoming = self.observe(source, timestamp, location);
        if source == ObservationSource::Journal {
            self.current_system = Some(incoming);
        } else if !replace_if_fresh(&mut self.current_system, incoming) {
            self.stale(source, "system location", line);
            return;
        }

        let docked = bool_field(object, "Docked").unwrap_or(false);
        if docked {
            self.set_docked_from_object(object, source, timestamp, line);
        } else {
            self.set_undocked(source, timestamp, line);
        }
    }

    /// Record what a carrier's door just did, newest observation winning.
    ///
    /// Only fleet carriers: an ordinary starport's docking permission is a
    /// function of allegiance and bounties rather than an owner's setting, and
    /// nothing downstream would use it.
    fn note_carrier_door(
        &mut self,
        object: &Map<String, Value>,
        door: CarrierDoor,
        timestamp: Option<&str>,
    ) {
        if string_field(object, "StationType") != Some("FleetCarrier") {
            return;
        }
        let Some(market_id) = read_u64_field(object, "MarketID") else {
            return;
        };
        let observation = DoorObservation {
            door,
            observed_at: timestamp.map(ToOwned::to_owned),
        };
        match self
            .carrier_doors
            .binary_search_by_key(&market_id, |(id, _)| *id)
        {
            // Timestamps are ISO-8601 UTC, so text order is time order. An
            // observation with no timestamp never displaces one that has a
            // timestamp, and never loses to one that does not.
            Ok(at) => {
                if self.carrier_doors[at].1.observed_at <= observation.observed_at {
                    self.carrier_doors[at].1 = observation;
                }
            }
            Err(at) => self.carrier_doors.insert(at, (market_id, observation)),
        }
    }

    fn apply_docked(
        &mut self,
        object: &Map<String, Value>,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        self.set_docked_from_object(object, source, timestamp, line);
    }

    fn set_docked_from_object(
        &mut self,
        object: &Map<String, Value>,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let docking = DockingState {
            docked: true,
            station_name: owned_nonempty(object, "StationName"),
            station_type: owned_nonempty(object, "StationType"),
            market_id: read_u64_field(object, "MarketID"),
            services: string_array_field(object, "StationServices"),
            arrived_at: timestamp.map(ToOwned::to_owned),
        };
        let incoming = self.observe(source, timestamp, docking.clone());
        let accepted = if source == ObservationSource::Journal {
            self.docking = Some(incoming);
            true
        } else {
            replace_if_fresh(&mut self.docking, incoming)
        };
        if !accepted {
            self.stale(source, "docking state", line);
            return;
        }

        let market = MarketState {
            market_id: docking.market_id,
            station_name: docking.station_name,
            star_system: owned_nonempty(object, "StarSystem").or_else(|| {
                self.current_system
                    .as_ref()
                    .map(|system| system.value.name.clone())
            }),
            accessible: true,
            items: Vec::new(),
        };
        let incoming = self.observe(source, timestamp, market);
        if source == ObservationSource::Journal {
            self.market = Some(incoming);
        } else if !replace_if_fresh(&mut self.market, incoming) {
            self.stale(source, "market access", line);
        }
    }

    fn apply_undocked(
        &mut self,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        self.set_undocked(source, timestamp, line);
    }

    fn set_undocked(
        &mut self,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let incoming = self.observe(
            source,
            timestamp,
            DockingState {
                docked: false,
                ..DockingState::default()
            },
        );
        let accepted = if source == ObservationSource::Journal {
            self.docking = Some(incoming);
            true
        } else {
            replace_if_fresh(&mut self.docking, incoming)
        };
        if !accepted {
            self.stale(source, "docking state", line);
            return;
        }

        let mut market = self
            .market
            .as_ref()
            .map_or_else(MarketState::default, |old| old.value.clone());
        market.accessible = false;
        let incoming = self.observe(source, timestamp, market);
        if source == ObservationSource::Journal {
            self.market = Some(incoming);
        } else if !replace_if_fresh(&mut self.market, incoming) {
            self.stale(source, "market access", line);
        }
    }

    fn apply_loadout(
        &mut self,
        object: &Map<String, Value>,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let ship = ShipState {
            ship_type: owned_nonempty(object, "Ship"),
            ship_id: read_u64_field(object, "ShipID"),
            max_jump_range: finite_nonnegative_field(object, "MaxJumpRange"),
        };
        let incoming = self.observe(source, timestamp, ship);
        if source == ObservationSource::Journal {
            self.ship = Some(incoming);
        } else if !replace_if_fresh(&mut self.ship, incoming) {
            self.stale(source, "ship loadout", line);
        }

        if let Some(capacity) = read_u64_field(object, "CargoCapacity") {
            let incoming = self.observe(source, timestamp, capacity);
            let accepted = if source == ObservationSource::Journal {
                self.cargo.capacity = Some(incoming);
                true
            } else {
                replace_if_fresh(&mut self.cargo.capacity, incoming)
            };
            if !accepted {
                self.stale(source, "cargo capacity", line);
            }
            self.recompute_cargo_free(line, source);
        } else if object.contains_key("CargoCapacity") {
            self.warn(
                WarningCode::InvalidField,
                "Loadout CargoCapacity is not a non-negative integer",
                line,
                Some(source),
            );
        }
    }

    fn apply_cargo_event(
        &mut self,
        object: &Map<String, Value>,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        if string_field(object, "Vessel").is_some_and(|vessel| !vessel.eq_ignore_ascii_case("ship"))
        {
            return;
        }

        let reported = read_u64_field(object, "Count");
        let parsed_inventory = object
            .get("Inventory")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| parse_cargo_item(entry, &mut self.warnings, line, source))
                    .collect::<Vec<_>>()
            });

        let inventory_sum = parsed_inventory
            .as_ref()
            .map(|inventory| self.sum_inventory(inventory, line, source));
        if let (Some(reported), Some(sum)) = (reported, inventory_sum)
            && reported != sum
        {
            self.warn(
                WarningCode::CargoCountMismatch,
                format!(
                    "Cargo Count ({reported}) differs from inventory sum ({sum}); inventory wins"
                ),
                line,
                Some(source),
            );
        }

        if let Some(inventory) = parsed_inventory {
            let incoming = self.observe(source, timestamp, inventory);
            let accepted = if source == ObservationSource::Journal {
                self.cargo.inventory = Some(incoming);
                true
            } else {
                replace_if_fresh(&mut self.cargo.inventory, incoming)
            };
            if !accepted {
                self.stale(source, "cargo inventory", line);
            }
        }

        if let Some(used) = inventory_sum.or(reported) {
            let incoming = self.observe(source, timestamp, used);
            let accepted = if source == ObservationSource::Journal {
                self.cargo.used = Some(incoming);
                true
            } else {
                replace_if_fresh(&mut self.cargo.used, incoming)
            };
            if !accepted {
                self.stale(source, "cargo used", line);
            }
        }
        self.recompute_cargo_free(line, source);
    }

    fn sum_inventory(
        &mut self,
        inventory: &[CargoItem],
        line: Option<usize>,
        source: ObservationSource,
    ) -> u64 {
        let mut used = 0_u64;
        for item in inventory {
            let Some(next) = used.checked_add(item.count) else {
                self.warn(
                    WarningCode::ArithmeticOverflow,
                    "cargo inventory sum overflowed; used cargo saturated",
                    line,
                    Some(source),
                );
                return u64::MAX;
            };
            used = next;
        }
        used
    }

    fn recompute_cargo_free(&mut self, line: Option<usize>, source: ObservationSource) {
        self.cargo.free = match (self.cargo.capacity_value(), self.cargo.used_value()) {
            (Some(capacity), Some(used)) if used > capacity => {
                self.warn(
                    WarningCode::CargoOverCapacity,
                    format!("cargo used ({used}) exceeds capacity ({capacity}); free cargo clamped to zero"),
                    line,
                    Some(source),
                );
                Some(0)
            }
            (Some(capacity), Some(used)) => Some(capacity - used),
            (Some(capacity), None) => Some(capacity),
            (None, _) => None,
        };
    }

    fn apply_nav_route_event(
        &mut self,
        object: &Map<String, Value>,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let hops = object
            .get("Route")
            .and_then(Value::as_array)
            .map(|route| route.iter().filter_map(parse_route_hop).collect())
            .unwrap_or_default();
        let incoming = self.observe(source, timestamp, NavRoute { hops });
        if source == ObservationSource::Journal {
            self.nav_route = Some(incoming);
        } else if !replace_if_fresh(&mut self.nav_route, incoming) {
            self.stale(source, "navigation route", line);
        }
    }

    fn apply_nav_route_clear(
        &mut self,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let incoming = self.observe(source, timestamp, NavRoute::default());
        if source == ObservationSource::Journal {
            self.nav_route = Some(incoming);
        } else if !replace_if_fresh(&mut self.nav_route, incoming) {
            self.stale(source, "navigation route", line);
        }
    }

    fn apply_market_event(
        &mut self,
        object: &Map<String, Value>,
        source: ObservationSource,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let market_id = read_u64_field(object, "MarketID");
        let station_name = owned_nonempty(object, "StationName");
        let star_system = owned_nonempty(object, "StarSystem");
        let docking_allows_access = self.docking.as_ref().is_some_and(|docking| {
            docking.value.docked
                && market_id.is_none_or(|id| {
                    docking
                        .value
                        .market_id
                        .is_none_or(|docked_id| id == docked_id)
                })
        });
        let items = object
            .get("Items")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_market_item).collect())
            .unwrap_or_default();
        let market = MarketState {
            market_id,
            station_name: station_name.clone(),
            star_system,
            accessible: docking_allows_access,
            items,
        };
        let incoming = self.observe(source, timestamp, market);
        let accepted = if source == ObservationSource::Journal {
            self.market = Some(incoming);
            true
        } else {
            replace_if_fresh(&mut self.market, incoming)
        };
        if !accepted {
            self.stale(source, "market snapshot", line);
            return;
        }

        // Market.json can fill fields absent from a Docked event, but it never
        // creates a docked state by itself.
        if docking_allows_access {
            let mut docking = self
                .docking
                .as_ref()
                .map(|observed| observed.value.clone())
                .unwrap_or_default();
            docking.market_id = market_id.or(docking.market_id);
            docking.station_name = station_name.or(docking.station_name);
            let incoming = self.observe(source, timestamp, docking);
            if source == ObservationSource::Journal {
                self.docking = Some(incoming);
            } else if !replace_if_fresh(&mut self.docking, incoming) {
                self.stale(source, "docking market details", line);
            }
        }
    }

    fn apply_trade(
        &mut self,
        object: &Map<String, Value>,
        kind: TradeKind,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let count = read_u64_field(object, "Count").unwrap_or(0);
        let (unit_key, total_key) = match kind {
            TradeKind::Buy => ("BuyPrice", "TotalCost"),
            TradeKind::Sell => ("SellPrice", "TotalSale"),
        };
        let trade = TradeExecution {
            kind,
            market_id: read_u64_field(object, "MarketID"),
            commodity: owned_nonempty(object, "Type").unwrap_or_default(),
            commodity_localised: owned_nonempty(object, "Type_Localised"),
            count,
            unit_price: read_u64_field(object, unit_key),
            total: read_u64_field(object, total_key),
            black_market: bool_field(object, "BlackMarket").unwrap_or(false),
            stolen_goods: bool_field(object, "StolenGoods").unwrap_or(false),
        };
        let observed_trade = self.observe(ObservationSource::Journal, timestamp, trade.clone());
        self.executed_trades.push(observed_trade);
        self.apply_trade_to_credits(&trade, timestamp, line);
        self.apply_trade_to_cargo(&trade, timestamp, line);
    }

    fn apply_trade_to_credits(
        &mut self,
        trade: &TradeExecution,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let (Some(balance), Some(total)) = (self.credits.as_ref().map(|v| v.value), trade.total)
        else {
            return;
        };
        let next = match trade.kind {
            TradeKind::Buy => balance.checked_sub(total).unwrap_or_else(|| {
                self.warn(
                    WarningCode::ArithmeticUnderflow,
                    "MarketBuy cost exceeds known credits; credits clamped to zero",
                    line,
                    Some(ObservationSource::Journal),
                );
                0
            }),
            TradeKind::Sell => balance.checked_add(total).unwrap_or_else(|| {
                self.warn(
                    WarningCode::ArithmeticOverflow,
                    "MarketSell proceeds overflow credits; credits saturated",
                    line,
                    Some(ObservationSource::Journal),
                );
                u64::MAX
            }),
        };
        let incoming = self.observe(ObservationSource::Journal, timestamp, next);
        self.credits = Some(incoming);
    }

    fn apply_trade_to_cargo(
        &mut self,
        trade: &TradeExecution,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        if trade.count == 0 {
            return;
        }

        if let Some(old_inventory) = self.cargo.inventory.as_ref() {
            let mut inventory = old_inventory.value.clone();
            let matching = inventory
                .iter()
                .position(|item| item.name.eq_ignore_ascii_case(&trade.commodity));
            match (trade.kind, matching) {
                (TradeKind::Buy, Some(index)) => {
                    inventory[index].count = inventory[index]
                        .count
                        .checked_add(trade.count)
                        .unwrap_or_else(|| {
                            self.warn(
                                WarningCode::ArithmeticOverflow,
                                "MarketBuy overflowed a cargo stack; count saturated",
                                line,
                                Some(ObservationSource::Journal),
                            );
                            u64::MAX
                        });
                }
                (TradeKind::Buy, None) => inventory.push(CargoItem {
                    name: trade.commodity.clone(),
                    name_localised: trade.commodity_localised.clone(),
                    count: trade.count,
                    stolen: 0,
                    mission_id: None,
                }),
                (TradeKind::Sell, Some(index)) => {
                    inventory[index].count = inventory[index]
                        .count
                        .checked_sub(trade.count)
                        .unwrap_or_else(|| {
                            self.warn(
                                WarningCode::ArithmeticUnderflow,
                                "MarketSell exceeds known commodity stack; count clamped to zero",
                                line,
                                Some(ObservationSource::Journal),
                            );
                            0
                        });
                    if inventory[index].count == 0 {
                        inventory.remove(index);
                    }
                }
                (TradeKind::Sell, None) => self.warn(
                    WarningCode::ArithmeticUnderflow,
                    "MarketSell commodity is absent from known inventory",
                    line,
                    Some(ObservationSource::Journal),
                ),
            }
            let used = self.sum_inventory(&inventory, line, ObservationSource::Journal);
            self.cargo.inventory =
                Some(self.observe(ObservationSource::Journal, timestamp, inventory));
            self.cargo.used = Some(self.observe(ObservationSource::Journal, timestamp, used));
        } else if let Some(used) = self.cargo.used.as_ref().map(|observed| observed.value) {
            let next = match trade.kind {
                TradeKind::Buy => used.checked_add(trade.count).unwrap_or_else(|| {
                    self.warn(
                        WarningCode::ArithmeticOverflow,
                        "MarketBuy overflowed cargo used; count saturated",
                        line,
                        Some(ObservationSource::Journal),
                    );
                    u64::MAX
                }),
                TradeKind::Sell => used.checked_sub(trade.count).unwrap_or_else(|| {
                    self.warn(
                        WarningCode::ArithmeticUnderflow,
                        "MarketSell exceeds known cargo used; count clamped to zero",
                        line,
                        Some(ObservationSource::Journal),
                    );
                    0
                }),
            };
            self.cargo.used = Some(self.observe(ObservationSource::Journal, timestamp, next));
        }
        self.recompute_cargo_free(line, ObservationSource::Journal);
    }

    fn apply_status(
        &mut self,
        object: &Map<String, Value>,
        timestamp: Option<&str>,
        line: Option<usize>,
    ) {
        let source = ObservationSource::StatusSidecar;
        if let Some(used) = read_u64_field(object, "Cargo") {
            let incoming = self.observe(source, timestamp, used);
            if !replace_if_fresh(&mut self.cargo.used, incoming) {
                self.stale(source, "cargo used", line);
            }
            self.recompute_cargo_free(line, source);
        }

        if let Some(flags) = read_u64_field(object, "Flags") {
            let docked = flags & 1 != 0;
            let state = if docked {
                let mut known = self
                    .docking
                    .as_ref()
                    .map_or_else(DockingState::default, |old| old.value.clone());
                known.docked = true;
                known
            } else {
                DockingState {
                    docked: false,
                    ..DockingState::default()
                }
            };
            let incoming = self.observe(source, timestamp, state);
            if !replace_if_fresh(&mut self.docking, incoming) {
                self.stale(source, "docking status", line);
                return;
            }
            if !docked {
                let mut market = self
                    .market
                    .as_ref()
                    .map_or_else(MarketState::default, |old| old.value.clone());
                market.accessible = false;
                let incoming = self.observe(source, timestamp, market);
                if !replace_if_fresh(&mut self.market, incoming) {
                    self.stale(source, "market access", line);
                }
            }
        }
    }

    fn stale(&mut self, source: ObservationSource, field: &str, line: Option<usize>) {
        self.warn(
            WarningCode::StaleSidecarIgnored,
            format!("stale sidecar did not replace newer {field}"),
            line,
            Some(source),
        );
    }
}

/// Which LoadGame-delimited session to replay.  `Number` is zero based.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionSelection {
    #[default]
    Latest,
    Number(usize),
    /// Replay the entire input.  A later `LoadGame` still resets all state, so
    /// the resulting values describe the final session.
    All,
}

/// Replay newline-delimited journal JSON for a selected session.
///
/// Blank lines are ignored.  Malformed records, including a commonly observed
/// truncated final record while the game is still writing, become warnings.
#[must_use]
pub fn replay_json_lines(input: &str, selection: SessionSelection) -> CommanderState {
    let records: Vec<(usize, &str, Option<Value>)> = input
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = crate::js::text::js_trim(line);
            (!trimmed.is_empty()).then(|| {
                (
                    index + 1,
                    trimmed,
                    serde_json::from_str::<Value>(trimmed).ok(),
                )
            })
        })
        .collect();

    let load_games: Vec<usize> = records
        .iter()
        .enumerate()
        .filter_map(|(index, (_, _, value))| {
            value
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|object| string_field(object, "event"))
                .filter(|event| *event == "LoadGame")
                .map(|_| index)
        })
        .collect();

    let selected_range = match selection {
        SessionSelection::All => Some((0, records.len())),
        SessionSelection::Latest => load_games.last().map_or(Some((0, records.len())), |start| {
            Some((*start, records.len()))
        }),
        SessionSelection::Number(number) if load_games.is_empty() && number == 0 => {
            Some((0, records.len()))
        }
        SessionSelection::Number(number) => load_games.get(number).map(|start| {
            let end = load_games.get(number + 1).copied().unwrap_or(records.len());
            (*start, end)
        }),
    };

    let mut state = CommanderState::default();
    let Some((start, end)) = selected_range else {
        state.warn(
            WarningCode::MissingSession,
            "selected journal session does not exist",
            None,
            Some(ObservationSource::Journal),
        );
        return state;
    };

    for (line_number, text, value) in &records[start..end] {
        if let Some(value) = value {
            state.apply_event(value, ObservationSource::Journal, Some(*line_number));
        } else {
            // Parse once above for session discovery, but go through the public
            // method here so warning behavior stays identical.
            state.apply_journal_json(text, *line_number);
        }
    }
    state
}

/// Convenience for the normal policy: use the most recent LoadGame session.
#[must_use]
pub fn replay_latest_json_lines(input: &str) -> CommanderState {
    replay_json_lines(input, SessionSelection::Latest)
}

fn replace_if_fresh<T>(slot: &mut Option<Observed<T>>, incoming: Observed<T>) -> bool {
    let replace = slot
        .as_ref()
        .is_none_or(|current| observation_is_fresh(&incoming, current));
    if replace {
        *slot = Some(incoming);
    }
    replace
}

fn observation_is_fresh<T>(incoming: &Observed<T>, current: &Observed<T>) -> bool {
    match (&incoming.timestamp, &current.timestamp) {
        (Some(new), Some(old)) => match compare_timestamps(new, old) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => {
                // At equal clock time the journal is authoritative.  Equal-time
                // sidecars may replace one another to supply a fuller snapshot.
                current.source != ObservationSource::Journal
                    || incoming.source == ObservationSource::Journal
            }
        },
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => incoming.ordinal >= current.ordinal,
    }
}

fn compare_timestamps(left: &str, right: &str) -> Ordering {
    match (timestamp_key(left), timestamp_key(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

/// Parse the RFC3339 subset emitted by Elite Dangerous.  Supporting offsets as
/// well as `Z` costs little and avoids a misleading lexical comparison if a
/// third-party journal tool rewrites timestamps.
fn timestamp_key(raw: &str) -> Option<i128> {
    let bytes = raw.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = i64::from(parse_digits(bytes, 0, 4)?);
    let month = parse_digits(bytes, 5, 2)?;
    let day = parse_digits(bytes, 8, 2)?;
    let hour = parse_digits(bytes, 11, 2)?;
    let minute = parse_digits(bytes, 14, 2)?;
    let second = parse_digits(bytes, 17, 2)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut cursor = 19;
    let mut nanos = 0_i128;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        let mut digits = 0;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            if digits < 9 {
                nanos = nanos * 10 + i128::from(bytes[cursor] - b'0');
            }
            digits += 1;
            cursor += 1;
        }
        if cursor == start {
            return None;
        }
        for _ in digits.min(9)..9 {
            nanos *= 10;
        }
    }

    let offset_seconds: i64 = match bytes.get(cursor) {
        Some(b'Z' | b'z') if cursor + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) if cursor + 6 == bytes.len() => {
            if bytes.get(cursor + 3) != Some(&b':') {
                return None;
            }
            let hours = i64::from(parse_digits(bytes, cursor + 1, 2)?);
            let minutes = i64::from(parse_digits(bytes, cursor + 4, 2)?);
            if hours > 23 || minutes > 59 {
                return None;
            }
            let value = hours * 3600 + minutes * 60;
            if *sign == b'+' { value } else { -value }
        }
        _ => return None,
    };

    let days = days_from_civil(year, month, day);
    let local_seconds =
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    Some(i128::from(local_seconds - offset_seconds) * 1_000_000_000 + nanos)
}

fn parse_digits(bytes: &[u8], start: usize, count: usize) -> Option<u32> {
    let mut value = 0_u32;
    for byte in bytes.get(start..start + count)? {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u32::from(*byte - b'0');
    }
    Some(value)
}

// Howard Hinnant's civil-date conversion; an arbitrary epoch is sufficient for
// comparison, but this returns days relative to 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn string_field<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn owned_nonempty(object: &Map<String, Value>, key: &str) -> Option<String> {
    string_field(object, key)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn bool_field(object: &Map<String, Value>, key: &str) -> Option<bool> {
    object.get(key).and_then(|value| match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) if value.as_u64() == Some(0) => Some(false),
        Value::Number(value) if value.as_u64() == Some(1) => Some(true),
        _ => None,
    })
}

/// Read exact integer JSON numbers (or decimal strings used by a few tools).
/// Numeric ids never pass through floating point.
fn read_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            let value = number.as_f64()?;
            (value.is_finite() && value >= 0.0 && value.fract() == 0.0 && value <= u64::MAX as f64)
                .then_some(value as u64)
        }),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn read_u64_field(object: &Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(read_u64)
}

fn finite_nonnegative_field(object: &Map<String, Value>, key: &str) -> Option<f64> {
    object
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn coordinates_field(object: &Map<String, Value>, key: &str) -> Option<[f64; 3]> {
    let array = object.get(key)?.as_array()?;
    if array.len() != 3 {
        return None;
    }
    let coordinates = [array[0].as_f64()?, array[1].as_f64()?, array[2].as_f64()?];
    coordinates
        .iter()
        .all(|value| value.is_finite())
        .then_some(coordinates)
}

fn string_array_field(object: &Map<String, Value>, key: &str) -> Vec<String> {
    let mut result = Vec::new();
    for value in object
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(value) = value.as_str().filter(|value| !value.is_empty()) else {
            continue;
        };
        if !result
            .iter()
            .any(|known: &String| known.eq_ignore_ascii_case(value))
        {
            result.push(value.to_owned());
        }
    }
    result
}

fn parse_cargo_item(
    value: &Value,
    warnings: &mut Vec<CommanderWarning>,
    line: Option<usize>,
    source: ObservationSource,
) -> Option<CargoItem> {
    let Some(object) = value.as_object() else {
        warnings.push(CommanderWarning {
            code: WarningCode::InvalidField,
            message: "non-object cargo inventory entry ignored".to_owned(),
            line,
            source: Some(source),
        });
        return None;
    };
    let Some(name) = owned_nonempty(object, "Name") else {
        warnings.push(CommanderWarning {
            code: WarningCode::InvalidField,
            message: "cargo inventory entry without Name ignored".to_owned(),
            line,
            source: Some(source),
        });
        return None;
    };
    let Some(count) = read_u64_field(object, "Count") else {
        warnings.push(CommanderWarning {
            code: WarningCode::InvalidField,
            message: "cargo inventory entry with invalid Count ignored".to_owned(),
            line,
            source: Some(source),
        });
        return None;
    };
    Some(CargoItem {
        name,
        name_localised: owned_nonempty(object, "Name_Localised"),
        count,
        stolen: read_u64_field(object, "Stolen").unwrap_or(0),
        mission_id: read_u64_field(object, "MissionID"),
    })
}

fn parse_route_hop(value: &Value) -> Option<RouteHop> {
    let object = value.as_object()?;
    Some(RouteHop {
        star_system: owned_nonempty(object, "StarSystem")?,
        system_address: read_u64_field(object, "SystemAddress"),
        star_position: coordinates_field(object, "StarPos"),
        star_class: owned_nonempty(object, "StarClass"),
    })
}

fn parse_market_item(value: &Value) -> Option<MarketCommodity> {
    let object = value.as_object()?;
    Some(MarketCommodity {
        id: read_u64_field(object, "id").or_else(|| read_u64_field(object, "ID")),
        name: owned_nonempty(object, "Name").unwrap_or_default(),
        name_localised: owned_nonempty(object, "Name_Localised"),
        buy_price: read_u64_field(object, "BuyPrice"),
        sell_price: read_u64_field(object, "SellPrice"),
        mean_price: read_u64_field(object, "MeanPrice"),
        stock: read_u64_field(object, "Stock"),
        demand: read_u64_field(object, "Demand"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIG_ID: u64 = 72_060_832_334_024_995;

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "test asserts serde's exact-u64 and identity-omission contract"
    )]
    fn ids_above_two_to_the_53rd_are_exact() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Commander":"do not retain","FID":"F123","Credits":10}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:01Z","event":"Location","StarSystem":"Exact","SystemAddress":72060832334024995,"StarPos":[1,2,3],"Docked":true,"StationName":"Port","MarketID":72060832334024996}"#,
        );
        let state = replay_latest_json_lines(input);
        assert_eq!(
            state.current_system.as_ref().unwrap().value.address,
            Some(BIG_ID)
        );
        assert_eq!(state.current_market_id(), Some(BIG_ID + 1));
        let serialised = serde_json::to_string(&state).unwrap();
        assert!(serialised.contains("72060832334024996"));
        assert!(!serialised.contains("do not retain"));
        assert!(!serialised.contains("F123"));
    }

    #[test]
    fn latest_of_two_sessions_starts_from_a_clean_reset() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Credits":1000,"Ship":"cobra_mkiii"}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:01Z","event":"Location","StarSystem":"Old","SystemAddress":1}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:02Z","event":"Cargo","Vessel":"Ship","Count":4,"Inventory":[{"Name":"gold","Count":4}] }"#,
            "\n",
            r#"{"timestamp":"3309-01-02T00:00:00Z","event":"LoadGame","Credits":25,"Ship":"sidewinder"}"#,
            "\n",
            r#"{"timestamp":"3309-01-02T00:00:01Z","event":"Location","StarSystem":"New","SystemAddress":2}"#,
        );
        let state = replay_json_lines(input, SessionSelection::Latest);
        assert_eq!(state.current_system.unwrap().value.name, "New");
        assert_eq!(state.credits.unwrap().value, 25);
        assert_eq!(
            state.ship.unwrap().value.ship_type.as_deref(),
            Some("sidewinder")
        );
        assert!(state.cargo.inventory.is_none());
        assert!(state.executed_trades.is_empty());

        let all = replay_json_lines(input, SessionSelection::All);
        assert_eq!(all.current_system.unwrap().value.name, "New");
        assert!(
            all.cargo.inventory.is_none(),
            "LoadGame must reset even in an all-session replay"
        );
    }

    #[test]
    fn dock_and_undock_transitions_are_timestamped_tombstones() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Credits":1}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:01:00Z","event":"Docked","StationName":"Jameson Memorial","StationType":"Coriolis","MarketID":128666762,"StarSystem":"Shinrarta Dezhra","StationServices":["Commodities","Shipyard"]}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:02:00Z","event":"Undocked","StationName":"Jameson Memorial","MarketID":128666762}"#,
        );
        let mut state = replay_latest_json_lines(input);
        assert!(!state.is_docked());
        assert_eq!(state.current_market_id(), None);
        assert!(!state.market.as_ref().unwrap().value.accessible);

        let stale_market = r#"{"timestamp":"3309-01-01T00:01:30Z","event":"Market","MarketID":128666762,"StationName":"Jameson Memorial","StarSystem":"Shinrarta Dezhra","Items":[]}"#;
        assert!(state.merge_market_sidecar(stale_market));
        assert!(!state.is_docked());
        assert!(!state.market.as_ref().unwrap().value.accessible);
        assert!(
            state
                .warnings
                .iter()
                .any(|warning| warning.code == WarningCode::StaleSidecarIgnored)
        );
    }

    #[test]
    fn cargo_underflow_clamps_free_to_zero_and_warns() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Credits":1}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:01Z","event":"Loadout","Ship":"python","ShipID":7,"CargoCapacity":8,"MaxJumpRange":31.25}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:02Z","event":"Cargo","Vessel":"Ship","Count":10,"Inventory":[{"Name":"silver","Count":10}]}"#,
        );
        let state = replay_latest_json_lines(input);
        assert_eq!(state.cargo.used_value(), Some(10));
        assert_eq!(state.cargo.capacity_value(), Some(8));
        assert_eq!(state.cargo.free, Some(0));
        assert!(
            state
                .warnings
                .iter()
                .any(|warning| warning.code == WarningCode::CargoOverCapacity)
        );
        assert_eq!(state.ship.unwrap().value.max_jump_range, Some(31.25));
    }

    #[test]
    fn executed_buy_and_sell_are_observed_and_update_known_state() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Credits":10000}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:01Z","event":"Loadout","CargoCapacity":10,"Ship":"adder"}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:02Z","event":"Cargo","Vessel":"Ship","Count":2,"Inventory":[{"Name":"gold","Name_Localised":"Gold","Count":2,"Stolen":0}]}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:03Z","event":"MarketBuy","MarketID":72060832334024995,"Type":"gold","Type_Localised":"Gold","Count":3,"BuyPrice":100,"TotalCost":300}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:00:04Z","event":"MarketSell","MarketID":72060832334024995,"Type":"gold","Count":1,"SellPrice":120,"TotalSale":120,"BlackMarket":false}"#,
        );
        let state = replay_latest_json_lines(input);
        assert_eq!(state.executed_trades.len(), 2);
        assert_eq!(state.executed_trades[0].value.kind, TradeKind::Buy);
        assert_eq!(state.executed_trades[0].value.market_id, Some(BIG_ID));
        assert_eq!(state.executed_trades[1].value.kind, TradeKind::Sell);
        assert_eq!(state.credits.unwrap().value, 9820);
        assert_eq!(state.cargo.used_value(), Some(4));
        assert_eq!(state.cargo.free, Some(6));
        assert_eq!(state.cargo.inventory.unwrap().value[0].count, 4);
    }

    #[test]
    fn malformed_final_line_is_only_a_warning() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Credits":42}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:01:00Z","event":"Location","StarSystem":"Safe","SystemAddress":99}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:02:00Z","event":"Cargo","Count":2"#,
        );
        let state = replay_latest_json_lines(input);
        assert_eq!(state.current_system.unwrap().value.name, "Safe");
        assert_eq!(state.credits.unwrap().value, 42);
        assert!(state.warnings.iter().any(|warning| {
            warning.code == WarningCode::MalformedJson && warning.line == Some(3)
        }));
    }

    #[test]
    fn cargo_sum_is_checked() {
        let input = concat!(
            r#"{"event":"LoadGame","Credits":0}"#,
            "\n",
            r#"{"event":"Cargo","Vessel":"Ship","Inventory":[{"Name":"a","Count":18446744073709551615},{"Name":"b","Count":1}]}"#,
        );
        let state = replay_latest_json_lines(input);
        assert_eq!(state.cargo.used_value(), Some(u64::MAX));
        assert!(
            state
                .warnings
                .iter()
                .any(|warning| warning.code == WarningCode::ArithmeticOverflow)
        );
    }

    #[test]
    fn stale_cargo_and_route_sidecars_do_not_override_journal() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Credits":0}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:10:00Z","event":"Cargo","Vessel":"Ship","Count":5,"Inventory":[{"Name":"gold","Count":5}]}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:10:00.500Z","event":"NavRoute","Route":[{"StarSystem":"New","SystemAddress":72060832334024995,"StarPos":[1,2,3]}]}"#,
        );
        let mut state = replay_latest_json_lines(input);
        state.merge_cargo_sidecar(
            r#"{"timestamp":"3309-01-01T00:09:59Z","event":"Cargo","Vessel":"Ship","Count":1,"Inventory":[{"Name":"old","Count":1}]}"#,
        );
        state.merge_nav_route_sidecar(
            r#"{"timestamp":"3309-01-01T00:10:00.250Z","event":"NavRoute","Route":[{"StarSystem":"Old","SystemAddress":1}]}"#,
        );
        assert_eq!(state.cargo.used_value(), Some(5));
        assert_eq!(state.nav_route.unwrap().value.hops[0].star_system, "New");
    }

    #[test]
    fn carrier_jump_and_status_are_tolerant() {
        let input = concat!(
            r#"{"timestamp":"3309-01-01T00:00:00Z","event":"LoadGame","Credits":0}"#,
            "\n",
            r#"{"timestamp":"3309-01-01T00:01:00Z","event":"CarrierJump","StarSystem":"Carrier destination","SystemAddress":"72060832334024995","StarPos":[4.0,5.0,6.0],"Docked":true,"StationName":"ABC-123","StationType":"FleetCarrier","MarketID":"72060832334024996","StationServices":["Commodities"]}"#,
        );
        let mut state = replay_latest_json_lines(input);
        assert_eq!(
            state.current_system.as_ref().unwrap().value.arrival,
            SystemArrival::CarrierJump
        );
        assert!(state.is_docked());
        state.merge_status_sidecar(
            r#"{"timestamp":"3309-01-01T00:02:00Z","event":"Status","Flags":0,"Cargo":3.0}"#,
        );
        assert!(!state.is_docked());
        assert_eq!(state.cargo.used_value(), Some(3));
    }

    #[test]
    fn an_explicit_earlier_session_can_be_selected() {
        let input = concat!(
            r#"{"event":"LoadGame","Credits":1}"#,
            "\n",
            r#"{"event":"Location","StarSystem":"First"}"#,
            "\n",
            r#"{"event":"LoadGame","Credits":2}"#,
            "\n",
            r#"{"event":"Location","StarSystem":"Second"}"#,
        );
        let state = replay_json_lines(input, SessionSelection::Number(0));
        assert_eq!(state.current_system.unwrap().value.name, "First");
        assert_eq!(state.credits.unwrap().value, 1);
    }
}

#[cfg(test)]
mod carrier_door_tests {
    use super::*;

    fn door_of(state: &CommanderState, market_id: u64) -> Option<&DoorObservation> {
        state
            .carrier_doors
            .iter()
            .find(|(id, _)| *id == market_id)
            .map(|(_, observation)| observation)
    }

    fn replay(lines: &[&str]) -> CommanderState {
        let mut state = CommanderState::default();
        for (n, line) in lines.iter().enumerate() {
            state.apply_journal_json(line, n + 1);
        }
        state
    }

    /// The event that started this: a real line from a real journal.
    #[test]
    fn a_restricted_access_denial_is_recorded() {
        let state = replay(&[
            r#"{ "timestamp":"2026-08-26T07:18:31Z", "event":"DockingDenied", "Reason":"RestrictedAccess", "MarketID":3712438528, "StationName":"1GOT", "StationType":"FleetCarrier" }"#,
        ]);
        assert_eq!(
            door_of(&state, 3_712_438_528).map(|o| o.clone()).as_ref(),
            Some(&DoorObservation {
                door: CarrierDoor::Refused,
                observed_at: Some("2026-08-26T07:18:31Z".to_owned()),
            })
        );
    }

    /// `NoSpace` is a full pad and `Distance` is a bad approach. Neither says
    /// anything about who the door opens for, and recording them would filter
    /// carriers on a transient.
    #[test]
    fn other_denial_reasons_say_nothing_about_the_door() {
        let state = replay(&[
            r#"{"timestamp":"2026-08-26T07:00:00Z","event":"DockingDenied","Reason":"NoSpace","MarketID":1,"StationName":"A","StationType":"FleetCarrier"}"#,
            r#"{"timestamp":"2026-08-26T07:00:01Z","event":"DockingDenied","Reason":"Distance","MarketID":2,"StationName":"B","StationType":"FleetCarrier"}"#,
            r#"{"timestamp":"2026-08-26T07:00:02Z","event":"DockingDenied","Reason":"TooLarge","MarketID":3,"StationName":"C","StationType":"FleetCarrier"}"#,
        ]);
        assert!(state.carrier_doors.is_empty());
    }

    #[test]
    fn a_successful_docking_records_admission() {
        let state = replay(&[
            r#"{"timestamp":"2026-08-26T07:00:00Z","event":"Docked","StationName":"ABC-123","StationType":"FleetCarrier","MarketID":42}"#,
        ]);
        assert_eq!(
            door_of(&state, 42).map(|o| o.door),
            Some(CarrierDoor::Admitted)
        );
    }

    /// An owner can lock a carrier between two visits, and does. The newest
    /// observation is the one that describes the door now.
    #[test]
    fn the_newest_observation_wins_in_both_directions() {
        let opened = replay(&[
            r#"{"timestamp":"2026-08-26T07:00:00Z","event":"DockingDenied","Reason":"RestrictedAccess","MarketID":42,"StationName":"A","StationType":"FleetCarrier"}"#,
            r#"{"timestamp":"2026-08-26T09:00:00Z","event":"Docked","StationName":"A","StationType":"FleetCarrier","MarketID":42}"#,
        ]);
        assert_eq!(
            door_of(&opened, 42).map(|o| o.door),
            Some(CarrierDoor::Admitted)
        );

        let closed = replay(&[
            r#"{"timestamp":"2026-08-26T07:00:00Z","event":"Docked","StationName":"A","StationType":"FleetCarrier","MarketID":42}"#,
            r#"{"timestamp":"2026-08-26T09:00:00Z","event":"DockingDenied","Reason":"RestrictedAccess","MarketID":42,"StationName":"A","StationType":"FleetCarrier"}"#,
        ]);
        assert_eq!(
            door_of(&closed, 42).map(|o| o.door),
            Some(CarrierDoor::Refused)
        );
    }

    /// An out-of-order line must not undo a later one. Journals are merged from
    /// several files and the merge is by first-timestamp, not per line.
    #[test]
    fn an_older_line_arriving_late_does_not_win() {
        let state = replay(&[
            r#"{"timestamp":"2026-08-26T09:00:00Z","event":"DockingDenied","Reason":"RestrictedAccess","MarketID":42,"StationName":"A","StationType":"FleetCarrier"}"#,
            r#"{"timestamp":"2026-08-26T07:00:00Z","event":"Docked","StationName":"A","StationType":"FleetCarrier","MarketID":42}"#,
        ]);
        assert_eq!(
            door_of(&state, 42).map(|o| o.door),
            Some(CarrierDoor::Refused)
        );
    }

    /// An ordinary starport's docking permission is a function of allegiance
    /// and bounties, not an owner's setting, and nothing reads it.
    #[test]
    fn only_fleet_carriers_are_recorded() {
        let state = replay(&[
            r#"{"timestamp":"2026-08-26T07:00:00Z","event":"Docked","StationName":"Jameson Memorial","StationType":"Coriolis","MarketID":128666762}"#,
            r#"{"timestamp":"2026-08-26T07:00:01Z","event":"DockingDenied","Reason":"RestrictedAccess","MarketID":128666763,"StationName":"Somewhere","StationType":"Orbis"}"#,
        ]);
        assert!(state.carrier_doors.is_empty());
    }

    /// Everything else here is a session value and is cleared. A door is not:
    /// a carrier that refused this commander is still refusing them after they
    /// quit to the main menu, and forgetting it would throw away the only
    /// commander-specific evidence the program has.
    #[test]
    fn doors_survive_a_new_session_when_everything_else_does_not() {
        let state = replay(&[
            r#"{"timestamp":"2026-08-26T07:00:00Z","event":"DockingDenied","Reason":"RestrictedAccess","MarketID":42,"StationName":"A","StationType":"FleetCarrier"}"#,
            r#"{"timestamp":"2026-08-26T07:00:01Z","event":"Docked","StationName":"Jameson Memorial","StationType":"Coriolis","MarketID":128666762}"#,
            r#"{"timestamp":"2026-08-26T08:00:00Z","event":"LoadGame","Commander":"Jameson"}"#,
        ]);
        assert!(state.docking.is_none(), "session state is cleared");
        assert_eq!(
            door_of(&state, 42).map(|o| o.door),
            Some(CarrierDoor::Refused),
            "the door is not"
        );
    }
}
