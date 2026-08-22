//! Reading a star system's markets out of the `/2.0/elite/starsystem` payload.
//!
//! Confirmed shape: `starsystem.polities[n].markets[marketId]`, each entry
//! carrying its own `id`, name, `poiType`/`outpostType`, `imported`/`exported`
//! commodity maps, services, economies, `bodyName` and `distFromSystem`. The
//! controlling faction resolves through
//! `starsystem.starsystem.minorFactions[controllingMinorFaction]`.
//!
//! The payload is around 500 KB and the captured logs that documented it
//! truncate at 16 KB, so the structural fallback in [`collect_points_of_interest`]
//! exists for the case where that shape drifts: rather than guess, it walks the
//! tree for anything carrying a market id or looking station-like and reports
//! where each hit was found.

use std::borrow::Cow;

use crate::js::json::{JsObject, JsValue};
use crate::js::{self, collate};

use super::read::{self, Read};

/// The five services the tables actually query.
///
/// The payload carries many more; `availableServices` (ts:2683) collects them
/// all into a set that is then only ever asked about these, so the rest are
/// dropped at parse time rather than allocated and discarded later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Services(u8);

impl Services {
    pub const COMMODITIES: Self = Self(1 << 0);
    pub const BLACK_MARKET: Self = Self(1 << 1);
    pub const OUTFITTING: Self = Self(1 << 2);
    pub const SHIPYARD: Self = Self(1 << 3);
    pub const REFUEL: Self = Self(1 << 4);

    /// In the order the `CBOYF` column renders them.
    pub const COLUMNS: [(Self, char); 5] = [
        (Self::COMMODITIES, 'C'),
        (Self::BLACK_MARKET, 'B'),
        (Self::OUTFITTING, 'O'),
        (Self::SHIPYARD, 'Y'),
        (Self::REFUEL, 'F'),
    ];

    #[must_use]
    pub fn has(self, service: Self) -> bool {
        self.0 & service.0 != 0
    }

    /// A service counts only when its state is exactly `"ok"`.
    fn read(market: &JsObject) -> Self {
        let Some(services) = market.record("services") else {
            return Self::default();
        };
        let mut flags = 0u8;
        for (name, state) in services.iter() {
            if state.as_str() != Some("ok") {
                continue;
            }
            flags |= match name.to_lowercase().as_str() {
                "commodities" => Self::COMMODITIES.0,
                "blackmarket" => Self::BLACK_MARKET.0,
                "outfitting" => Self::OUTFITTING.0,
                "shipyard" => Self::SHIPYARD.0,
                "refuel" => Self::REFUEL.0,
                _ => 0,
            };
        }
        Self(flags)
    }
}

/// One dockable location in a system.
#[derive(Clone, Debug)]
pub struct MarketPoint<'a> {
    pub market_id: f64,
    pub name: Cow<'a, str>,
    /// A normalised `poiType`, or the raw value when it is not one we know.
    pub kind: Cow<'a, str>,
    pub body_name: Option<&'a str>,
    pub distance: Option<f64>,
    pub economy: Option<&'a str>,
    pub faction: Option<&'a str>,
    pub imports: usize,
    pub exports: usize,
    pub services: Services,
    pub market_state: &'a str,
}

impl MarketPoint<'_> {
    /// Does this location trade commodities at all? The sweep skips the ones
    /// that do not unless `--all-markets` is given.
    #[must_use]
    pub fn trades(&self) -> bool {
        self.imports > 0 || self.exports > 0
    }

    #[must_use]
    pub fn is_carrier(&self) -> bool {
        self.kind == "carrier"
    }
}

/// `poiType` values seen in live payloads, normalised to something a table can
/// hold.
fn normalise_poi_type(raw: &str) -> Option<&'static str> {
    // A `match` rather than a map lookup, deliberately. The TypeScript indexes
    // an object literal, so `POI_TYPE_LABELS["constructor"]` walks the
    // prototype chain and yields a *function*, which `??` then accepts as the
    // label and which later kills the command at `type.toUpperCase()`.
    // Reproducing that would mean modelling a function value in the type system
    // for no benefit; we return the raw string instead. C14.
    Some(match raw {
        "starport" => "starport",
        "outpost" => "outpost",
        "dockableplanetstation" => "planetary",
        "onfootsettlement" => "settlement",
        "fleetcarrier" => "carrier",
        "megaship" => "megaship",
        "gameplaypoi" => "poi",
        _ => return None,
    })
}

/// Display order for the type bands: places you can dock and trade at first.
pub const POI_TYPE_ORDER: [&str; 7] = [
    "starport",
    "outpost",
    "planetary",
    "settlement",
    "megaship",
    "poi",
    "carrier",
];

