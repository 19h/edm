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
use crate::watch::Event;
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
    ranking_with(kind, routes, markets, commodities, false, None)
}

/// The ranking table, optionally with the credits-per-hour column \[C39\].
#[must_use]
pub fn ranking_with(
    kind: RouteKind,
    routes: &[Route],
    markets: &[Market],
    commodities: &Commodities,
    show_rate: bool,
    origin: Option<edm_core::domain::id64::Coordinates>,
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
            let mut cells: Vec<std::borrow::Cow<'static, str>> = vec![
                js::format_integer((index + 1) as f64).into(),
                stops(route, markets).into(),
                cargo(route, commodities).into(),
                money(route.profit.0).into(),
                quantities(route, markets).into(),
                approach(route, markets, origin).into(),
                js::to_fixed_1(route.legs.iter().map(|leg| leg.distance_ly).sum()).into(),
            ];
            if show_rate {
                // A single hop has no steady state — repeating it means flying
                // back empty — so it has only a first-lap rate.
                cells.push(per_hour(rate.steady.unwrap_or(rate.first_lap)).into());
            }
            cells.push(duration(route.cycle_millis.0).into());
            cells.push(claim(rate.guarantee).into());
            Row::Data(cells)
        })
        .collect();

    let mut blocks = vec![Block::Table {
        title: title(kind).to_owned(),
        columns: if show_rate {
            columns::ROUTE_COLUMNS_WITH_RATE
        } else {
            columns::ROUTE_COLUMNS
        },
        rows,
    }];
    // The ordering key is not on screen and `Profit` over `Lap` does not
    // reproduce it: a single hop is ranked over its *first* lap, which charges
    // the supercruise in to the starting station, where `Lap` is the cycle and
    // charges none. Two rows can therefore show the same `Lap`, and the one
    // with less profit can still rank higher — which reads as a broken sort
    // unless the note says where the missing time went. Naming `To start`
    // matters for the same reason \[C40\]: it is the one number on screen that
    // looks like it belongs in a rate and is deliberately not in this one.
    // The objective is read off the routes rather than passed in: it is a
    // property of the search that produced them, and the key already carries it
    // so the heap could honour it \[C47\].
    let by_profit = routes
        .first()
        .is_some_and(|route| route.rank.objective == crate::time::Objective::Profit);
    if by_profit {
        blocks.push(Block::Note(
            "ordered by credits per run, with travel time ignored (--by-profit). Ly and To start \
             are shown but do not affect the order, so a row far away can outrank a nearer one \
             that pays less"
                .to_owned(),
        ));
    } else if !show_rate {
        blocks.push(Block::Note(
            "ordered by credits per hour on the first lap, which --per-hour shows: that clock \
             starts on approach to the first station, where Lap does not, so two rows can share \
             a Lap and still rank apart. To start is not in it — it is paid once, and a rate is \
             per lap"
                .to_owned(),
        ));
    }

    // One line per distinct caveat across the whole ranking, not per route:
    // they are properties of the model, and repeating them under every row
    // would train the reader to skip them.
    for caveat in distinct_caveats(routes) {
        blocks.push(Block::Note(explain(caveat).to_owned()));
    }
    blocks
}

/// What the seller has, against what the buyer will take, for the leg that
/// binds the route \[C39\].
///
/// One pair rather than one per leg, because the table has one row per route:
/// the binding leg is the one whose units are smallest, which is the leg that
/// decided the profit. For a single hop that is the only leg.
///
/// A destination that publishes a bracket and no tonnage reads `?` rather than
/// a number — the optimiser assumes a full hold there, and printing that
/// assumption as if it were a measurement is what the `demand unpublished`
/// caveat exists to prevent.
fn quantities(route: &Route, markets: &[Market]) -> String {
    let Some(leg) = route
        .legs
        .iter()
        .min_by_key(|leg| leg.choice.units.0)
    else {
        return "-".to_owned();
    };
    let stock = markets
        .get(leg.from as usize)
        .and_then(|market| {
            market
                .supply
                .iter()
                .find(|row| row.commodity == leg.choice.commodity)
        })
        .map_or_else(|| "?".to_owned(), |row| js::format_integer(row.stock.0 as f64));
    let demand = markets
        .get(leg.to as usize)
        .and_then(|market| {
            market
                .demand
                .iter()
                .find(|row| row.commodity == leg.choice.commodity)
        })
        .map_or_else(
            || "?".to_owned(),
            |row| match row.qty {
                crate::model::DemandQty::Published(tons) => js::format_integer(tons.0 as f64),
                crate::model::DemandQty::Unpublished => "?".to_owned(),
            },
        );
    format!("{stock}/{demand}")
}

