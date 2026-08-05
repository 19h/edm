//! Which markets are worth a Companion API request.
//!
//! Every request this filter removes is one that is never sent, so this is the
//! single decision that sets what a wide sweep costs. Measured against Ardent,
//! filtering to starports removes 87–93% of the markets in a region.
//!
//! **The saving is the smaller half of the argument.** A 1,232-tonne hauler
//! cannot berth at an Odyssey settlement or an outpost at all, so a route
//! through one is not a cheaper answer, it is a wrong one. Near Sol 63% of what
//! Ardent calls a station is an on-foot settlement; near Colonia 58% are fleet
//! carriers. The filter is correctness, and the request count follows.
//!
//! Every exclusion is **counted and named**, with the flag that would undo it,
//! because a user shown only the surviving count cannot tell a deliberate
//! filter from a tool that quietly missed most of the region.

use std::cmp::Ordering;

use crate::ardent::{ArdentStation, is_carrier, is_starport, separation_ly};
use crate::cli::config::{Pad, RouteConfig};
use crate::domain::id64::Coordinates;
use crate::spend::Exclusion;

/// What survived, and what did not.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    /// The markets worth spending a request on, in the order they arrived.
    pub keep: Vec<ArdentStation>,
    /// One line per filter that removed something, in the order the filters
    /// ran — which is the order that explains the arithmetic.
    pub exclusions: Vec<Exclusion>,
    /// How many stations were considered.
    pub considered: usize,
}

/// Why a station was dropped.
///
/// Ordered by how much each one removes in practice, so the plan's first line
/// is the one that accounts for most of the difference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    Settlement,
    Carrier,
    NotAStarport,
    TooFarFromTheStar,
    BeyondTheRadius,
}

impl Reason {
    /// The exclusion label and the flag that keeps these stations.
    const fn describe(self) -> (&'static str, &'static str) {
        match self {
            // Named for what they are rather than for the filter, because
            // "settlements" is the word a commander would use.
            Self::Settlement => ("Odyssey settlements", "--settlements"),
            Self::Carrier => ("fleet carriers", "--include-carriers"),
            Self::NotAStarport => ("outposts and other berths", "--station-types"),
            Self::TooFarFromTheStar => ("beyond --max-star-distance", "--max-star-distance"),
            Self::BeyondTheRadius => ("outside the radius", "--radius"),
        }
    }
}

/// Every reason, in report order.
const REASONS: [Reason; 5] = [
    Reason::Settlement,
    Reason::Carrier,
    Reason::NotAStarport,
    Reason::TooFarFromTheStar,
    Reason::BeyondTheRadius,
];

/// A station type that is an on-foot settlement.
///
/// Matched on the prefix because the Odyssey types are a family —
/// `OnFootSettlement` is what Ardent reports today, and the game has shipped
/// several spellings of the same idea. A settlement admitted by a spelling
/// nobody anticipated costs a request and then contributes a route that cannot
/// be flown, which is the expensive direction to be wrong in.
#[must_use]
pub fn is_settlement(station_type: Option<&str>) -> bool {
    station_type.is_some_and(|kind| {
        let kind = kind.to_ascii_lowercase();
        kind.starts_with("onfoot") || kind.contains("settlement")
    })
}

/// Apply the pre-CAPI filters.
///
/// `centre` is the enumeration's reference point; stations are placed by
/// [`crate::ardent::place`] before they get here, so a station whose system
/// coordinates were never filled carries `NaN` and fails the radius test — the
/// direction that costs an answer rather than a wrong one.
#[must_use]
pub fn select(stations: Vec<ArdentStation>, config: &RouteConfig, centre: &Coordinates) -> Selection {
    let considered = stations.len();
    let mut removed = [0usize; REASONS.len()];
    let mut keep = Vec::with_capacity(considered);

    for station in stations {
        match reject(&station, config, centre) {
            Some(reason) => {
                let index = REASONS.iter().position(|candidate| *candidate == reason);
                if let Some(index) = index {
                    removed[index] += 1;
                }
            }
            None => keep.push(station),
        }
    }

    let exclusions = REASONS
        .iter()
        .zip(removed)
        .filter(|(_, count)| *count > 0)
        .map(|(reason, count)| {
            let (label, keep_with) = reason.describe();
            Exclusion { label, removed: count, keep_with }
        })
        .collect();

    Selection { keep, exclusions, considered }
}

