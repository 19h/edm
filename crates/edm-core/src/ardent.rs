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

mod categories;
pub use categories::{commodity_category, known_categories, resolve_category};

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
    format!(
        "{base}/search/station/name/{}",
        encode_uri_component(station_name)
    )
}

/// The route that maps a bare market id to the names EDDN requires.
///
/// Not one of `ardent.ts`'s exports — `game-internal-api.ts:27` defines it
/// separately, and it is a different route from that module's
/// `marketCommodityUrl`.
#[must_use]
pub fn market_url(base: &str, market_id: f64) -> String {
    format!("{base}/market/{}", js::js_number(market_id))
}

/// Which half of Ardent's price index to read.
///
/// In the game's terminology an `exports` row is a market that sells cargo to
/// the commander, so it is ordered by the price the commander pays
/// (`buyPrice`). An `imports` row buys cargo from the commander and is ordered
/// by its `sellPrice`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommodityDirection {
    Exports,
    Imports,
}

/// The price index is only a candidate source, but stale candidates waste
/// authenticated Frontier reads. This mirrors Ardent's own route client's
/// seven-day index window; the resulting listing is still verified live.
pub const DEFAULT_COMMODITY_MAX_DAYS_AGO: f64 = 7.0;

impl CommodityDirection {
    /// The endpoint segment Ardent accepts.
    #[must_use]
    pub const fn segment(self) -> &'static str {
        match self {
            Self::Exports => "exports",
            Self::Imports => "imports",
        }
    }

    /// The station's role, for output intended for a commander.
    #[must_use]
    pub const fn market_role(self) -> &'static str {
        match self {
            Self::Exports => "sells",
            Self::Imports => "buys",
        }
    }
}

/// `/system/name/{system}/commodity/name/{commodity}/nearby/{direction}`.
///
/// This is Ardent's deep, price-ordered route. It caps each side at 1,000 rows
/// and silently clamps its radius to 500 Ly, so the client clamps too rather
/// than describing a broader search than the server performed. Server-side
/// volume and carrier filters come **before** that cap; applying either after a
/// page arrived could evict a qualifying price from the answer.
#[must_use]
pub fn commodity_nearby_url(
    base: &str,
    system_name: &str,
    commodity: &str,
    direction: CommodityDirection,
    max_distance_ly: f64,
    include_carriers: bool,
    min_volume: f64,
) -> String {
    // Ardent parses this query with `parseInt`. Round upward before sending so
    // its integer radius encloses the caller's fractional radius; local station
    // selection then keeps the exact requested boundary.
    let distance = js::js_number(js::js_min(max_distance_ly.ceil(), ARDENT_MAX_DISTANCE_LY));
    let mut query = format!("maxDistance={}", encode_uri_component(&distance));
    if !include_carriers {
        query.push_str("&fleetCarriers=false");
    }
    query.push_str("&maxDaysAgo=");
    query.push_str(&encode_uri_component(&js::js_number(
        DEFAULT_COMMODITY_MAX_DAYS_AGO,
    )));
    // Ardent's default is one unit. Keeping that parameter absent makes a
    // default no-cargo lookup use the same compact, documented request as the
    // service's own clients, while a cargo-derived or explicit floor reaches
    // the server before its price cap.
    if min_volume > 1.0 {
        query.push_str("&minVolume=");
        query.push_str(&encode_uri_component(&js::js_number(min_volume)));
    }
    format!(
        "{base}/system/name/{}/commodity/name/{}/nearby/{}?{query}",
        encode_uri_component(system_name),
        encode_uri_component(commodity),
        direction.segment(),
    )
}

/// All rows for one commodity at the reference system itself.
///
/// The nearby price endpoint omits its centre system. This sibling route fills
/// that otherwise invisible zero-Ly hole; callers still select its rows with
/// the same local filters and always verify the eventual listing live.
#[must_use]
pub fn system_commodity_url(base: &str, system_name: &str, commodity: &str) -> String {
    format!(
        "{base}/system/name/{}/commodity/name/{}?maxDaysAgo={}",
        encode_uri_component(system_name),
        encode_uri_component(commodity),
        encode_uri_component(&js::js_number(DEFAULT_COMMODITY_MAX_DAYS_AGO)),
    )
}