/// How far the ship is from the market this route starts at \[C40\].
///
/// `-` when the origin is unknown, which is honest: the model has never
/// included the approach in the rate, and inventing a zero would say the route
/// begins where you are standing.
fn approach(
    route: &Route,
    markets: &[Market],
    origin: Option<edm_core::domain::id64::Coordinates>,
) -> String {
    let Some(origin) = origin else {
        return "-".to_owned();
    };
    let Some(start) = route
        .legs
        .first()
        .and_then(|leg| markets.get(leg.from as usize))
    else {
        return "-".to_owned();
    };
    js::to_fixed_1(crate::time::distance_ly(origin, start.coords))
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
                format!(
                    "{}{}",
                    money(choice.sell_price.0),
                    if choice.bulk_estimated { " (est.)" } else { "" },
                )
                .into(),
                money(choice.profit.0).into(),
                limiter(choice.limiter).to_owned().into(),
                format!("{} Ly", js::to_fixed_1(leg.distance_ly)).into(),
                duration(leg.millis.0).into(),
            ])
        })
        .collect();

    // The origin is in the title rather than a column: the legs are in flying
    // order, so every other station appears exactly once as a destination.
    let start = route
        .legs
        .first()
        .map_or_else(|| "?".to_owned(), |leg| name(markets, leg.from));
    vec![Block::Table {
        title: format!("LEGS  from {start}"),
        columns: columns::LEG_COLUMNS,
        rows,
    }]
}

/// One line of search progress.
///
/// Returned, not printed: this crate writes to nothing. The caller decides
/// whether a line is worth showing, which is also where the throttling has to
/// live, because only the caller knows how long the run has been silent.
///
/// The rate is the point of the round line. A search that will take five
/// minutes is bearable if the number in front of you is climbing and unbearable
/// if it is a spinner, and the number is real: every rate reported is a rate
/// some cycle in the data actually earns.
#[must_use]
pub fn progress(event: Event) -> String {
    let n = |value: usize| js::format_integer(value as f64);
    match event {
        Event::Building { done, total, edges } => format!(
            "  building the trade graph: {} / {} commodities, {} legs",
            n(done),
            n(total),
            n(edges)
        ),
        Event::Round {
            round, stops: 0, ..
        } => {
            format!("  loop search round {}: no route yet", n(round as usize))
        }
        Event::Round { round, rate, stops } => format!(
            "  loop search round {}: best so far {} over {} stops",
            n(round as usize),
            per_hour(rate),
            n(stops)
        ),
        Event::Expanded { paths, budget } => format!(
            "  loop search: {} of {} partial routes explored",
            js::format_integer(paths as f64),
            js::format_integer(budget as f64)
        ),
        Event::Abandoned => {
            "  the search ran out of time; reporting the best route it had, unproved".to_owned()
        }
    }
}