/// The first reason this station is not worth a request, or `None` to keep it.
///
/// The order is the report order, and it is also the honest one: a fleet
/// carrier parked at an on-foot settlement's system is reported as a carrier
/// because that is why it was dropped first. Only one reason is ever counted
/// per station, so the exclusion lines sum to exactly what was removed.
fn reject(station: &ArdentStation, config: &RouteConfig, centre: &Coordinates) -> Option<Reason> {
    let kind = station.station_type.as_deref();

    if !config.include_settlements && is_settlement(kind) {
        return Some(Reason::Settlement);
    }
    if !config.include_carriers && is_carrier(kind) {
        return Some(Reason::Carrier);
    }
    if !allowed_type(kind, config) {
        return Some(Reason::NotAStarport);
    }
    if let Some(limit) = config.max_star_distance_ls {
        // An unreported arrival distance is kept. Ardent omits it for a
        // minority of rows, and dropping those would silently narrow the search
        // on a missing field rather than on a measured one; the travel model
        // then charges its own default for the supercruise leg.
        if station.distance_to_arrival.is_some_and(|ls| ls > limit) {
            return Some(Reason::TooFarFromTheStar);
        }
    }
    // Written through `partial_cmp` because the incomparable case is the point:
    // an unplaced station's separation is `NaN`, which is neither inside the
    // radius nor outside it, and must be treated as outside.
    let separation = separation_ly(centre, &station.coordinates);
    if !matches!(
        separation.partial_cmp(&config.radius_ly),
        Some(Ordering::Less | Ordering::Equal)
    ) {
        return Some(Reason::BeyondTheRadius);
    }
    None
}

