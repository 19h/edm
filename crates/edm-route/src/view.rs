//! Rendering a result, with the claims that qualify it.
//!
//! This lives in the optimiser's own crate rather than beside the port's other
//! tables, because the rule it enforces is the optimiser's: **a rate may not be
//! printed without its [`Guarantee`], and a `Guarantee` may not be printed
//! without its [`Caveat`]s.** [`Route::rate`] is the only way to reach the
//! number and it hands back all three together; putting the renderer anywhere
//! else would mean re-deriving that pairing at the call site, which is exactly
//! the place it would eventually be forgotten.
//!
//! `f64` is legal here and nowhere in the solving path. Everything below is
//! display: the numbers have already been decided, exactly, over the integers.

use edm_core::js;
use edm_core::render::{Block, Row, columns};

use crate::model::{Commodities, Market};

use crate::num::Ratio;
use crate::report::{Caveat, Guarantee, Route, RouteKind};
use crate::weight::Limiter;

/// The header a shape's table carries.
#[must_use]
pub fn title(kind: RouteKind) -> &'static str {
    match kind {
        RouteKind::SingleHop => "BEST SINGLE HOPS",
        RouteKind::RoundTrip => "BEST ROUND TRIPS",
        RouteKind::Loop { .. } => "BEST REPEATABLE LOOPS",
    }
}

/// One shape's ranking.
///
/// Empty input yields a note rather than an empty frame: "no route" and "the
/// table did not render" must not look the same.
#[must_use]
pub fn ranking(
    kind: RouteKind,
    routes: &[Route],
    markets: &[Market],
    commodities: &Commodities,
) -> Vec<Block<'static>> {
    if routes.is_empty() {
        return vec![
            Block::Heading(title(kind).to_owned()),
            Block::Note(format!("no {} in this data", noun(kind))),
        ];
    }

    let rows = routes
        .iter()
        .enumerate()
        .map(|(index, route)| {
            let rate = route.rate();
            Row::Data(vec![
                js::format_integer((index + 1) as f64).into(),
                stops(route, markets).into(),
                cargo(route, commodities).into(),
                money(route.profit.0).into(),
                // A single hop has no steady state at all — repeating it means
                // flying back empty — so its rate cell is a dash rather than a
                // number that quietly assumes a free return leg.
                rate.steady.map_or_else(|| "—".to_owned(), per_hour).into(),
                duration(route.cycle_millis.0).into(),
                claim(rate.guarantee).into(),
            ])
        })
        .collect();

    let mut blocks = vec![Block::Table {
        title: title(kind).to_owned(),
        columns: columns::ROUTE_COLUMNS,
        rows,
    }];

    // One line per distinct caveat across the whole ranking, not per route:
    // they are properties of the model, and repeating them under every row
    // would train the reader to skip them.
    for caveat in distinct_caveats(routes) {
        blocks.push(Block::Note(explain(caveat).to_owned()));
    }
    blocks
}

/// Every leg of one route, for `--detail`.
#[must_use]
pub fn legs(route: &Route, markets: &[Market], commodities: &Commodities) -> Vec<Block<'static>> {
    let rows = route
        .legs
        .iter()
        .map(|leg| {
            let choice = &leg.choice;
            Row::Data(vec![
                name(markets, leg.to).into(),
                commodities
                    .name(choice.commodity)
                    .unwrap_or("(unknown)")
                    .to_owned()
                    .into(),
                js::format_integer(choice.units.0 as f64).into(),
                money(choice.buy_price.0).into(),
                money(choice.sell_price.0).into(),
                money(choice.profit.0).into(),
                limiter(choice.limiter).to_owned().into(),
                format!("{} Ly", js::to_fixed_1(leg.distance_ly)).into(),
                duration(leg.millis.0).into(),
            ])
        })
        .collect();

    // The origin is in the title rather than a column: the legs are in flying
    // order, so every other station appears exactly once as a destination.
    let start = route.legs.first().map_or_else(|| "?".to_owned(), |leg| name(markets, leg.from));
    vec![Block::Table {
        title: format!("LEGS  from {start}"),
        columns: columns::LEG_COLUMNS,
        rows,
    }]
}

/// The stations in flying order, **station names only**.
///
/// A cycle's last destination is its first origin, so it is not repeated: a
/// round trip reads `A > B` and a four-stop loop `A > B > C > D`, both of which
/// return to `A` by definition. Printing the closing stop spent the width twice
/// on the name a reader already has.
///
/// The system is deliberately absent. `Isherwood Works (FF Andromedae)` is
/// forty-six characters for one stop, and at region scale the systems are
/// procedural names like `Piscium Sector JH-V b2-4` that no reader recognises
/// and that pushed the destination off the end of the cell. `--detail` and
/// `--json` carry them.
fn stops(route: &Route, markets: &[Market]) -> String {
    // Every leg's *origin*, which for a cycle is exactly the set of stops with
    // the closing repeat left off.
    route
        .legs
        .iter()
        .map(|leg| station(markets, leg.from))
        .collect::<Vec<_>>()
        .join(" > ")
}