/// The stations in flying order, **with their systems**.
///
/// A cycle's last destination is its first origin, so it is not repeated: a
/// round trip reads `A > B` and a four-stop loop `A > B > C > D`, both of which
/// return to `A` by definition. Printing the closing stop spent the width twice
/// on the name a reader already has.
///
/// **A single hop is not a cycle**, so its destination is printed. Leaving it
/// off — which is what dropping the closing stop did to every open route —
/// produced twenty rows that all read `Isherwood Works` and never said where to
/// fly, the one fact a single hop consists of.
///
/// The system is here because you cannot plot a course to a station name. It
/// was dropped once to buy width for the `Cargo` column and that was the wrong
/// trade: a procedural name like `Piscium Sector JH-V b2-4` is unrecognisable
/// but it is also the only thing the galaxy map accepts. The cell squeezes
/// instead.
///
/// A stop in the same system as the one before it does not repeat the system —
/// `Daedalus (Sol) > Galileo` — because at that point the system is not new
/// information and the width is better spent on the next station.
fn stops(route: &Route, markets: &[Market]) -> String {
    // Every leg's *origin*, which for a cycle is exactly the set of stops with
    // the closing repeat left off.
    let mut indices: Vec<u32> = route.legs.iter().map(|leg| leg.from).collect();
    if !route.kind.is_cycle()
        && let Some(last) = route.legs.last()
    {
        indices.push(last.to);
    }

    let mut out = String::new();
    let mut previous: Option<&str> = None;
    for (position, index) in indices.iter().enumerate() {
        if position > 0 {
            out.push_str(" > ");
        }
        let Some(market) = markets.get(*index as usize) else {
            out.push_str("(unknown)");
            continue;
        };
        out.push_str(&market.station);
        if previous != Some(market.system.as_str()) {
            out.push_str(" (");
            out.push_str(&market.system);
            out.push(')');
        }
        previous = Some(market.system.as_str());
    }
    out
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
/// The game-internal API returns the internal identifier; the game shows the spaced
/// form, and so does every third-party tool a commander has ever used. Left
/// unsplit, a truncated cell reads `AgronomicTr~`, which is both longer and
/// harder to recognise than `Agronomic Tr~`.
///
/// Only ASCII case is examined, which is all these identifiers contain, and a
/// name that is already spaced is returned unchanged.
pub fn readable(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (index, current) in chars.iter().enumerate() {
        let previous = if index == 0 { '\0' } else { chars[index - 1] };
        // `agronomicTreatment` -> a word boundary after a lowercase run, and
        // `CMMComposite` -> one after an acronym, which is the case the first
        // rule alone misses. The game writes both with the space.
        let after_word = current.is_ascii_uppercase() && previous.is_ascii_lowercase();
        let after_acronym = current.is_ascii_uppercase()
            && previous.is_ascii_uppercase()
            && chars.get(index + 1).is_some_and(char::is_ascii_lowercase);
        if (after_word || after_acronym) && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push(*current);
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

pub fn money(credits: i64) -> String {
    format!("{} cr", js::format_integer(credits as f64))
}

/// Credits per hour, floored — the rounding law says every quantisation moves
/// the reported rate down, so the number is never an overstatement.
pub fn per_hour(rate: Ratio) -> String {
    format!(
        "{}/h",
        js::format_integer(rate.credits_per_hour_floor() as f64)
    )
}

pub fn duration(millis: i64) -> String {
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
pub fn claim(guarantee: Guarantee) -> String {
    match guarantee {
        Guarantee::ProvedOptimal => "proved optimal".to_owned(),
        Guarantee::OptimalForStartingCredits => "optimal at start credits".to_owned(),
        // The bound is named, not merely announced: "within a bound" alone
        // tells a reader nothing they can act on, and the number says how much
        // room the search could not rule out.
        Guarantee::BoundedGap { upper } => {
            format!(
                "<= {}/h possible",
                js::format_integer(upper.credits_per_hour_floor() as f64)
            )
        }
        // A rescored route *was* measured — what narrowed is the ordering
        // claim, not the price. Collapsing it into "best found" would report
        // the stronger fact as the weaker one.
        Guarantee::Heuristic {
            reason: crate::report::HeuristicReason::RescoredAfterSearch,
        } => "verified, best of these".to_owned(),
        Guarantee::Heuristic { .. } => "best found".to_owned(),
    }
}

pub fn limiter(limiter: Limiter) -> &'static str {
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
pub fn explain(caveat: Caveat) -> &'static str {
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
            "market observations are ageing independently; another commander may have traded them since"
        }
        Caveat::BulkPriceEstimated => {
            "destination prices use an empirical cargo-quantity estimate, not an executable quote"
        }
        Caveat::PricedFromCache => {
            "at least one price here was reused from the local cache, not read during this run"
        }
        Caveat::CarrierSourceDoesNotRestock => {
            "a leg buys from a fleet carrier: that stock was put there by a commander and does not restock, so the rate describes a lap you can fly once"
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
        Caveat::TimeModelAssumed => "times come from a calibrated model, not from your ship",
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
        assert!(
            matches!(blocks.last(), Some(Block::Note(text)) if text.contains("no profitable round trip"))
        );
    }

    /// The claim column may never read softer or harder than the guarantee.
    #[test]
    fn every_guarantee_has_its_own_words() {
        let claims = [
            claim(Guarantee::ProvedOptimal),
            claim(Guarantee::OptimalForStartingCredits),
            claim(Guarantee::BoundedGap {
                upper: Ratio::new(crate::num::Credits(1), crate::num::Millis(1)),
            }),
            claim(Guarantee::Heuristic {
                reason: crate::report::HeuristicReason::NodesCapped,
            }),
        ];
        let mut sorted = claims.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            claims.len(),
            "two guarantees print the same words"
        );
        assert!(claims[0].contains("proved"));
        assert!(
            !claims[3].contains("proved"),
            "a heuristic must not read as a proof"
        );
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
            Caveat::BulkPriceEstimated,
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
        assert_eq!(
            stops(&route, &markets),
            "Station 1 (System 1) > Station 2 (System 2)"
        );
        // Two legs, two commodities, in flying order — and necessarily
        // different, because a station's buy price exceeds its sell price.
        let carried = cargo(&route, &commodities);
        assert!(carried.contains(" > "), "{carried}");
        let names: Vec<&str> = carried.split(" > ").collect();
        assert_eq!(names.len(), 2, "{carried}");
        assert_ne!(
            names[0], names[1],
            "a round trip cannot carry the same thing both ways"
        );
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
        // An acronym followed by a word, which the game writes with the space.
        assert_eq!(readable("CMMComposite"), "CMM Composite");
        assert_eq!(readable("HNShockMount"), "HN Shock Mount");
        assert_eq!(readable(""), "");
    }

    #[test]
    fn a_progress_line_says_what_was_done_and_never_what_is_coming() {
        let lines = [
            progress(Event::Building {
                done: 47,
                total: 99,
                edges: 8_123_456,
            }),
            progress(Event::Round {
                round: 3,
                rate: Ratio {
                    credits: 1,
                    millis: 1,
                },
                stops: 4,
            }),
            progress(Event::Round {
                round: 1,
                rate: Ratio::ZERO,
                stops: 0,
            }),
            progress(Event::Expanded {
                paths: 4_096,
                budget: 20_000_000,
            }),
            progress(Event::Abandoned),
        ];
        assert!(
            lines[0].contains("47 / 99 commodities, 8,123,456 legs"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("round 3") && lines[1].contains("3,600,000/h"),
            "{}",
            lines[1]
        );
        // A round with no witness has no rate to quote, and must not print one.
        assert!(!lines[2].contains("/h"), "{}", lines[2]);
        assert!(lines[3].contains("4,096 of 20,000,000"), "{}", lines[3]);
        // The abandoned line has to withdraw the claim in words, because the
        // Claim column only has room for "best found".
        assert!(lines[4].contains("unproved"), "{}", lines[4]);
        for line in &lines {
            assert!(
                !line.contains("estimated") && !line.contains("remaining"),
                "{line}"
            );
        }
    }

    #[test]
    fn durations_read_as_durations() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59_999), "59s");
        assert_eq!(duration(60_000), "1m 0s");
        assert_eq!(duration(3_725_000), "62m 5s");
    }
}