/// `/commodities` — every commodity id Ardent indexes.
///
/// One free read that turns a whole class of silent empty answers into a usage
/// error: a name Ardent does not know answers `200 []`, not 404, so a lookup
/// that skips this check spends its per-commodity queries to learn nothing and
/// then reports "no candidates" as though the region were bare.
#[must_use]
pub fn commodities_url(base: &str) -> String {
    format!("{base}/commodities")
}

/// The commodity ids from a `/commodities` response, in the order given.
#[must_use]
pub fn parse_commodity_ids(value: &JsValue) -> Vec<String> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let name = row.as_record()?.get("commodityName")?.as_str()?;
            (!name.is_empty()).then(|| name.to_owned())
        })
        .collect()
}

/// What a `--item` spelling turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The normalised spelling is an id Ardent indexes.
    Exact(String),
    /// It is not, but a near neighbour is, and the difference is only the
    /// plural the in-game display name carries. Reported to the user, because a
    /// lookup that quietly answers about a different id is worse than one that
    /// asks.
    Adjusted(String),
    /// No id matches. The suggestion, when there is one, is the closest id
    /// within an edit distance the user could plausibly have typed.
    Unknown { suggestion: Option<String> },
}

/// Resolve one spelling against Ardent's catalogue.
///
/// Three steps, narrowest first: the exact normalised id, then the singular or
/// plural of it, then a spelling-distance suggestion that is offered and never
/// silently applied.
#[must_use]
pub fn resolve_commodity(wanted: &str, known: &[String]) -> Resolution {
    let id = normalise_commodity_name(wanted);
    if known.contains(&id) {
        return Resolution::Exact(id);
    }
    // The display name is the symbol's plural far more often than anything
    // else: "Low Temperature Diamonds" for `lowtemperaturediamond`, "Void
    // Opals" for `opal`. Try it both ways before giving up.
    for variant in [
        id.strip_suffix('s').map(str::to_owned),
        Some(format!("{id}s")),
    ]
    .into_iter()
    .flatten()
    {
        if known.contains(&variant) {
            return Resolution::Adjusted(variant);
        }
    }
    Resolution::Unknown {
        suggestion: nearest(wanted, &id, known),
    }
}

