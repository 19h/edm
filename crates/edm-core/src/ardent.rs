//! Ardent Insight lookups — names to system addresses, and market ids back to
//! names.
//!
//! The TypeScript reaches this API by `import()`ing a module from a sibling
//! project at runtime (`/models/dev/edtrade/src/ardent.ts`, overridable with
//! `ARDENT_MODULE`) and duck-typing four of its exports. Rust cannot import
//! TypeScript, so those four are ported here — **C1**. That turns a hidden
//! runtime coupling into a checked one: `cargo xtask ardent-contract` executes
//! the original module under Bun and diffs both URL builders and both parsers
//! against these over a shared corpus.
//!
//! `ARDENT_MODULE` is consequently accepted and ignored; `EDM_ARDENT_BASE`
//! repoints the API instead.
//!
//! Only the pure half lives here. Fetching is the binary's job.
//!
//! The enumeration half — `nearby_url`, `system_markets_url` and their parsers
//! — has no TypeScript counterpart at all: it exists for `edm route` \[C25\],
//! and `xtask ardent-contract` therefore covers the four ported exports above
//! and nothing below them. Its constants come from measurement against the live
//! API rather than from Ardent's documentation, which does not state them.

use crate::js::json::{JsObject, JsValue};
use crate::js::{self, text};

use super::domain::id64::Coordinates;

/// `encodeURIComponent`.
///
/// Not `percent-encoding`'s defaults and not a URL builder's: the unreserved
/// set is exactly `A-Za-z0-9-_.!~*'()`, the hex digits are uppercase, and it
/// encodes UTF-8 bytes. Getting the set wrong changes the URL for any station
/// with a space, an apostrophe or a hyphen in its name — which is most of
/// them. R80.
#[must_use]
pub fn encode_uri_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(char::from(byte)),
            _ => {
                out.push('%');
                out.push(char::from(HEX[(byte >> 4) as usize]));
                out.push(char::from(HEX[(byte & 0xf) as usize]));
            }
        }
    }
    out
}

/// `ardent.ts:79` — `systemUrl`.
#[must_use]
pub fn system_url(base: &str, system_name: &str) -> String {
    format!("{base}/system/name/{}", encode_uri_component(system_name))
}

/// `ardent.ts:94` — `stationSearchUrl`. Matches on prefix, so a caller must
/// disambiguate the results itself.
#[must_use]
pub fn station_search_url(base: &str, station_name: &str) -> String {
    format!("{base}/search/station/name/{}", encode_uri_component(station_name))
}

/// The route that maps a bare market id to the names EDDN requires.
///
/// Not one of `ardent.ts`'s exports — `market-request.ts:27` defines it
/// separately, and it is a different route from that module's
/// `marketCommodityUrl`.
#[must_use]
pub fn market_url(base: &str, market_id: f64) -> String {
    format!("{base}/market/{}", js::js_number(market_id))
}

/// A system as Ardent reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReferenceSystem {
    pub name: String,
    pub address: f64,
    pub coordinates: Coordinates,
}

/// `ardent.ts:114` — `parseSystem`. Every field is required and every number
/// must be finite; anything else is `None` rather than a partial result.
#[must_use]
pub fn parse_system(value: &JsValue) -> Option<ReferenceSystem> {
    let record = value.as_record()?;
    let name = record.get("systemName")?.as_str()?;
    Some(ReferenceSystem {
        name: name.to_owned(),
        address: finite(record, "systemAddress")?,
        coordinates: Coordinates {
            x: finite(record, "systemX")?,
            y: finite(record, "systemY")?,
            z: finite(record, "systemZ")?,
        },
    })
}

fn finite(record: &JsObject, key: &str) -> Option<f64> {
    record.get(key)?.as_f64().filter(|n| n.is_finite())
}

/// One hit from a station name search.
#[derive(Clone, Debug, PartialEq)]
pub struct StationMatch {
    pub station_name: String,
    pub system_name: String,
    pub station_type: Option<String>,
    /// `maxLandingPadSize`. Read, and then ignored by the API's own filters.
    pub pad: Option<f64>,
}

