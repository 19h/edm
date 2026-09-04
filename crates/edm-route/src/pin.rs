//! A route's identity across solves, for pinning it \[C53\].
//!
//! Every [`Route`] addresses its markets by index into the vector it was
//! solved over, and its commodities by [`CommodityId`], which is only stable
//! within one [`Commodities`] interner. Neither survives a fresh ingest. What
//! does survive is the pair `RankKey` already ends its total order on — the
//! market ids and the commodities carried, in one canonical rotation — spelled
//! here with names instead of ids so it can be written to disk and read back
//! against any later instance.
//!
//! A pinned route is re-priced by rebuilding a *skeleton* of itself over
//! freshly read markets and handing that to [`crate::rescore::rescore`], which
//! re-prices a route with its commodity held fixed and drops it when a leg no
//! longer trades. That is exactly the question a pin asks — "does the route I
//! chose still trade, and at what" — and it never turns into a search that
//! could quietly answer with a different route.

use edm_core::js::json::{JsObject, JsValue};

use crate::model::{Commodities, Market};
use crate::num::{Credits, Tons};
use crate::report::{Route, RouteKind};
use crate::time::{Geometry, TimeModel};
use crate::weight::{LegChoice, Limiter};

/// The shape of a pinned route, spelled for a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PinKind {
    OneWay,
    RoundTrip,
    Loop,
}

impl PinKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::OneWay => "one-way",
            Self::RoundTrip => "round-trip",
            Self::Loop => "loop",
        }
    }

    fn parse(label: &str) -> Option<Self> {
        match label {
            "one-way" => Some(Self::OneWay),
            "round-trip" => Some(Self::RoundTrip),
            "loop" => Some(Self::Loop),
            _ => None,
        }
    }
}

/// What makes a route the same route on another day.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PinKey {
    pub kind: PinKind,
    /// Market ids in flying order. For a cycle, rotated so the smallest is
    /// first and the last leg returns to it; for a one-way hop, origin then
    /// destination.
    pub stations: Vec<i64>,
    /// The commodity each leg carries, one per departure in `stations`, in the
    /// same rotation. A one-way hop has one.
    pub commodities: Vec<String>,
}

impl PinKey {
    /// The key of a solved route.
    #[must_use]
    pub fn of(route: &Route, markets: &[Market], commodities: &Commodities) -> Self {
        let id = |index: u32| markets.get(index as usize).map_or(0, |m| m.market_id);
        let name = |leg: &crate::report::RouteLeg| {
            commodities
                .name(leg.choice.commodity)
                .unwrap_or("?")
                .to_owned()
        };
        let mut stations: Vec<i64> = route.legs.iter().map(|leg| id(leg.from)).collect();
        let mut carried: Vec<String> = route.legs.iter().map(name).collect();
        let kind = match route.kind {
            RouteKind::SingleHop => {
                if let Some(last) = route.legs.last() {
                    stations.push(id(last.to));
                }
                PinKind::OneWay
            }
            RouteKind::RoundTrip => PinKind::RoundTrip,
            RouteKind::Loop { .. } => PinKind::Loop,
        };
        if kind != PinKind::OneWay && !stations.is_empty() {
            // The same rotation `RankKey` uses: smallest market id first.
            let start = stations
                .iter()
                .enumerate()
                .min_by_key(|(_, id)| **id)
                .map_or(0, |(index, _)| index);
            stations.rotate_left(start);
            carried.rotate_left(start);
        }
        Self {
            kind,
            stations,
            commodities: carried,
        }
    }

    /// Whether `route` is this key's route.
    #[must_use]
    pub fn matches(&self, route: &Route, markets: &[Market], commodities: &Commodities) -> bool {
        *self == Self::of(route, markets, commodities)
    }