/// The id most likely to be the one that was meant, or nothing.
///
/// Two rules, and the order matters. Word containment first, because the names
/// that actually diverge diverge by a whole word — "Agri-Medicines" is
/// `agriculturalmedicines` — and a spelling distance cannot see that; it
/// measured "Agri-Medicines" as nearer to `basicmedicines`, which is a
/// different commodity offered with a straight face. Edit distance second, for
/// the ordinary typo.
fn nearest(typed: &str, id: &str, known: &[String]) -> Option<String> {
    // Short tokens match too much to be evidence: "of", "ic" and the like occur
    // in dozens of ids.
    let words: Vec<String> = typed
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect();
    if !words.is_empty() {
        let mut matches = known
            .iter()
            .filter(|candidate| words.iter().all(|word| candidate.contains(word.as_str())));
        // Only when it is the *only* id containing every word. Two matches means
        // the words did not identify a commodity, and guessing between them is
        // how a lookup answers about the wrong cargo.
        if let (Some(single), None) = (matches.next(), matches.next()) {
            return Some(single.clone());
        }
    }
    let limit = std::cmp::max(2, id.len() / 3);
    known
        .iter()
        .map(|candidate| (edit_distance(id, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        // Ties go to the first id in Ardent's own order, so the suggestion for
        // one spelling is the same on every run.
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate.clone())
}

/// Ids that share this spelling's longest word, for an error that can be acted
/// on.
///
/// "Marine Equipment" is `marinesupplies`, and no distance or containment rule
/// reaches that. Naming the handful of ids that do mention marine turns a dead
/// end into a one-line correction.
///
/// The word that matches *fewest* ids wins, not the longest one: "equipment"
/// is the longer half of "Marine Equipment" and the less informative, because
/// four commodities are some kind of equipment and only two are marine.
/// Returns nothing rather than a long list — a wall of ids is the same dead end
/// with more reading.
#[must_use]
pub fn related_commodities(typed: &str, known: &[String], limit: usize) -> Vec<String> {
    typed
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() >= 4)
        .map(|word| {
            let word = word.to_ascii_lowercase();
            known
                .iter()
                .filter(|candidate| candidate.contains(word.as_str()))
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|matches| !matches.is_empty() && matches.len() <= limit)
        .min_by_key(Vec::len)
        .unwrap_or_default()
}

/// Levenshtein distance over bytes.
///
/// Both sides are ASCII by construction: [`normalise_commodity_name`] keeps only
/// ASCII alphanumerics, and Ardent's ids are Frontier symbols.
fn edit_distance(a: &str, b: &str) -> usize {
    let b = b.as_bytes();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, left) in a.bytes().enumerate() {
        current[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != *right);
            current[j + 1] = substitution.min(current[j] + 1).min(previous[j + 1] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
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
    /// The station-scoped id consumed by Frontier's market and vendor routes.
    /// Older or partial Ardent rows may omit it.
    pub market_id: Option<f64>,
    /// `maxLandingPadSize`. Read, and then ignored by the API's own filters.
    pub pad: Option<f64>,
}

/// `ardent.ts:166` — `parseStationMatches`. A row missing either name is
/// skipped, not fatal.
#[must_use]
pub fn parse_station_matches(value: &JsValue) -> Vec<StationMatch> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let record = row.as_record()?;
            Some(StationMatch {
                station_name: record.get("stationName")?.as_str()?.to_owned(),
                system_name: record.get("systemName")?.as_str()?.to_owned(),
                station_type: record
                    .get("stationType")
                    .and_then(JsValue::as_str)
                    .map(str::to_owned),
                market_id: finite(record, "marketId"),
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
        return Err(format!(
            "Ardent found no system or station matching \"{name}\""
        ));
    }

    let wanted = name.to_lowercase();
    let exact: Vec<&StationMatch> = matches
        .iter()
        .filter(|m| m.station_name.to_lowercase() == wanted)
        .collect();
    let chosen: Vec<&StationMatch> = if exact.is_empty() {
        matches.iter().collect()
    } else {
        exact
    };

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
    let system_name = record
        .get("systemName")
        .and_then(JsValue::as_str)
        .unwrap_or("");
    let station_name = record
        .get("stationName")
        .and_then(JsValue::as_str)
        .unwrap_or("");
    if system_name.is_empty() || station_name.is_empty() {
        return None;
    }
    let station_type = record
        .get("stationType")
        .and_then(JsValue::as_str)
        .unwrap_or("");
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

/// Reduce a commodity spelling to the shape of an Ardent commodity id.
///
/// Ardent keys on Frontier's own symbol, lowercased and stripped of everything
/// that is not an ASCII letter or digit — `lowtemperaturediamond`. This is only
/// half of resolving a name: the symbol is **not** the in-game display name, and
/// the difference is more than punctuation. "Low Temperature Diamonds" is the
/// symbol's plural, "Agri-Medicines" is `agriculturalmedicines`, and "Marine
/// Equipment" is `marinesupplies`. Normalising is therefore a candidate, and
/// [`resolve_commodity`] is what decides. Non-ASCII letters are intentionally
/// not transliterated into a different commodity name.
#[must_use]
pub fn normalise_commodity_name(raw: &str) -> String {
    raw.bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|byte| char::from(byte.to_ascii_lowercase()))
        .collect()
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
    format!(
        "{base}/system/name/{}/markets",
        encode_uri_component(system_name)
    )
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
    let Some(rows) = value.as_array() else {
        return NearbyPage {
            systems: Vec::new(),
            rows: 0,
        };
    };
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
    NearbyPage {
        systems,
        rows: rows.len(),
    }
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
    /// admit hundreds of unlandable rows per region and spend a game-internal API
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
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let record = row.as_record()?;
            Some(ArdentStation {
                market_id: finite(record, "marketId")?,
                station_name: record.get("stationName")?.as_str()?.to_owned(),
                system_name: record.get("systemName")?.as_str()?.to_owned(),
                system_address: f64::NAN,
                station_type: record
                    .get("stationType")
                    .and_then(JsValue::as_str)
                    .map(str::to_owned),
                max_landing_pad_size: finite(record, "maxLandingPadSize"),
                distance_to_arrival: finite(record, "distanceToArrival"),
                coordinates: Coordinates {
                    x: f64::NAN,
                    y: f64::NAN,
                    z: f64::NAN,
                },
            })
        })
        .collect()
}

/// One price-ranked row from Ardent's commodity-nearby endpoint.
///
/// Unlike `/markets`, this endpoint carries the system address and coordinates
/// on every row, so its [`station`](Self::station) can go directly into the
/// live-poll and route-ingest paths without a second discovery pass.
#[derive(Clone, Debug, PartialEq)]
pub struct CommodityPrice {
    pub commodity_name: String,
    pub direction: CommodityDirection,
    /// The direction-specific price: `buyPrice` for an export and `sellPrice`
    /// for an import.
    pub price: f64,
    /// The direction-specific advertised quantity: `stock` or `demand`.
    pub volume: f64,
    /// The matching `stockBracket` or `demandBracket`, when Ardent supplied
    /// it. A zero import demand with a positive bracket means Frontier did not
    /// publish an exact quantity, not that the market buys zero cargo.
    pub volume_bracket: Option<f64>,
    pub station: ArdentStation,
}

/// Parse the side of a price-ranked commodity response that was requested.
///
/// A malformed row is skipped rather than made into a partly-known market. A
/// quick lookup's next step is an authenticated read, so an invalid identity
/// must cost no request and must never be collapsed onto id zero. Prices and
/// volumes may be zero here; the caller applies the requested floor. In
/// particular, an import with zero demand and a positive bracket has an
/// unreported quantity, which Ardent also retains despite `minVolume`.
#[must_use]
pub fn parse_commodity_prices(
    value: &JsValue,
    direction: CommodityDirection,
) -> Vec<CommodityPrice> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_992.0;

    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let record = row.as_record()?;
            let commodity_name = record.get("commodityName")?.as_str()?.to_owned();
            if commodity_name.is_empty() {
                return None;
            }
            let market_id = finite(record, "marketId")?;
            let system_address = finite(record, "systemAddress")?;
            if !(1.0..=MAX_SAFE_INTEGER).contains(&market_id)
                || !(1.0..=MAX_SAFE_INTEGER).contains(&system_address)
                || !js::safe_int(market_id)
                || !js::safe_int(system_address)
            {
                return None;
            }
            let station_name = record.get("stationName")?.as_str()?.to_owned();
            let system_name = record.get("systemName")?.as_str()?.to_owned();
            if station_name.is_empty() || system_name.is_empty() {
                return None;
            }
            let (price_key, volume_key, bracket_key) = match direction {
                CommodityDirection::Exports => ("buyPrice", "stock", "stockBracket"),
                CommodityDirection::Imports => ("sellPrice", "demand", "demandBracket"),
            };
            let price = finite(record, price_key)?;
            let volume = finite(record, volume_key)?;
            Some(CommodityPrice {
                commodity_name,
                direction,
                price,
                volume,
                volume_bracket: finite(record, bracket_key),
                station: ArdentStation {
                    market_id,
                    station_name,
                    system_name,
                    system_address,
                    station_type: record
                        .get("stationType")
                        .and_then(JsValue::as_str)
                        .map(str::to_owned),
                    max_landing_pad_size: finite(record, "maxLandingPadSize"),
                    distance_to_arrival: finite(record, "distanceToArrival"),
                    coordinates: Coordinates {
                        x: finite(record, "systemX")?,
                        y: finite(record, "systemY")?,
                        z: finite(record, "systemZ")?,
                    },
                },
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
/// the game-internal API spend for a region — but that is the smaller half of the
/// argument. Excluding a berth a large ship cannot use is *correctness*; the
/// saving is a consequence.
pub const STARPORT_TYPES: [&str; 7] = [
    "Coriolis",
    "Orbis",
    "Ocellus",
    "AsteroidBase",
    "CraterPort",
    "PlanetaryPort",
    "MegaShip",
];

/// Whether a station type is in [`STARPORT_TYPES`].
///
/// Compared case-insensitively: Ardent, EDDN and the game-internal API each spell
/// these consistently and differently from each other, and a filter that
/// silently dropped every station because of a capital letter would look
/// exactly like a sparse region.
#[must_use]
pub fn is_starport(station_type: Option<&str>) -> bool {
    station_type.is_some_and(|kind| {
        STARPORT_TYPES
            .iter()
            .any(|known| known.eq_ignore_ascii_case(kind))
    })
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
            market_id: None,
            pad: None,
        }
    }

    #[test]
    fn an_exact_name_beats_a_longer_prefix_hit() {
        let matches = [
            station("Jaques Station Alpha", "Colonia"),
            station("Jaques", "Eol Prou"),
        ];
        assert_eq!(
            choose_station(&matches, "jaques").unwrap().system_name,
            "Eol Prou"
        );
    }

    /// Several berths in one system are not ambiguous, because only the system
    /// is wanted.
    #[test]
    fn several_hits_in_one_system_resolve() {
        let matches = [
            station("Ohm City", "Colonia"),
            station("Ohm Depot", "Colonia"),
        ];
        assert_eq!(
            choose_station(&matches, "ohm").unwrap().station_name,
            "Ohm City"
        );
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
        assert_eq!(
            parse_nearby_page(&value),
            NearbyPage {
                systems: Vec::new(),
                rows: 0
            }
        );
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

        let sol = Coordinates {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
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
        let sol = Coordinates {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let barnards = Coordinates {
            x: -3.03125,
            y: 1.375,
            z: 4.9375,
        };
        let separation = separation_ly(&sol, &barnards);
        assert!(
            (separation - 5.954_663).abs() < 1e-6,
            "{}",
            js::js_number(separation)
        );
        assert_eq!(js::js_round(separation), 6.0);

        assert_eq!(separation_ly(&barnards, &sol), separation);
        assert_eq!(separation_ly(&barnards, &barnards), 0.0);
    }

    #[test]
    fn quick_lookup_urls_are_encoded_capped_and_server_filtered() {
        // The display name pluralises where the symbol does not; normalising
        // cannot know that, and `resolve_commodity` is what closes the gap.
        assert_eq!(
            normalise_commodity_name("Low Temperature Diamonds"),
            "lowtemperaturediamonds"
        );
        assert_eq!(normalise_commodity_name("Agri-Medicines"), "agrimedicines");
        assert_eq!(
            system_commodity_url("http://a", "Hyades Sector NI-X a16-0", "gold"),
            "http://a/system/name/Hyades%20Sector%20NI-X%20a16-0/commodity/name/gold?maxDaysAgo=7"
        );
        assert_eq!(
            commodity_nearby_url(
                "http://a",
                "Hyades Sector NI-X a16-0",
                "lowtemperaturediamond",
                CommodityDirection::Exports,
                30.0,
                false,
                79.0,
            ),
            "http://a/system/name/Hyades%20Sector%20NI-X%20a16-0/commodity/name/lowtemperaturediamond/nearby/exports?maxDistance=30&fleetCarriers=false&maxDaysAgo=7&minVolume=79"
        );
        assert_eq!(
            commodity_nearby_url(
                "http://a",
                "Sol",
                "gold",
                CommodityDirection::Imports,
                600.0,
                true,
                1.0,
            ),
            "http://a/system/name/Sol/commodity/name/gold/nearby/imports?maxDistance=500&maxDaysAgo=7"
        );
        assert_eq!(
            commodity_nearby_url(
                "http://a",
                "Sol",
                "gold",
                CommodityDirection::Exports,
                30.5,
                true,
                1.0,
            ),
            "http://a/system/name/Sol/commodity/name/gold/nearby/exports?maxDistance=31&maxDaysAgo=7"
        );
    }

    #[test]
    fn a_display_name_is_resolved_against_the_catalogue_and_never_guessed_at() {
        let known = [
            "gold".to_owned(),
            "lowtemperaturediamond".to_owned(),
            "agriculturalmedicines".to_owned(),
            "opal".to_owned(),
        ];
        assert_eq!(
            resolve_commodity("Gold", &known),
            Resolution::Exact("gold".to_owned())
        );
        // The in-game name pluralises the symbol. This is the single most
        // common way a correct-looking --item selects nothing at all.
        assert_eq!(
            resolve_commodity("Low Temperature Diamonds", &known),
            Resolution::Adjusted("lowtemperaturediamond".to_owned())
        );
        // And the other way round, for a symbol that is itself the plural.
        assert_eq!(
            resolve_commodity("opals", &known),
            Resolution::Adjusted("opal".to_owned())
        );
        // A near miss is offered, never applied: "gild" is not gold, and a
        // lookup that decided otherwise would answer a question nobody asked.
        assert_eq!(
            resolve_commodity("gild", &known),
            Resolution::Unknown {
                suggestion: Some("gold".to_owned())
            }
        );
        // "Agri-Medicines" is `agriculturalmedicines`. Edit distance puts it
        // nearer to a different medicine; whole words identify it exactly.
        assert_eq!(
            resolve_commodity("Agri-Medicines", &known),
            Resolution::Unknown {
                suggestion: Some("agriculturalmedicines".to_owned())
            }
        );
        // But only when the words pick out one id. "Medicines" alone does not.
        let ambiguous = [
            "agriculturalmedicines".to_owned(),
            "basicmedicines".to_owned(),
            "nanomedicines".to_owned(),
        ];
        assert_eq!(
            resolve_commodity("medicines", &ambiguous),
            Resolution::Unknown { suggestion: None }
        );
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn a_dead_end_still_names_the_ids_that_mention_the_same_word() {
        let known = [
            "marinesupplies".to_owned(),
            "chieridanimarinepaste".to_owned(),
            "gold".to_owned(),
        ];
        assert_eq!(
            related_commodities("Marine Equipment", &known, 4),
            ["marinesupplies", "chieridanimarinepaste"]
        );
        // Too many is no more useful than none, and much longer to read.
        assert!(related_commodities("Marine Equipment", &known, 1).is_empty());
        // The rarer word decides. Three ids are some kind of paste-or-supply
        // and only one is Chieridani, so that is the half worth printing.
        let mixed = [
            "marinesupplies".to_owned(),
            "chieridanimarinepaste".to_owned(),
            "onionheadaalpha".to_owned(),
        ];
        assert_eq!(
            related_commodities("Chieridani Marine", &mixed, 4),
            ["chieridanimarinepaste"]
        );
        // Nothing shares a word with it, and short words are not evidence.
        assert!(related_commodities("Gild", &known, 4).is_empty());
        assert!(related_commodities("of a", &known, 4).is_empty());
    }

    #[test]
    fn the_catalogue_is_a_list_of_ids_and_a_malformed_row_is_not_one() {
        let value = JsValue::parse(
            r#"[{"commodityName":"gold","avgBuyPrice":9000},{"commodityName":""},{"x":1},
                {"commodityName":"lowtemperaturediamond"}]"#,
        )
        .expect("valid JSON");
        assert_eq!(
            parse_commodity_ids(&value),
            ["gold", "lowtemperaturediamond"]
        );
        assert_eq!(commodities_url("http://a"), "http://a/commodities");
    }

    #[test]
    fn a_price_index_row_becomes_a_placed_station_for_its_requested_side() {
        let value = JsValue::parse(
            r#"[{"commodityName":"gold","marketId":128123384,"stationName":"Jones Estate",
                 "stationType":"Orbis","distanceToArrival":9.5,"maxLandingPadSize":3,
                 "systemAddress":7267755828641,"systemName":"Groombridge 34",
                 "systemX":-9.90625,"systemY":-3.6875,"systemZ":-5.09375,
                 "buyPrice":47000,"sellPrice":66959,"stock":42,"stockBracket":2,"demand":8188,"demandBracket":3},
                {"commodityName":"gold","marketId":0,"stationName":"broken","systemName":"Sol",
                 "systemAddress":1,"systemX":0,"systemY":0,"systemZ":0,"buyPrice":1,"stock":1}]"#,
        )
        .expect("valid JSON");
        let exports = parse_commodity_prices(&value, CommodityDirection::Exports);
        let imports = parse_commodity_prices(&value, CommodityDirection::Imports);
        assert_eq!(exports.len(), 1);
        assert_eq!(imports.len(), 1);
        assert_eq!(exports[0].price, 47_000.0);
        assert_eq!(exports[0].volume, 42.0);
        assert_eq!(imports[0].price, 66_959.0);
        assert_eq!(imports[0].volume, 8_188.0);
        assert_eq!(imports[0].volume_bracket, Some(3.0));
        assert_eq!(imports[0].station.system_name, "Groombridge 34");
        assert_eq!(imports[0].station.coordinates.x, -9.90625);
    }
}