/// Sort key for a type band. Unknown types sort after every known one, then
/// alphabetically among themselves.
#[must_use]
pub fn poi_type_rank(kind: &str) -> usize {
    POI_TYPE_ORDER
        .iter()
        .position(|k| *k == kind)
        .unwrap_or(POI_TYPE_ORDER.len())
}

/// The economy with the largest proportion, first one winning a tie.
fn primary_economy(market: &JsObject) -> Option<&str> {
    let economies = market.record("economies")?;
    let mut best: Option<(&str, f64)> = None;
    for (_, entry) in economies.iter() {
        let Some(record) = entry.as_record() else {
            continue;
        };
        let name = record.string("name");
        let proportion = record.num("proportion");
        if !name.is_empty() && best.is_none_or(|(_, top)| proportion > top) {
            best = Some((name, proportion));
        }
    }
    best.map(|(name, _)| name)
}

/// `countEntries` (ts:2693) — an object's key count, an array's length, else 0.
fn count_entries(value: Option<&JsValue>) -> usize {
    match value {
        Some(JsValue::Obj(o)) => o.len(),
        Some(JsValue::Arr(a)) => a.len(),
        _ => 0,
    }
}

/// `lookupFaction` (ts:2703).
///
/// Minor faction ids run past 2⁵³ — `72060832334024995` is a real one — so
/// `JSON.parse` rounds the *value* while the map's *keys* keep every digit, and
/// an exact string match misses. The TypeScript therefore falls back to a linear
/// rescan comparing the rounded forms, and that rescan is live precisely
/// because of the precision loss. Modelling ids as `u64` here would find the
/// faction "more correctly" and print a different name; we keep the `f64`
/// comparison. R19.
fn lookup_faction<'a>(factions: &'a JsObject, id: Option<&JsValue>) -> Option<&'a JsObject> {
    let id = id?;
    let as_key = match id {
        JsValue::Num(n) => js::js_number(*n),
        JsValue::Str(s) => s.to_string(),
        // The TypeScript guards on `typeof id`, so anything else is not a key.
        _ => return None,
    };

    if let Some(direct) = factions.record(&as_key) {
        return Some(direct);
    }

    let wanted = match id {
        JsValue::Num(n) => *n,
        JsValue::Str(s) => js::to_number(s),
        _ => return None,
    };
    if !wanted.is_finite() {
        return None;
    }
    factions
        .iter()
        .find(|(key, _)| js::to_number(key) == wanted)
        .and_then(|(_, value)| value.as_record())
}

/// `readMarketPoints` (ts:2715).
#[must_use]
pub fn read_market_points(payload: &JsObject) -> Vec<MarketPoint<'_>> {
    let Some(outer) = payload.record("starsystem") else {
        return Vec::new();
    };
    let Some(polities) = outer.record("polities") else {
        return Vec::new();
    };
    let factions = outer
        .record("starsystem")
        .and_then(|core| core.record("minorFactions"));

    let mut points = Vec::new();
    for (_, polity_value) in polities.iter() {
        let Some(polity) = polity_value.as_record() else {
            continue;
        };
        let Some(markets) = polity.record("markets") else {
            continue;
        };

        let faction = factions
            .and_then(|f| lookup_faction(f, polity.get("controllingMinorFaction")))
            .map(|record| record.string("name"))
            .filter(|name| !name.is_empty());

        for (key, value) in markets.iter() {
            let Some(market) = value.as_record() else {
                continue;
            };
            // Only two fallbacks here, where `toCommodity` has three — so a
            // key that is not numeric leaves this as NaN and the guard below
            // drops the market. R16.
            let market_id = read::or_else(market.num("id"), || js::to_number(key));
            if !js::safe_int(market_id) || market_id <= 0.0 {
                continue;
            }

            let poi_type = market.string("poiType");
            let kind = match normalise_poi_type(&poi_type.to_lowercase()) {
                Some(label) => Cow::Borrowed(label),
                None => Cow::Borrowed(read::or_else_str(
                    poi_type,
                    read::or_else_str(market.string("outpostType"), "unknown"),
                )),
            };

            let distance = market.num("distFromSystem");
            let body_name = market.string("bodyName");

            points.push(MarketPoint {
                market_id,
                name: if market.string("name").is_empty() {
                    Cow::Owned(format!("market {}", js::js_number(market_id)))
                } else {
                    Cow::Borrowed(market.string("name"))
                },
                kind,
                body_name: (!body_name.is_empty()).then_some(body_name),
                distance: (distance > 0.0).then_some(distance),
                economy: primary_economy(market),
                faction,
                imports: count_entries(market.get("imported")),
                exports: count_entries(market.get("exported")),
                services: Services::read(market),
                market_state: market.string("market_state"),
            });
        }
    }
    points
}

// ---------------------------------------------------------------------------
// Structural fallback
// ---------------------------------------------------------------------------