/// `ardent.ts:166` — `parseStationMatches`. A row missing either name is
/// skipped, not fatal.
#[must_use]
pub fn parse_station_matches(value: &JsValue) -> Vec<StationMatch> {
    let Some(rows) = value.as_array() else { return Vec::new() };
    rows.iter()
        .filter_map(|row| {
            let record = row.as_record()?;
            Some(StationMatch {
                station_name: record.get("stationName")?.as_str()?.to_owned(),
                system_name: record.get("systemName")?.as_str()?.to_owned(),
                station_type: record.get("stationType").and_then(JsValue::as_str).map(str::to_owned),
                pad: finite(record, "maxLandingPadSize"),
            })
        })
        .collect()
}

/// Picks one station out of a prefix search (ts:2496).
///
/// An exact name match wins outright. Failing that, several hits are tolerated
/// as long as they are all in the *same* system — the caller only wants the
/// system, so which berth it picks does not matter. Only a spread across
/// several systems is ambiguous enough to refuse.
pub fn choose_station<'a>(
    matches: &'a [StationMatch],
    name: &str,
) -> Result<&'a StationMatch, String> {
    if matches.is_empty() {
        return Err(format!("Ardent found no system or station matching \"{name}\""));
    }

    let wanted = name.to_lowercase();
    let exact: Vec<&StationMatch> =
        matches.iter().filter(|m| m.station_name.to_lowercase() == wanted).collect();
    let chosen: Vec<&StationMatch> =
        if exact.is_empty() { matches.iter().collect() } else { exact };

    if chosen.len() > 1 {
        let mut systems: Vec<&str> = chosen.iter().map(|m| m.system_name.as_str()).collect();
        systems.sort_unstable();
        systems.dedup();
        if systems.len() > 1 {
            let listed: Vec<String> = chosen
                .iter()
                .take(6)
                .map(|m| format!("{} ({})", m.station_name, m.system_name))
                .collect();
            return Err(format!(
                "\"{name}\" matches {} stations across {} systems: {}{}",
                chosen.len(),
                systems.len(),
                listed.join(", "),
                if chosen.len() > 6 { ", ..." } else { "" },
            ));
        }
    }
    Ok(chosen[0])
}

/// The station names EDDN needs, read out of `/v2/market/{id}` (ts:2974).
///
/// Returns `None` for any failure at all — a transport error, a missing record,
/// or an empty name — because the caller reports the same "Ardent does not know
/// market X" either way. R81.
#[must_use]
pub fn parse_market_station(value: &JsValue) -> Option<(String, String, Option<String>)> {
    let record = value.as_record()?;
    let system_name = record.get("systemName").and_then(JsValue::as_str).unwrap_or("");
    let station_name = record.get("stationName").and_then(JsValue::as_str).unwrap_or("");
    if system_name.is_empty() || station_name.is_empty() {
        return None;
    }
    let station_type = record.get("stationType").and_then(JsValue::as_str).unwrap_or("");
    Some((
        system_name.to_owned(),
        station_name.to_owned(),
        (!station_type.is_empty()).then(|| station_type.to_owned()),
    ))
}

/// Whether a name should be looked up as a system, a station, or either.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// `--station` was given: skip the direct system lookup entirely.
    Station,
    /// `--system` was given: a miss is fatal rather than falling back.
    System,
    /// A bare positional: try the system first, then station search.
    Auto,
}

/// The message for a `--system` lookup that found nothing.
#[must_use]
pub fn unknown_system(name: &str) -> String {
    format!("Ardent does not know a system called \"{name}\"")
}

/// The message for a station whose system Ardent cannot resolve.
#[must_use]
pub fn unknown_station_system(station: &str, system: &str) -> String {
    format!("Ardent knows station {station} but not its system {system}")
}

/// Trims a user-supplied name the way the accessors do before it reaches a URL.
#[must_use]
pub fn normalise_name(name: &str) -> &str {
    text::js_trim(name)
}

// ---------------------------------------------------------------------------
// Enumeration — no TypeScript counterpart; `edm route` only
// ---------------------------------------------------------------------------