/// What is carried, in the same order as the stops.
///
/// The single most actionable fact about a route and it was reachable only
/// through `--detail`. A round trip necessarily carries two different
/// commodities — a station's buy price always exceeds its sell price, so for
/// any one commodity at most one direction of a pair can pay — so this is never
/// the same word twice for a 2-cycle.
fn cargo(route: &Route, commodities: &Commodities) -> String {
    route
        .legs
        .iter()
        .map(|leg| readable(commodities.name(leg.choice.commodity).unwrap_or("?")))
        .collect::<Vec<_>>()
        .join(" > ")
}

/// `AgronomicTreatment` as `Agronomic Treatment`.
///
/// The Companion API returns the internal identifier; the game shows the spaced
/// form, and so does every third-party tool a commander has ever used. Left
/// unsplit, a truncated cell reads `AgronomicTr~`, which is both longer and
/// harder to recognise than `Agronomic Tr~`.
///
/// Only ASCII case is examined, which is all these identifiers contain, and a
/// name that is already spaced is returned unchanged.
fn readable(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut previous = '\0';
    for current in name.chars() {
        if current.is_ascii_uppercase()
            && previous.is_ascii_lowercase()
            && !out.ends_with(' ')
        {
            out.push(' ');
        }
        out.push(current);
        previous = current;
    }
    out
}

/// A station's name, without its system.
fn station(markets: &[Market], index: u32) -> String {
    markets
        .get(index as usize)
        .map_or_else(|| "(unknown)".to_owned(), |market| market.station.clone())
}

/// A station's name with its system, for the leg table and the empty case.
fn name(markets: &[Market], index: u32) -> String {
    markets.get(index as usize).map_or_else(
        || "(unknown)".to_owned(),
        |market| format!("{} ({})", market.station, market.system),
    )
}

fn money(credits: i64) -> String {
    format!("{} cr", js::format_integer(credits as f64))
}

/// Credits per hour, floored — the rounding law says every quantisation moves
/// the reported rate down, so the number is never an overstatement.
fn per_hour(rate: Ratio) -> String {
    format!("{}/h", js::format_integer(rate.credits_per_hour_floor() as f64))
}

fn duration(millis: i64) -> String {
    let seconds = millis / 1_000;
    if seconds < 60 {
        return format!("{}s", js::format_integer(seconds as f64));
    }
    format!(
        "{}m {}s",
        js::format_integer((seconds / 60) as f64),
        js::format_integer((seconds % 60) as f64)
    )
}

/// What the search claims. Short enough for a column, and never softer than the
/// truth.
fn claim(guarantee: Guarantee) -> String {
    match guarantee {
        Guarantee::ProvedOptimal => "proved optimal".to_owned(),
        Guarantee::OptimalForStartingCredits => "optimal at start credits".to_owned(),
        // The bound is named, not merely announced: "within a bound" alone
        // tells a reader nothing they can act on, and the number says how much
        // room the search could not rule out.
        Guarantee::BoundedGap { upper } => {
            format!("<= {}/h possible", js::format_integer(upper.credits_per_hour_floor() as f64))
        }
        Guarantee::Heuristic { .. } => "best found".to_owned(),
    }
}

fn limiter(limiter: Limiter) -> &'static str {
    match limiter {
        Limiter::Cargo => "hold full",
        Limiter::Stock => "stock",
        Limiter::Demand => "demand",
        Limiter::Credits => "credits",
    }
}

fn noun(kind: RouteKind) -> &'static str {
    match kind {
        RouteKind::SingleHop => "profitable hop",
        RouteKind::RoundTrip => "profitable round trip",
        RouteKind::Loop { .. } => "repeatable loop",
    }
}

/// Every caveat that appears anywhere in the ranking, in a stable order.
fn distinct_caveats(routes: &[Route]) -> Vec<Caveat> {
    let mut seen: Vec<Caveat> = Vec::new();
    for route in routes {
        for caveat in route.rate().caveats {
            if !seen.contains(caveat) {
                seen.push(*caveat);
            }
        }
    }
    seen
}