/// Anything in the payload that looks like a dockable location, found by shape
/// rather than by path.
#[derive(Clone, Debug)]
pub struct PointOfInterest<'a> {
    pub name: &'a str,
    pub market_id: Option<f64>,
    pub kind: Option<&'a str>,
    pub economy: Option<&'a str>,
    pub faction: Option<&'a str>,
    /// Where in the tree it was found, so a shape change can be diagnosed.
    pub path: String,
}

const MARKET_ID_KEYS: [&str; 3] = ["marketid", "market_id", "marketids"];
const NAME_KEYS: [&str; 6] = [
    "name",
    "stationname",
    "station_name",
    "marketname",
    "portname",
    "settlementname",
];
const TYPE_KEYS: [&str; 6] = [
    "type",
    "stationtype",
    "station_type",
    "porttype",
    "subtype",
    "kind",
];
const ECONOMY_KEYS: [&str; 4] = ["economy", "primaryeconomy", "economyname", "economy_name"];
const FACTION_KEYS: [&str; 5] = [
    "faction",
    "controllingfaction",
    "minorfaction",
    "owner",
    "ownername",
];

/// Keys lowercased once per record, as `lowerKeys` (ts:2546) does.
struct LowerKeys<'a>(Vec<(String, &'a JsValue)>);

impl<'a> LowerKeys<'a> {
    fn of(record: &'a JsObject) -> Self {
        Self(record.iter().map(|(k, v)| (k.to_lowercase(), v)).collect())
    }

    fn get(&self, key: &str) -> Option<&'a JsValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
    }

    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    fn pick_string(&self, keys: &[&str]) -> Option<&'a str> {
        keys.iter().find_map(|key| {
            self.get(key)
                .and_then(JsValue::as_str)
                .filter(|value| !crate::js::text::js_trim(value).is_empty())
        })
    }

    fn pick_number(&self, keys: &[&str]) -> Option<f64> {
        keys.iter().find_map(|key| {
            self.get(key)
                .and_then(JsValue::as_f64)
                .filter(|value| js::safe_int(*value) && *value > 0.0)
        })
    }
}

/// `collectPointsOfInterest` (ts:2568).
#[must_use]
pub fn collect_points_of_interest(payload: &JsValue) -> Vec<PointOfInterest<'_>> {
    let mut found: Vec<(String, PointOfInterest<'_>)> = Vec::new();
    walk(payload, "", 0, &mut found);

    let mut points: Vec<PointOfInterest<'_>> = found.into_iter().map(|(_, p)| p).collect();
    // Entries carrying a market id first, then by name. Stable, so equal names
    // keep discovery order.
    points.sort_by(|left, right| {
        left.market_id
            .is_none()
            .cmp(&right.market_id.is_none())
            .then_with(|| collate::locale_cmp(left.name, right.name))
    });
    points
}

fn walk<'a>(
    value: &'a JsValue,
    path: &str,
    depth: usize,
    found: &mut Vec<(String, PointOfInterest<'a>)>,
) {
    if depth > 12 {
        return;
    }
    if let JsValue::Arr(items) = value {
        for (index, entry) in items.iter().enumerate() {
            walk(entry, &format!("{path}[{index}]"), depth + 1, found);
        }
        return;
    }
    let Some(record) = value.as_record() else {
        return;
    };

    let fields = LowerKeys::of(record);
    let market_id = fields.pick_number(&MARKET_ID_KEYS);
    let name = fields.pick_string(&NAME_KEYS);
    let kind = fields.pick_string(&TYPE_KEYS);

    // A market id is proof on its own; otherwise a name plus a station-ish
    // companion field. The type pattern is `/…/i` with no `u` flag, so its
    // case-insensitivity is ASCII-only. R32.
    let looks_like_port = market_id.is_some()
        || (name.is_some()
            && (fields.has("services")
                || fields.has("landingpads")
                || fields.has("dockingaccess")
                || kind.is_some_and(|k| {
                    let lower = k.to_ascii_lowercase();
                    [
                        "station",
                        "port",
                        "settlement",
                        "outpost",
                        "carrier",
                        "hub",
                        "dock",
                    ]
                    .iter()
                    .any(|needle| lower.contains(needle))
                })));

    if looks_like_port && let Some(name) = name {
        let key = match market_id {
            Some(id) => format!("market:{}", js::js_number(id)),
            None => format!("path:{path}:{name}"),
        };
        if !found.iter().any(|(existing, _)| *existing == key) {
            found.push((
                key,
                PointOfInterest {
                    name,
                    market_id,
                    kind,
                    economy: fields.pick_string(&ECONOMY_KEYS),
                    faction: fields.pick_string(&FACTION_KEYS),
                    path: path.to_owned(),
                },
            ));
        }
    }

    for (key, child) in record.iter() {
        let child_path = if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}.{key}")
        };
        walk(child, &child_path, depth + 1, found);
    }
}