/// The largest `maxDistance` Ardent honours.
///
/// A wider request is not refused and is not reported: the server clamps
/// silently, so the answer describes a smaller ball than the one asked for and
/// nothing in the response says which. Anything claiming completeness above
/// this is claiming something the API never offered.
pub const ARDENT_MAX_DISTANCE_LY: f64 = 500.0;

/// The row cap on `/nearby`.
///
/// Measured 2026-08-05: `Sol?maxDistance=600` answers with exactly 1,000 rows
/// reaching 46 Ly. The rows are sorted by ascending `distance`, which is what
/// turns a full page from "some systems are missing" into the far stronger
/// "every system out to the last row is present".
pub const NEARBY_ROW_CAP: usize = 1000;

/// How far a full page's completeness claim must be pulled in.
///
/// `distance` is `round(true distance)` in whole light years — measured over
/// all 1,000 rows around Sol with zero mismatches. The server takes the 1,000
/// smallest by that *integer* key, so every system whose true distance is below
/// `d_max - 0.5` has a key of at most `d_max - 1` and cannot have been cut (it
/// would have displaced a `d_max` row). Systems at exactly `d_max` may or may
/// not have made it. Half a light year is therefore the exact amount of the
/// last shell that the sort order does not vouch for — and it is why the
/// completeness bound is computed from the *reported* integer rather than from
/// the recomputed separation, whose maximum ran 0.44 Ly beyond it in the same
/// sample and would have overclaimed by that much.
pub const NEARBY_ROUNDING_SLACK_LY: f64 = 0.5;

/// `/system/name/{s}/nearby?maxDistance=R` — every system Ardent knows inside
/// the radius, nearest first, capped at [`NEARBY_ROW_CAP`] rows and clamped at
/// [`ARDENT_MAX_DISTANCE_LY`].
///
/// The radius is encoded rather than interpolated because `js_number` renders
/// large values in exponent form (`1e+21`), and a bare `+` in a query string
/// means a space.
#[must_use]
pub fn nearby_url(base: &str, system_name: &str, max_distance: f64) -> String {
    format!(
        "{base}/system/name/{}/nearby?maxDistance={}",
        encode_uri_component(system_name),
        encode_uri_component(&js::js_number(max_distance)),
    )
}

/// `/system/name/{s}/markets` — every station in one system with a commodity
/// market. Uncapped, unpaginated, CDN-fronted and free, which is what makes it
/// affordable to run once per system before spending anything on the Companion
/// API.
#[must_use]
pub fn system_markets_url(base: &str, system_name: &str) -> String {
    format!("{base}/system/name/{}/markets", encode_uri_component(system_name))
}

/// One row of a `/nearby` answer.
#[derive(Clone, Debug, PartialEq)]
pub struct NearbySystem {
    pub name: String,
    pub address: f64,
    pub coordinates: Coordinates,
    /// The `distance` column, exactly as Ardent reports it: whole light years,
    /// rounded, and radial from the queried system.
    ///
    /// It is kept for one purpose only — it is the key the server sorts and
    /// caps on, so it is the only quantity that can say how far a full page's
    /// completeness reaches. It is never a geometric quantity: the separation
    /// of two rows cannot be derived from two rounded radii at all, and even
    /// this row's own distance is wrong by up to half a light year. Use
    /// [`separation_ly`] for anything that measures. (`edm`'s enumeration
    /// re-bases the field onto its own centre as an exact separation; see
    /// `route::discover::Enumeration`.)
    pub distance: f64,
}

/// A `/nearby` answer: the rows that parsed, and how many the server sent.
///
/// Both numbers are needed. The second is what decides whether the row cap
/// bound, and a row this parser rejected must not be allowed to make a full
/// page look like a short one — that would turn a truncated enumeration into a
/// claim of completeness, which is the one failure the caller exists to avoid.
#[derive(Clone, Debug, PartialEq)]
pub struct NearbyPage {
    pub systems: Vec<NearbySystem>,
    pub rows: usize,
}