    /// The route this key names, priced at nothing, over `markets`.
    ///
    /// `None` when a station or commodity the key names is not in the
    /// instance. Hand the result to [`crate::rescore::rescore`], which prices
    /// it or drops it.
    #[must_use]
    pub fn skeleton(
        &self,
        markets: &[Market],
        commodities: &Commodities,
        time: TimeModel,
    ) -> Option<Route> {
        let index = |id: i64| {
            markets
                .iter()
                .position(|market| market.market_id == id)
                .map(|position| position as u32)
        };
        let nodes: Vec<u32> = self
            .stations
            .iter()
            .map(|id| index(*id))
            .collect::<Option<Vec<_>>>()?;
        let choices: Vec<LegChoice> = self
            .commodities
            .iter()
            .map(|name| {
                Some(LegChoice {
                    commodity: commodities.id_of_symbol(name)?,
                    buy_price: Credits(0),
                    sell_price: Credits(0),
                    units: Tons(0),
                    profit: Credits(0),
                    limiter: Limiter::Cargo,
                    demand_assumed: false,
                    bulk_estimated: false,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let geometry = Geometry::new(markets, time);
        match self.kind {
            PinKind::OneWay => {
                if nodes.len() != 2 || choices.len() != 1 {
                    return None;
                }
                Some(Route::single_hop(&geometry, nodes[0], nodes[1], choices[0]))
            }
            PinKind::RoundTrip | PinKind::Loop => {
                if nodes.len() < 2 || nodes.len() != choices.len() {
                    return None;
                }
                Some(Route::cycle(&geometry, &nodes, &choices))
            }
        }
    }

    /// `{"kind": "one-way", "stations": [..], "commodities": [..]}`.
    #[must_use]
    pub fn to_json(&self) -> JsValue {
        JsValue::Obj(JsObject::from_document_order(vec![
            ("kind".into(), JsValue::Str(self.kind.label().into())),
            (
                "stations".into(),
                JsValue::Arr(
                    self.stations
                        .iter()
                        .map(|id| JsValue::Num(*id as f64))
                        .collect(),
                ),
            ),
            (
                "commodities".into(),
                JsValue::Arr(
                    self.commodities
                        .iter()
                        .map(|name| JsValue::Str(name.as_str().into()))
                        .collect(),
                ),
            ),
        ]))
    }

    /// The inverse of [`PinKey::to_json`]; `None` for anything malformed.
    #[must_use]
    pub fn from_json(value: &JsValue) -> Option<Self> {
        let object = value.as_object()?;
        let kind = PinKind::parse(object.get("kind")?.as_str()?)?;
        let stations: Vec<i64> = object
            .get("stations")?
            .as_array()?
            .iter()
            .map(|id| {
                let id = id.as_f64()?;
                (id.is_finite() && id.fract() == 0.0).then_some(id as i64)
            })
            .collect::<Option<Vec<_>>>()?;
        let commodities: Vec<String> = object
            .get("commodities")?
            .as_array()?
            .iter()
            .map(|name| name.as_str().map(ToOwned::to_owned))
            .collect::<Option<Vec<_>>>()?;
        let key = Self {
            kind,
            stations,
            commodities,
        };
        let well_formed = match key.kind {
            PinKind::OneWay => key.stations.len() == 2 && key.commodities.len() == 1,
            PinKind::RoundTrip | PinKind::Loop => {
                key.stations.len() >= 2 && key.stations.len() == key.commodities.len()
            }
        };
        well_formed.then_some(key)
    }

    /// `Station A > Station B (gold)`-style summary over the markets that name
    /// the stations, falling back to ids.
    #[must_use]
    pub fn describe(&self, markets: &[Market]) -> String {
        let name = |id: i64| {
            markets
                .iter()
                .find(|market| market.market_id == id)
                .map_or_else(|| id.to_string(), |market| market.station.clone())
        };
        let mut stops: Vec<String> = self.stations.iter().map(|id| name(*id)).collect();
        if self.kind != PinKind::OneWay
            && let Some(first) = stops.first().cloned()
        {
            stops.push(first);
        }
        format!(
            "{} ({})",
            stops.join(" > "),
            self.commodities
                .iter()
                .map(|name| crate::view::readable(name))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{choice, geometry, limits, market, ship};
    use crate::model::Commodities;

    const GOLD: u32 = 0;
    const SILVER: u32 = 1;
    const TEA: u32 = 2;

    fn commodities() -> Commodities {
        let mut names = Commodities::default();
        names.intern("gold");
        names.intern("silver");
        names.intern("tea");
        names
    }

    /// Three markets in a triangle; gold 1→2, silver 2→3, tea 3→1.
    fn triangle() -> Vec<Market> {
        vec![
            market(30, 0.0, &[(GOLD, 100, 100)], &[(TEA, 500, 100)]),
            market(10, 10.0, &[(SILVER, 100, 100)], &[(GOLD, 500, 100)]),
            market(20, 20.0, &[(TEA, 100, 100)], &[(SILVER, 500, 100)]),
        ]
    }

    /// The same cycle started from any node is one key, and it starts at the
    /// smallest market id.
    #[test]
    fn a_cycle_has_one_key_whatever_node_it_started_from() {
        let markets = triangle();
        let names = commodities();
        let geometry = geometry(&markets);
        let choices = [choice(GOLD, 1), choice(SILVER, 1), choice(TEA, 1)];
        let from_first = Route::cycle(&geometry, &[0, 1, 2], &choices);
        let from_second = Route::cycle(&geometry, &[1, 2, 0], &[choices[1], choices[2], choices[0]]);
        let from_third = Route::cycle(&geometry, &[2, 0, 1], &[choices[2], choices[0], choices[1]]);
        let key = PinKey::of(&from_first, &markets, &names);
        assert_eq!(key.kind, PinKind::Loop);
        assert_eq!(key.stations, vec![10, 20, 30]);
        assert_eq!(key.commodities, vec!["silver", "tea", "gold"]);
        assert_eq!(PinKey::of(&from_second, &markets, &names), key);
        assert_eq!(PinKey::of(&from_third, &markets, &names), key);
        assert!(key.matches(&from_third, &markets, &names));
        assert_eq!(
            key.describe(&markets),
            "Station 10 > Station 20 > Station 30 > Station 10 (silver, tea, gold)"
        );
    }

    #[test]
    fn a_one_way_hop_names_both_ends() {
        let markets = triangle();
        let names = commodities();
        let geometry = geometry(&markets);
        let hop = Route::single_hop(&geometry, 0, 1, choice(GOLD, 1));
        let key = PinKey::of(&hop, &markets, &names);
        assert_eq!(key.kind, PinKind::OneWay);
        assert_eq!(key.stations, vec![30, 10]);
        assert_eq!(key.commodities, vec!["gold"]);
    }

    #[test]
    fn a_key_round_trips_through_json_and_refuses_malformed_input() {
        let key = PinKey {
            kind: PinKind::RoundTrip,
            stations: vec![128_016_384, 3_705_929_472],
            commodities: vec!["gold".to_owned(), "silver".to_owned()],
        };
        let text = key.to_json().stringify_compact();
        assert_eq!(
            text,
            r#"{"kind":"round-trip","stations":[128016384,3705929472],"commodities":["gold","silver"]}"#
        );
        assert_eq!(PinKey::from_json(&JsValue::parse(&text).unwrap()), Some(key));
        for bad in [
            r#"{"kind":"loop","stations":[1],"commodities":["gold"]}"#,
            r#"{"kind":"one-way","stations":[1,2,3],"commodities":["gold"]}"#,
            r#"{"kind":"one-way","stations":[1,2.5],"commodities":["gold"]}"#,
            r#"{"kind":"triangle","stations":[1,2],"commodities":["gold"]}"#,
            r#"[]"#,
        ] {
            assert_eq!(PinKey::from_json(&JsValue::parse(bad).unwrap()), None, "{bad}");
        }
    }

    /// A skeleton over fresh markets re-prices to the route the key names,
    /// and a market that stopped trading drops it rather than re-routing it.
    #[test]
    fn a_skeleton_is_repriced_by_rescore_and_dropped_when_a_leg_dies() {
        let markets = triangle();
        let names = commodities();
        let key = PinKey {
            kind: PinKind::Loop,
            stations: vec![10, 20, 30],
            commodities: vec!["silver".into(), "tea".into(), "gold".into()],
        };
        // Markets in a different order from the one the key was built over.
        let mut shuffled = markets.clone();
        shuffled.rotate_left(2);
        let skeleton = key.skeleton(&shuffled, &names, TimeModel::default()).expect("all present");
        let priced = crate::rescore::rescore(&shuffled, TimeModel::default(), &ship(), &limits(), vec![skeleton]);
        assert_eq!(priced.len(), 1);
        assert!(key.matches(&priced[0], &shuffled, &names));
        assert!(priced[0].profit.0 > 0, "{:?}", priced[0].profit);

        // The gold buyer withdraws its order: the pin is gone, not replaced.
        let mut dead = shuffled.clone();
        for market in &mut dead {
            market.demand.retain(|row| row.commodity.0 != GOLD);
        }
        let skeleton = key.skeleton(&dead, &names, TimeModel::default()).expect("stations still present");
        let priced = crate::rescore::rescore(&dead, TimeModel::default(), &ship(), &limits(), vec![skeleton]);
        assert!(priced.is_empty());

        // A station missing from the instance is not a route at all.
        assert!(key.skeleton(&dead[..2], &names, TimeModel::default()).is_none());
    }
}