/// Ready-to-run `edm trade` invocations for every route in the ranking.
///
/// The ranking names stations; the trade command wants a **market id**, and
/// nothing else in the output carries one. Without this the answer stops one
/// step short of being usable: you know where to go and what to carry, and then
/// have to go and look up two numbers by hand.
///
/// The commodity goes on the wire as the game-internal API's own identifier rather
/// than the spaced form the table shows, because [`domain::find_commodity`]
/// strips whitespace and lowercases before matching and takes an **exact** hit
/// over a partial one — so `Tantalum` can only ever resolve to Tantalum, while
/// a partial like `gold` is ambiguous against `LowTemperatureDiamond`-style
/// names in a way that depends on what the market happens to stock.
///
/// `--fill --cargo N` for a buy when the hold size is known, because that is
/// robust to stock having moved since the sweep; an exact `--qty` for a sell,
/// because you sell what you are actually carrying.
#[must_use]
pub fn trade_commands(
    routes: &[Route],
    markets: &[Market],
    commodities: &Commodities,
    cargo: Option<i64>,
) -> Vec<Block<'static>> {
    if routes.is_empty() {
        return Vec::new();
    }

    let mut blocks = vec![Block::Heading("TRADE COMMANDS".to_owned())];
    blocks.push(Block::Note(
        "run each where it says; --top controls how many routes are listed here".to_owned(),
    ));

    for (index, route) in routes.iter().enumerate() {
        blocks.push(Block::Raw(format!(
            "\n{:>3}  {}",
            js::format_integer((index + 1) as f64),
            stops(route, markets),
        )));

        for leg in &route.legs {
            let item = commodities.name(leg.choice.commodity).unwrap_or("?");
            let units = plain(leg.choice.units.0);

            let buy = match cargo {
                Some(hold) => format!("--type buy --item {item} --fill --cargo {}", plain(hold)),
                None => format!("--type buy --item {item} --qty {units}"),
            };
            blocks.push(Block::Raw(format!(
                "       at {:<28} edm trade --market-id {} {buy}",
                station(markets, leg.from),
                id(markets, leg.from),
            )));
            blocks.push(Block::Raw(format!(
                "       at {:<28} edm trade --market-id {} --type sell --item {item} --qty {units}",
                station(markets, leg.to),
                id(markets, leg.to),
            )));
        }
    }
    blocks
}