/// Parses a `/nearby` answer, keeping the count of rows the server actually
/// sent.
#[must_use]
pub fn parse_nearby_page(value: &JsValue) -> NearbyPage {
    let Some(rows) = value.as_array() else { return NearbyPage { systems: Vec::new(), rows: 0 } };
    let systems = rows
        .iter()
        .filter_map(|row| {
            let record = row.as_record()?;
            Some(NearbySystem {
                name: record.get("systemName")?.as_str()?.to_owned(),
                address: finite(record, "systemAddress")?,
                coordinates: Coordinates {
                    x: finite(record, "systemX")?,
                    y: finite(record, "systemY")?,
                    z: finite(record, "systemZ")?,
                },
                distance: finite(record, "distance")?,
            })
        })
        .collect();
    NearbyPage { systems, rows: rows.len() }
}

/// Parses a `/nearby` answer. A row missing any field is skipped, not fatal.
///
/// Callers that reason about the row cap want [`parse_nearby_page`] instead.
#[must_use]
pub fn parse_nearby(value: &JsValue) -> Vec<NearbySystem> {
    parse_nearby_page(value).systems
}

/// One station from a `/system/name/{s}/markets` answer.
#[derive(Clone, Debug, PartialEq)]
pub struct ArdentStation {
    pub market_id: f64,
    pub station_name: String,
    pub system_name: String,
    /// Frontier's stable 64-bit system address.
    ///
    /// **Not present in Ardent's `/markets` row.** Like the coordinates, the
    /// caller fills it from the system enumeration with [`place`]. Keeping it
    /// beside the coordinates prevents the optimiser from collapsing every
    /// market into the synthetic address zero.
    pub system_address: f64,
    /// Ardent's `stationType`. The only field here worth filtering on — see
    /// [`is_starport`].
    pub station_type: Option<String>,
    /// `maxLandingPadSize`, and **advisory only**.
    ///
    /// Ardent reports 3 (Large) for 30 of the 46 on-foot settlements in Sol,
    /// which cannot berth a large ship at all. Reading it as a pad filter would
    /// admit hundreds of unlandable rows per region and spend a Companion API
    /// request on each. Filter on [`station_type`](Self::station_type); use
    /// this to break ties, or to warn.
    pub max_landing_pad_size: Option<f64>,
    /// Light seconds from the arrival star, for the supercruise term of the
    /// travel model.
    pub distance_to_arrival: Option<f64>,
    /// Where the station's system sits.
    ///
    /// **Not present in the payload.** Measured 2026-08-05, a `/markets` row
    /// carries thirteen keys and no coordinates whatsoever, so this parser
    /// leaves it `NaN` and the caller fills it from the enumeration that
    /// produced the system — which already holds them — with [`place`]. `NaN`
    /// rather than the origin because the origin *is* Sol: an unfilled station
    /// must fail every distance test rather than quietly pass the wrong one.
    pub coordinates: Coordinates,
}

/// Assigns one system's coordinates to the stations parsed out of it.
///
/// See [`ArdentStation::coordinates`] for why this is a separate step and not
/// something the parser can do.
pub fn place(stations: &mut [ArdentStation], system_address: f64, coordinates: Coordinates) {
    for station in stations {
        station.system_address = system_address;
        station.coordinates = coordinates;
    }
}

/// Parses a `/system/name/{s}/markets` answer. A row missing an id or either
/// name is skipped.
#[must_use]
pub fn parse_system_markets(value: &JsValue) -> Vec<ArdentStation> {
    let Some(rows) = value.as_array() else { return Vec::new() };
    rows.iter()
        .filter_map(|row| {
            let record = row.as_record()?;
            Some(ArdentStation {
                market_id: finite(record, "marketId")?,
                station_name: record.get("stationName")?.as_str()?.to_owned(),
                system_name: record.get("systemName")?.as_str()?.to_owned(),
                system_address: f64::NAN,
                station_type: record.get("stationType").and_then(JsValue::as_str).map(str::to_owned),
                max_landing_pad_size: finite(record, "maxLandingPadSize"),
                distance_to_arrival: finite(record, "distanceToArrival"),
                coordinates: Coordinates { x: f64::NAN, y: f64::NAN, z: f64::NAN },
            })
        })
        .collect()
}