/// What a caveat means, in the terms a commander would use.
///
/// Each one names a way the *model* is narrower than the game — never a doubt
/// about the search, which is what `Guarantee` is for.
fn explain(caveat: Caveat) -> &'static str {
    match caveat {
        Caveat::StockDepletion => {
            "stock depletes as you fly this; a second lap may carry less than the first"
        }
        Caveat::DemandUnpublished => {
            "at least one destination publishes no quantity, so its demand is assumed unbounded"
        }
        // Not "from the cache": the caveat is that prices age, which is true
        // of a listing read a second ago. Saying "cache" here contradicted the
        // coverage block's "read live during this run" two lines above it.
        Caveat::StaleListing => {
            "prices were read at one instant and are already ageing; another commander may have \
             traded them since"
        }
        Caveat::JumpGraphUnmodelled => {
            "distances are straight lines; the real jump graph may be longer"
        }
        Caveat::CreditCapBinds => {
            "your balance limits at least one leg, so the route improves as you get richer"
        }
        Caveat::SingleHopNotRepeatable => {
            "a single hop has no steady rate: flying it repeatedly means returning empty"
        }
        Caveat::AccessUnmodelled => {
            "permits, allegiance and landing-pad availability are not modelled"
        }
        Caveat::TimeModelAssumed => {
            "times come from a calibrated model, not from your ship"
        }
        Caveat::EdgesBelowFloorDropped => {
            "legs below --min-profit were excluded; the result is optimal for what remained"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shape_with_no_route_says_so_rather_than_drawing_an_empty_frame() {
        let blocks = ranking(RouteKind::RoundTrip, &[], &[], &Commodities::new());
        assert!(matches!(blocks.last(), Some(Block::Note(text)) if text.contains("no profitable round trip")));
    }

    /// The claim column may never read softer or harder than the guarantee.
    #[test]
    fn every_guarantee_has_its_own_words() {
        let claims = [
            claim(Guarantee::ProvedOptimal),
            claim(Guarantee::OptimalForStartingCredits),
            claim(Guarantee::BoundedGap { upper: Ratio::new(crate::num::Credits(1), crate::num::Millis(1)) }),
            claim(Guarantee::Heuristic { reason: crate::report::HeuristicReason::NodesCapped }),
        ];
        let mut sorted = claims.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), claims.len(), "two guarantees print the same words");
        assert!(claims[0].contains("proved"));
        assert!(!claims[3].contains("proved"), "a heuristic must not read as a proof");
    }

    /// A caveat may not contradict the coverage block, which says in the same
    /// output that every price was read live during this run.
    #[test]
    fn the_staleness_caveat_does_not_claim_the_cache_was_used() {
        let text = explain(Caveat::StaleListing);
        assert!(!text.contains("cache"), "{text}");
        assert!(text.contains("ageing"), "{text}");
    }

    /// Every caveat is explained. A caveat with no words is a caveat nobody
    /// acts on.
    #[test]
    fn every_caveat_is_explained_in_plain_terms() {
        for caveat in [
            Caveat::StockDepletion,
            Caveat::DemandUnpublished,
            Caveat::StaleListing,
            Caveat::JumpGraphUnmodelled,
            Caveat::CreditCapBinds,
            Caveat::SingleHopNotRepeatable,
            Caveat::AccessUnmodelled,
            Caveat::TimeModelAssumed,
            Caveat::EdgesBelowFloorDropped,
        ] {
            let text = explain(caveat);
            assert!(text.len() > 20, "{caveat:?}: {text}");
            assert!(!text.contains("Caveat"), "{caveat:?} names its own enum");
        }
    }

    /// Where to go and what to carry, both legible, neither behind a flag.
    /// The first live radius-100 run printed `Isherwood Works (FF Andromedae)
    /// -> Cassidy Beacon (Piscium Sector JH-V b~` and no commodity at all.
    #[test]
    fn a_route_row_says_where_to_go_and_what_to_carry() {
        let route = crate::fixture::proved_round_trip();
        let markets = crate::fixture::round_trip_markets();
        let commodities = crate::fixture::round_trip_commodities();

        // The closing stop is not repeated: a cycle returns to its origin by
        // definition, and printing it spent the width twice on one name.
        assert_eq!(stops(&route, &markets), "Station 1 > Station 2");
        // Two legs, two commodities, in flying order — and necessarily
        // different, because a station's buy price exceeds its sell price.
        let carried = cargo(&route, &commodities);
        assert!(carried.contains(" > "), "{carried}");
        let names: Vec<&str> = carried.split(" > ").collect();
        assert_eq!(names.len(), 2, "{carried}");
        assert_ne!(names[0], names[1], "a round trip cannot carry the same thing both ways");
    }

    /// The API's identifier is not the name a commander knows.
    #[test]
    fn a_commodity_reads_the_way_the_game_spells_it() {
        assert_eq!(readable("AgronomicTreatment"), "Agronomic Treatment");
        assert_eq!(readable("Gold"), "Gold");
        assert_eq!(readable("Superconductors"), "Superconductors");
        assert_eq!(readable("LowTemperatureDiamond"), "Low Temperature Diamond");
        // Already spaced, and single letters, are left alone.
        assert_eq!(readable("Agronomic Treatment"), "Agronomic Treatment");
        assert_eq!(readable("CMMComposite"), "CMMComposite");
        assert_eq!(readable(""), "");
    }

    #[test]
    fn durations_read_as_durations() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59_999), "59s");
        assert_eq!(duration(60_000), "1m 0s");
        assert_eq!(duration(3_725_000), "62m 5s");
    }
}
