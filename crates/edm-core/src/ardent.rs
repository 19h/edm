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
}