/// The station types that can berth a large ship **and** carry a real commodity
/// market.
///
/// Settlements and outposts can do neither, and they are most of what Ardent
/// calls a station: 46 of Sol's 62 rows are on-foot settlements, and near
/// Colonia 58% are fleet carriers. Filtering to these seven removes 87-93% of
/// the Companion API spend for a region — but that is the smaller half of the
/// argument. Excluding a berth a large ship cannot use is *correctness*; the
/// saving is a consequence.
pub const STARPORT_TYPES: [&str; 7] =
    ["Coriolis", "Orbis", "Ocellus", "AsteroidBase", "CraterPort", "PlanetaryPort", "MegaShip"];

/// Whether a station type is in [`STARPORT_TYPES`].
///
/// Compared case-insensitively: Ardent, EDDN and the Companion API each spell
/// these consistently and differently from each other, and a filter that
/// silently dropped every station because of a capital letter would look
/// exactly like a sparse region.
#[must_use]
pub fn is_starport(station_type: Option<&str>) -> bool {
    station_type
        .is_some_and(|kind| STARPORT_TYPES.iter().any(|known| known.eq_ignore_ascii_case(kind)))
}

/// Whether a station is a fleet carrier — excluded by default, because its
/// prices are one commander's whim and it may not be there tomorrow.
#[must_use]
pub fn is_carrier(station_type: Option<&str>) -> bool {
    station_type.is_some_and(|kind| kind.eq_ignore_ascii_case("FleetCarrier"))
}