/// A station's market id, as the trade command wants it.
///
/// Through `js_number`, **never** `format_integer`: every other number in this
/// module is grouped for reading, and a grouped market id is
/// `--market-id 128,666,762`, which is not a market id. Anything that goes into
/// a command line here is plain decimal for the same reason.
fn id(markets: &[Market], index: u32) -> String {
    markets.get(index as usize).map_or_else(
        || "?".to_owned(),
        |market| js::js_number(market.market_id as f64),
    )
}

/// A quantity as a command line wants it: plain decimal, no grouping.
fn plain(value: i64) -> String {
    js::js_number(value as f64)
}

#[cfg(test)]
mod command_tests {
    use super::*;

    fn rendered() -> String {
        let route = crate::fixture::proved_round_trip();
        let markets = crate::fixture::round_trip_markets();
        let commodities = crate::fixture::round_trip_commodities();
        let blocks = trade_commands(&[route], &markets, &commodities, Some(1232));
        blocks
            .iter()
            .filter_map(|block| match block {
                Block::Raw(text) | Block::Note(text) | Block::Heading(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole point: the ranking names stations, the trade command wants a
    /// market id, and nothing else in the output carries one.
    #[test]
    fn every_leg_gets_a_market_id_and_a_command() {
        let text = rendered();
        assert!(text.contains("--market-id 1 "), "{text}");
        assert!(text.contains("--market-id 2 "), "{text}");
        assert!(text.contains("--type buy"), "{text}");
        assert!(text.contains("--type sell"), "{text}");
    }

    /// A grouped market id is not a market id. This is the one formatting
    /// mistake in this block that produces a command which silently addresses
    /// the wrong market — or, more likely, fails to parse.
    #[test]
    fn nothing_on_a_command_line_is_thousands_separated() {
        let markets = vec![{
            let mut market = crate::fixture::round_trip_markets().remove(0);
            market.market_id = 4_306_502_403;
            market
        }];
        let route = crate::fixture::proved_round_trip();
        let text = trade_commands(
            &[route],
            &markets,
            &crate::fixture::round_trip_commodities(),
            Some(1232),
        )
        .iter()
        .filter_map(|block| match block {
            Block::Raw(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

        assert!(text.contains("--market-id 4306502403"), "{text}");
        for line in text.lines().filter(|line| line.contains("edm trade")) {
            let command = &line[line.find("edm trade").expect("a command")..];
            assert!(
                !command.contains(','),
                "grouped number in a command: {command}"
            );
        }
    }

    /// `--cargo` reaches the buy, because that is the workflow this exists for:
    /// fill the hold, fly, sell it.
    #[test]
    fn a_known_hold_size_becomes_fill_and_cargo() {
        assert!(rendered().contains("--fill --cargo 1232"));
    }

    /// And without one, the buy is sized to exactly what the route assumed,
    /// rather than silently filling a hold whose size nobody stated.
    #[test]
    fn an_unknown_hold_size_buys_the_units_the_route_ranked() {
        let route = crate::fixture::proved_round_trip();
        let units = route.legs[0].choice.units.0;
        let markets = crate::fixture::round_trip_markets();
        let blocks = trade_commands(
            &[route],
            &markets,
            &crate::fixture::round_trip_commodities(),
            None,
        );
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Raw(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains(&format!("--type buy --item gold --qty {units}")),
            "{text}"
        );
        assert!(!text.contains("--fill"), "{text}");
    }

    /// Empty in, empty out — no heading for a ranking that has no routes.
    #[test]
    fn no_routes_means_no_block_at_all() {
        assert!(trade_commands(&[], &[], &Commodities::new(), None).is_empty());
    }
}

#[cfg(test)]
mod open_route_tests {
    use super::*;
    use crate::report::RouteKind;

    /// Twenty single hops that all read `Isherwood Works` and never say where
    /// to fly. Dropping the closing stop is right for a cycle — its last
    /// destination is its first origin — and it silently deleted the
    /// destination of every open route, which is the whole of what a single hop
    /// is.
    #[test]
    fn a_single_hop_says_where_it_goes() {
        let markets = crate::fixture::round_trip_markets();
        let geometry = crate::fixture::geometry(&markets);
        let hop =
            crate::report::Route::single_hop(&geometry, 0, 1, crate::fixture::choice(0, 1_000));
        assert!(!hop.kind.is_cycle());
        assert_eq!(
            stops(&hop, &markets),
            "Station 1 (System 1) > Station 2 (System 2)"
        );
    }

    /// And a cycle still does not repeat the stop it returns to.
    /// Two stations in one system name it once. At that point the system is
    /// not new information and the width is better spent on the next station.
    #[test]
    fn a_second_stop_in_the_same_system_does_not_repeat_it() {
        let mut markets = crate::fixture::round_trip_markets();
        markets[1].system = markets[0].system.clone();
        let route = crate::fixture::proved_round_trip();

        assert_eq!(stops(&route, &markets), "Station 1 (System 1) > Station 2");
    }

    #[test]
    fn a_cycle_does_not_repeat_its_origin() {
        let route = crate::fixture::proved_round_trip();
        assert!(route.kind.is_cycle());
        assert_eq!(
            stops(&route, &crate::fixture::round_trip_markets()),
            "Station 1 (System 1) > Station 2 (System 2)"
        );
    }

    /// A single hop's rate cell shows the first-lap rate, which is the only one
    /// it has and the one the ranking is by. A dash hid the sort key and left
    /// the profit column looking unordered. Under `--per-hour`, where the
    /// column exists at all, that must still hold \[C39\].
    #[test]
    fn a_single_hop_shows_the_rate_it_is_ranked_by() {
        let markets = crate::fixture::round_trip_markets();
        let geometry = crate::fixture::geometry(&markets);
        let hop =
            crate::report::Route::single_hop(&geometry, 0, 1, crate::fixture::choice(0, 1_000));
        let blocks = ranking_with(
            RouteKind::SingleHop,
            std::slice::from_ref(&hop),
            &markets,
            &crate::fixture::round_trip_commodities(),
            true,
            None,
        );
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Table { rows, .. } => Some(
                    rows.iter()
                        .filter_map(|row| match row {
                            Row::Data(cells) => Some(cells.join(" | ")),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !text.contains('—'),
            "no dash where the ranking key belongs: {text}"
        );
        assert!(text.contains("/h"), "{text}");
    }

    /// By default the rate is not on screen, so the table has to say what it is
    /// ordered by — otherwise the profit column reads as unsorted, which is the
    /// exact defect the rate column was originally added to fix \[C39\].
    #[test]
    fn hiding_the_rate_leaves_the_ordering_stated() {
        let markets = crate::fixture::round_trip_markets();
        let geometry = crate::fixture::geometry(&markets);
        let hop =
            crate::report::Route::single_hop(&geometry, 0, 1, crate::fixture::choice(0, 1_000));
        let blocks = ranking(
            RouteKind::SingleHop,
            std::slice::from_ref(&hop),
            &markets,
            &crate::fixture::round_trip_commodities(),
        );
        let notes: Vec<&str> = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Note(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            notes.iter().any(|note| note.contains("ordered by credits per hour")),
            "{notes:?}"
        );
    }

    /// The approach is measured from where the ship is, not from the search
    /// centre, and is absent rather than zero when nobody said \[C40\].
    #[test]
    fn the_approach_is_the_distance_to_the_first_market() {
        use edm_core::domain::id64::Coordinates;

        let markets = crate::fixture::round_trip_markets();
        let geometry = crate::fixture::geometry(&markets);
        let hop =
            crate::report::Route::single_hop(&geometry, 0, 1, crate::fixture::choice(0, 1_000));

        // The fixture puts market 0 at the origin and market 1 eight light
        // years along x, so a ship three light years out is three from the
        // start and not eight.
        let ship_at = Coordinates {
            x: -3.0,
            y: 0.0,
            z: 0.0,
        };
        assert_eq!(approach(&hop, &markets, Some(ship_at)), "3.0");
        assert_eq!(approach(&hop, &markets, None), "-");
    }

    /// The pair that replaced the rate: what the seller has against what the
    /// buyer will take, for the leg that bound the route.
    #[test]
    fn the_quantity_cell_names_both_ends() {
        let markets = crate::fixture::round_trip_markets();
        let geometry = crate::fixture::geometry(&markets);
        let hop =
            crate::report::Route::single_hop(&geometry, 0, 1, crate::fixture::choice(0, 1_000));
        assert_eq!(quantities(&hop, &markets), "5,000/7,000");
    }
}