/// The station-type test, honouring `--station-types` when it was given.
///
/// `--pad` deliberately does **not** consult Ardent's `maxLandingPadSize`: it
/// claims Large for 30 of Sol's 46 on-foot settlements. What `--pad` selects is
/// the *type* set, which is the field that is actually reliable — so `--pad M`
/// widens the default set to include outposts rather than trusting a number.
fn allowed_type(kind: Option<&str>, config: &RouteConfig) -> bool {
    if let Some(allowed) = &config.station_types {
        return kind.is_some_and(|kind| {
            allowed.iter().any(|wanted| wanted.eq_ignore_ascii_case(kind))
        });
    }
    match config.pad {
        Pad::Large => is_starport(kind),
        // A medium or small ship can use an outpost, so the set widens rather
        // than the filter loosening.
        Pad::Medium | Pad::Small => {
            is_starport(kind) || kind.is_some_and(|kind| kind.eq_ignore_ascii_case("Outpost"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::access::{Cli, EnvSnapshot};
    use crate::cli::config::route_config;
    use crate::cli::{Table, parse_with};

    fn config(argv: &[&str]) -> RouteConfig {
        let owned: Vec<String> = argv.iter().map(|s| (*s).to_owned()).collect();
        let parsed = parse_with(&owned, Table::Extended).expect("parses");
        let env = EnvSnapshot::empty();
        route_config(&Cli::new(&parsed, &env)).expect("configures")
    }

    fn at(kind: &str, x: f64) -> ArdentStation {
        ArdentStation {
            market_id: x,
            station_name: format!("{kind} {x}"),
            system_name: "Somewhere".to_owned(),
            station_type: Some(kind.to_owned()),
            max_landing_pad_size: Some(3.0),
            distance_to_arrival: Some(100.0),
            coordinates: Coordinates { x, y: 0.0, z: 0.0 },
        }
    }

    const ORIGIN: Coordinates = Coordinates { x: 0.0, y: 0.0, z: 0.0 };

    /// The headline: settlements and carriers go, starports stay, and every
    /// removal is named with the flag that undoes it.
    #[test]
    fn the_defaults_keep_starports_and_name_what_they_dropped() {
        let stations = vec![
            at("Coriolis", 1.0),
            at("OnFootSettlement", 2.0),
            at("FleetCarrier", 3.0),
            at("Outpost", 4.0),
            at("PlanetaryPort", 5.0),
        ];

        let selection = select(stations, &config(&["route", "Sol"]), &ORIGIN);

        assert_eq!(selection.keep.len(), 2);
        assert_eq!(selection.considered, 5);
        let named: Vec<(&str, usize, &str)> = selection
            .exclusions
            .iter()
            .map(|e| (e.label, e.removed, e.keep_with))
            .collect();
        assert_eq!(
            named,
            vec![
                ("Odyssey settlements", 1, "--settlements"),
                ("fleet carriers", 1, "--include-carriers"),
                ("outposts and other berths", 1, "--station-types"),
            ]
        );
    }

    /// The exclusion counts must sum to exactly what was removed, or the plan
    /// table's arithmetic does not close and the user cannot check it.
    #[test]
    fn the_exclusions_account_for_every_removal() {
        let stations = vec![
            at("Coriolis", 1.0),
            at("OnFootSettlement", 2.0),
            at("FleetCarrier", 3.0),
            at("Outpost", 4.0),
            at("Coriolis", 999.0),
        ];
        let selection = select(stations, &config(&["route", "Sol", "--radius", "30"]), &ORIGIN);

        let dropped: usize = selection.exclusions.iter().map(|e| e.removed).sum();
        assert_eq!(dropped + selection.keep.len(), selection.considered);
    }

    /// `--pad M` widens the type set rather than trusting Ardent's pad column,
    /// which claims Large for thirty of Sol's forty-six settlements.
    #[test]
    fn a_smaller_pad_widens_the_set_it_does_not_trust_the_pad_column() {
        let stations = vec![at("Outpost", 1.0), at("OnFootSettlement", 2.0)];

        let large = select(stations.clone(), &config(&["route", "Sol"]), &ORIGIN);
        assert!(large.keep.is_empty(), "an outpost cannot berth a large ship");

        let medium = select(stations, &config(&["route", "Sol", "--pad", "M"]), &ORIGIN);
        assert_eq!(medium.keep.len(), 1, "the outpost is now usable");
        // Still not the settlement: `--pad` is about berths, `--settlements`
        // is about a different claim entirely.
        assert_eq!(medium.keep[0].station_type.as_deref(), Some("Outpost"));
    }

    /// A station whose coordinates were never filled in must fail the radius
    /// test. `NaN` fails every comparison, which is why the test is written as
    /// a negated `<=` rather than a `>`.
    #[test]
    fn an_unplaced_station_is_excluded_rather_than_placed_at_the_origin() {
        let mut station = at("Coriolis", 1.0);
        station.coordinates = Coordinates { x: f64::NAN, y: f64::NAN, z: f64::NAN };

        let selection = select(vec![station], &config(&["route", "Sol"]), &ORIGIN);

        assert!(selection.keep.is_empty());
        assert_eq!(selection.exclusions[0].label, "outside the radius");
    }

    /// A missing arrival distance is not a reason to drop a market: that would
    /// narrow the search on an absent field rather than a measured one.
    #[test]
    fn an_unreported_arrival_distance_is_kept() {
        let mut near = at("Coriolis", 1.0);
        near.distance_to_arrival = None;
        let mut far = at("Coriolis", 2.0);
        far.distance_to_arrival = Some(500_000.0);

        let selection = select(vec![near, far], &config(&["route", "Sol"]), &ORIGIN);

        assert_eq!(selection.keep.len(), 1);
        assert_eq!(selection.exclusions[0].label, "beyond --max-star-distance");
    }

    /// `--station-types` replaces the default set outright, including the
    /// starport test — a user who names a set means that set.
    #[test]
    fn an_explicit_type_list_replaces_the_default() {
        let stations = vec![at("Coriolis", 1.0), at("Outpost", 2.0), at("MegaShip", 3.0)];

        let selection =
            select(stations, &config(&["route", "Sol", "--station-types", "outpost,megaship"]), &ORIGIN);

        assert_eq!(selection.keep.len(), 2);
        assert_eq!(selection.keep[0].station_type.as_deref(), Some("Outpost"));
    }

    /// Only one reason is counted per station, and it is the first that
    /// applies — so a carrier is reported as a carrier even when it also sits
    /// outside the radius.
    #[test]
    fn a_station_is_counted_once_under_its_first_reason() {
        let selection = select(vec![at("FleetCarrier", 999.0)], &config(&["route", "Sol"]), &ORIGIN);

        assert_eq!(selection.exclusions.len(), 1);
        assert_eq!(selection.exclusions[0].label, "fleet carriers");
    }

    /// The settlement test matches the family, not one spelling. A settlement
    /// admitted by an unanticipated name costs a request and then offers a
    /// route that cannot be flown.
    #[test]
    fn the_settlement_test_covers_the_family() {
        for kind in ["OnFootSettlement", "onfootsettlement", "SurfaceSettlement", "OnFoot"] {
            assert!(is_settlement(Some(kind)), "{kind}");
        }
        for kind in ["Coriolis", "Outpost", "PlanetaryPort", "CraterPort"] {
            assert!(!is_settlement(Some(kind)), "{kind}");
        }
        assert!(!is_settlement(None), "an unreported type is not a settlement");
    }
}