/// Straight-line separation in light years.
///
/// Every distance this program acts on is computed here, from coordinates.
/// An API `distance` field is rounded to whole light years and radial from one
/// reference point, so the separation between two rows cannot be recovered from
/// them even in principle.
#[must_use]
pub fn separation_ly(from: &Coordinates, to: &Coordinates) -> f64 {
    let dx = from.x - to.x;
    let dy = from.y - to.y;
    let dz = from.z - to.z;
    dz.mul_add(dz, dx.mul_add(dx, dy * dy)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encoding_covers_the_names_that_actually_occur() {
        assert_eq!(encode_uri_component("Jaques Station"), "Jaques%20Station");
        assert_eq!(
            encode_uri_component("Hyades Sector NI-X a16-0"),
            "Hyades%20Sector%20NI-X%20a16-0"
        );
        // The unreserved set is wider than a URL crate's default.
        assert_eq!(encode_uri_component("a-_.!~*'()"), "a-_.!~*'()");
        assert_eq!(encode_uri_component("/?&=#+"), "%2F%3F%26%3D%23%2B");
        assert_eq!(encode_uri_component("Böthold"), "B%C3%B6thold");
    }

    fn station(name: &str, system: &str) -> StationMatch {
        StationMatch {
            station_name: name.to_owned(),
            system_name: system.to_owned(),
            station_type: None,
            pad: None,
        }
    }

    #[test]
    fn an_exact_name_beats_a_longer_prefix_hit() {
        let matches = [station("Jaques Station Alpha", "Colonia"), station("Jaques", "Eol Prou")];
        assert_eq!(choose_station(&matches, "jaques").unwrap().system_name, "Eol Prou");
    }

    /// Several berths in one system are not ambiguous, because only the system
    /// is wanted.
    #[test]
    fn several_hits_in_one_system_resolve() {
        let matches = [station("Ohm City", "Colonia"), station("Ohm Depot", "Colonia")];
        assert_eq!(choose_station(&matches, "ohm").unwrap().station_name, "Ohm City");
    }

    #[test]
    fn hits_spread_across_systems_are_refused() {
        let matches = [station("Ohm City", "Colonia"), station("Ohm Depot", "Sol")];
        let error = choose_station(&matches, "ohm").unwrap_err();
        assert_eq!(
            error,
            "\"ohm\" matches 2 stations across 2 systems: Ohm City (Colonia), Ohm Depot (Sol)"
        );
    }

    #[test]
    fn enumeration_urls_encode_both_the_name_and_the_radius() {
        assert_eq!(
            nearby_url("http://a", "Hyades Sector NI-X a16-0", 12.5),
            "http://a/system/name/Hyades%20Sector%20NI-X%20a16-0/nearby?maxDistance=12.5"
        );
        // `1e+21` in a query string would otherwise arrive as `1e 21`.
        assert_eq!(
            nearby_url("http://a", "Sol", 1e21),
            "http://a/system/name/Sol/nearby?maxDistance=1e%2B21"
        );
        assert_eq!(
            system_markets_url("http://a", "Jaques' Rest"),
            "http://a/system/name/Jaques'%20Rest/markets"
        );
    }

    /// The row count is the server's, not the parser's — otherwise a rejected
    /// row turns a truncated page into a claim of completeness.
    #[test]
    fn a_rejected_row_still_counts_toward_the_page_size() {
        let value = JsValue::parse(
            r#"[{"systemName":"Sol","systemAddress":10477373803,"systemX":0,"systemY":0,"systemZ":0,"distance":0},
                {"systemName":"Nowhere"},
                {"systemName":"Broken","systemAddress":1,"systemX":null,"systemY":0,"systemZ":0,"distance":2}]"#,
        )
        .expect("valid JSON");
        let page = parse_nearby_page(&value);
        assert_eq!(page.rows, 3);
        assert_eq!(page.systems.len(), 1);
        assert_eq!(page.systems[0].name, "Sol");
        assert_eq!(parse_nearby(&value).len(), 1);
    }

    #[test]
    fn a_body_that_is_not_an_array_is_an_empty_page() {
        let value = JsValue::parse(r#"{"error":"not found"}"#).expect("valid JSON");
        assert_eq!(parse_nearby_page(&value), NearbyPage { systems: Vec::new(), rows: 0 });
        assert!(parse_system_markets(&value).is_empty());
    }

    /// A verbatim `/system/name/Sol/markets` row, minus the fields nothing
    /// reads. It carries no coordinates, and its pad size is a fiction.
    #[test]
    fn a_market_row_arrives_without_coordinates() {
        let value = JsValue::parse(
            r#"[{"systemAddress":10477373803,"systemName":"Sol","marketId":3802401536,
                 "stationName":"Abimbola Metallurgic Reserve","stationType":"OnFootSettlement",
                 "distanceToArrival":9142.916182,"maxLandingPadSize":3}]"#,
        )
        .expect("valid JSON");
        let mut stations = parse_system_markets(&value);
        assert_eq!(stations.len(), 1);
        assert_eq!(stations[0].market_id, 3_802_401_536.0);
        assert_eq!(stations[0].max_landing_pad_size, Some(3.0));
        assert!(stations[0].coordinates.x.is_nan());

        let sol = Coordinates { x: 0.0, y: 0.0, z: 0.0 };
        assert!(separation_ly(&sol, &stations[0].coordinates).is_nan());
        place(&mut stations, 10_477_373_803.0, sol);
        assert_eq!(separation_ly(&sol, &stations[0].coordinates), 0.0);
    }

    /// Ardent says this settlement has a large pad. It does not have one, and
    /// the type is what says so.
    #[test]
    fn the_station_filter_reads_the_type_and_not_the_pad() {
        assert!(!is_starport(Some("OnFootSettlement")));
        assert!(!is_starport(Some("CraterOutpost")));
        assert!(!is_starport(Some("Outpost")));
        assert!(!is_starport(None));
        assert!(is_starport(Some("Orbis")));
        assert!(is_starport(Some("coriolis")));
        assert!(is_carrier(Some("FleetCarrier")));
        assert!(!is_carrier(Some("MegaShip")));
    }

    /// Barnard's Star sits 5.9547 Ly from Sol and Ardent's column says 6. Two
    /// such columns cannot be subtracted into anything.
    #[test]
    fn separation_is_recomputed_because_the_reported_column_is_rounded() {
        let sol = Coordinates { x: 0.0, y: 0.0, z: 0.0 };
        let barnards = Coordinates { x: -3.03125, y: 1.375, z: 4.9375 };
        let separation = separation_ly(&sol, &barnards);
        assert!((separation - 5.954_663).abs() < 1e-6, "{}", js::js_number(separation));
        assert_eq!(js::js_round(separation), 6.0);

        assert_eq!(separation_ly(&barnards, &sol), separation);
        assert_eq!(separation_ly(&barnards, &barnards), 0.0);
    }
}
