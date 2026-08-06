//! Lossless parsing and validation for Frontier's daily populated-system digest.
//!
//! `/2.0/elite/starsystem/dailydigest_part` looks like one map of systems, but
//! every response also carries a repeated sparse overlay.  A full observed page
//! contains 4,000 rows from the paginated primary table and 359 overlay rows.
//! Consequently neither the JSON member count nor the number of normalized
//! systems says whether the caller has reached the last page.  [`DigestPage`]
//! keeps those counts separate and [`DigestPage::is_terminal`] considers only
//! primary rows.
//!
//! The endpoint's identifiers are parsed with `serde_json`, not the crate's
//! JavaScript-number model.  In particular, minor-faction identifiers commonly
//! exceed 2^53 and must remain exact `u64` values.

use std::collections::HashSet;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use thiserror::Error;

use super::id64::{self, Coordinates};

/// Number of primary-table slots in a full response.
pub const PRIMARY_PAGE_SIZE: usize = 4_000;

/// A normalized populated-system row.
#[derive(Clone, Debug, PartialEq)]
pub struct DigestSystem {
    pub address: u64,
    /// Galactic coordinates (Sol is `(0, 0, 0)`), rather than the raw
    /// Frontier-grid coordinates carried by the endpoint.
    pub coordinates: Coordinates,
    pub status: DigestStatus,
}

/// Mutable/status data carried beside a system's stable identity and position.
///
/// All fields are optional deliberately.  The digest is a topology source;
/// omission of a status field in a future response must not silently remove a
/// system from that topology.  A present field still has to have the right
/// type, however, or the whole page is rejected.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DigestStatus {
    pub faction_id: Option<u32>,
    /// Kept as `u64`: observed values exceed JavaScript's exact integer range.
    pub minor_faction_id: Option<u64>,
    pub government_id: Option<u32>,
    pub development_level: Option<u32>,
    pub standard_of_living: Option<u32>,
    /// Zero is a legitimate observed population and is not an absence marker.
    pub population: Option<u64>,
    pub system_value: Option<f64>,
    pub tech_level: Option<f64>,
    pub economies: Option<[Option<u32>; 2]>,
    pub state: Option<String>,
    /// An internal database identifier, not a system address.
    pub starsystem_id: Option<String>,
    pub tw_rescue_market: Option<bool>,
    pub old_minor_faction_ids: Option<Vec<u64>>,
    pub power_id: Option<u32>,
    pub power_state: Option<String>,
    pub security_level: Option<u32>,
}

/// One completely validated response page.
#[derive(Clone, Debug, PartialEq)]
pub struct DigestPage {
    /// Real systems only: overlays and the fixed sentinel never appear here.
    pub systems: Vec<DigestSystem>,
    /// Rows drawn from the endpoint's paginated primary table.
    ///
    /// This includes the one recognized sentinel.  The sentinel occupies a
    /// primary-table slot on page zero, so excluding it here would turn that
    /// otherwise-full page into 3,999 rows and terminate a crawl immediately.
    pub primary_rows: usize,
    pub overlay_rows: usize,
    /// A subset of `primary_rows`, excluded from `systems`.
    pub sentinel_rows: usize,
}

impl DigestPage {
    /// Whether this page proves that no subsequent primary-table page exists.
    ///
    /// Overlay count, normalized-system count, and total JSON members are all
    /// intentionally irrelevant.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.primary_rows < PRIMARY_PAGE_SIZE
    }
}

/// Why a page could not be trusted as a complete slice of the digest.
#[derive(Debug, Error)]
pub enum DigestError {
    #[error("invalid daily-digest JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("daily-digest row {key:?} is malformed: {reason}")]
    MalformedRow { key: String, reason: String },
    #[error("daily-digest page contains system address {address} more than once")]
    DuplicateAddress { address: u64 },
}

/// Parse, classify, normalize, and validate one daily-digest response.
///
/// Sparse overlays are ignored only when their shape is exactly the three
/// observed fields.  A partial rich row, a mistyped known status field, an
/// identity mismatch, invalid coordinates, or a duplicate rich address rejects
/// the entire page rather than punching an unnoticed hole in discovery.
pub fn parse_page(document: &str) -> Result<DigestPage, DigestError> {
    let envelope: DigestEnvelope = serde_json::from_str(document)?;
    let mut systems = Vec::new();
    let mut seen = HashSet::new();
    let mut primary_rows = 0;
    let mut overlay_rows = 0;
    let mut sentinel_rows = 0;

    for (key, row) in envelope.systems {
        let identity_fields = IDENTITY_FIELDS
            .iter()
            .filter(|field| row.has(field))
            .count();

        if identity_fields == 0 {
            if !row.has_exact_fields(&OVERLAY_FIELDS) {
                return Err(malformed(
                    key,
                    "a row without identity/coordinates is not the exact sparse overlay",
                ));
            }
            // Checking the key set is not enough: negative, fractional, or
            // otherwise mistyped overlay values are schema drift, not overlays.
            let _: SparseOverlay =
                serde_json::from_value(Value::Object(row.fields)).map_err(|error| {
                    malformed(key.clone(), format!("invalid sparse overlay: {error}"))
                })?;
            overlay_rows += 1;
            continue;
        }

        if identity_fields != IDENTITY_FIELDS.len() {
            return Err(malformed(
                key,
                "identity/coordinate fields are only partially present",
            ));
        }

        let sentinel_shape = row.has_exact_fields(&SENTINEL_FIELDS);
        let wire: DigestWire = serde_json::from_value(Value::Object(row.fields))
            .map_err(|error| malformed(key.clone(), format!("invalid rich row: {error}")))?;

        if sentinel_shape && is_observed_sentinel(&key, &wire) {
            // The sentinel is part of the paginated primary table even though
            // it is not a discoverable system.
            primary_rows += 1;
            sentinel_rows += 1;
            continue;
        }

        let system = normalize_rich(&key, wire)?;
        if !seen.insert(system.address) {
            return Err(DigestError::DuplicateAddress {
                address: system.address,
            });
        }
        primary_rows += 1;
        systems.push(system);
    }

    Ok(DigestPage {
        systems,
        primary_rows,
        overlay_rows,
        sentinel_rows,
    })
}

const IDENTITY_FIELDS: [&str; 4] = ["systemAddr", "x", "y", "z"];
const OVERLAY_FIELDS: [&str; 3] = ["factionId", "governmentId", "securityLevel"];
const SENTINEL_FIELDS: [&str; 18] = [
    "factionId",
    "minorfactionId",
    "governmentId",
    "developmentLevel",
    "standardOfLiving",
    "population",
    "systemValue",
    "techLevel",
    "economies",
    "state",
    "starsystem_id",
    "systemAddr",
    "tw_rescueMarket",
    "oldMinorFactionIDs",
    "x",
    "y",
    "z",
    "securityLevel",
];

#[derive(Debug, Deserialize)]
struct DigestEnvelope {
    #[serde(deserialize_with = "deserialize_rows")]
    systems: Vec<(String, RawRow)>,
}

/// An object retained as fields until classification.
///
/// Going through `serde_json::Value` for the envelope would collapse duplicate
/// outer keys.  This custom map visitor retains every system entry, while this
/// row visitor also rejects duplicate fields within a row.
#[derive(Debug)]
struct RawRow {
    fields: Map<String, Value>,
}

impl RawRow {
    fn has(&self, field: &str) -> bool {
        self.fields.contains_key(field)
    }

    fn has_exact_fields(&self, expected: &[&str]) -> bool {
        self.fields.len() == expected.len()
            && expected
                .iter()
                .all(|field| self.fields.contains_key(*field))
    }
}

impl<'de> Deserialize<'de> for RawRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RowVisitor;

        impl<'de> Visitor<'de> for RowVisitor {
            type Value = RawRow;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a daily-digest row object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut fields = Map::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if fields.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!("duplicate row field {key:?}")));
                    }
                }
                Ok(RawRow { fields })
            }
        }

        deserializer.deserialize_map(RowVisitor)
    }
}

fn deserialize_rows<'de, D>(deserializer: D) -> Result<Vec<(String, RawRow)>, D::Error>
where
    D: Deserializer<'de>,
{
    struct RowsVisitor;

    impl<'de> Visitor<'de> for RowsVisitor {
        type Value = Vec<(String, RawRow)>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("the daily-digest systems object")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut rows = Vec::with_capacity(map.size_hint().unwrap_or(0));
            // Deliberately do not collect into a map: duplicate rich member
            // names must reach the address-level duplicate check below.
            while let Some(entry) = map.next_entry::<String, RawRow>()? {
                rows.push(entry);
            }
            Ok(rows)
        }
    }

    deserializer.deserialize_map(RowsVisitor)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SparseOverlay {
    #[allow(dead_code)]
    faction_id: u32,
    #[allow(dead_code)]
    government_id: u32,
    #[allow(dead_code)]
    security_level: u32,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
struct DigestWire {
    #[serde(rename = "systemAddr")]
    system_addr: Option<u64>,
    x: Option<f64>,
    y: Option<f64>,
    z: Option<f64>,
    #[serde(rename = "factionId")]
    faction_id: Option<u32>,
    #[serde(rename = "minorfactionId")]
    minor_faction_id: Option<u64>,
    #[serde(rename = "governmentId")]
    government_id: Option<u32>,
    #[serde(rename = "developmentLevel")]
    development_level: Option<u32>,
    #[serde(rename = "standardOfLiving")]
    standard_of_living: Option<u32>,
    population: Option<u64>,
    #[serde(rename = "systemValue")]
    system_value: Option<f64>,
    #[serde(rename = "techLevel")]
    tech_level: Option<f64>,
    economies: Option<[Option<u32>; 2]>,
    state: Option<String>,
    starsystem_id: Option<String>,
    #[serde(rename = "tw_rescueMarket")]
    tw_rescue_market: Option<bool>,
    #[serde(rename = "oldMinorFactionIDs")]
    old_minor_faction_ids: Option<Vec<u64>>,
    #[serde(rename = "powerId")]
    power_id: Option<u32>,
    #[serde(rename = "powerState")]
    power_state: Option<String>,
    #[serde(rename = "securityLevel")]
    security_level: Option<u32>,
}

fn is_observed_sentinel(key: &str, wire: &DigestWire) -> bool {
    key.is_empty()
        && wire.system_addr == Some(0)
        && wire.x == Some(1_000.0)
        && wire.y == Some(-999.0)
        && wire.z == Some(-999.0)
        && wire.faction_id == Some(0)
        && wire.minor_faction_id == Some(0)
        && wire.government_id == Some(0)
        && wire.development_level == Some(60)
        && wire.standard_of_living == Some(50)
        && wire.population == Some(900_000)
        && wire.system_value == Some(40.0)
        && wire.tech_level == Some(50.0)
        && wire.economies == Some([Some(0), None])
        && wire.state.as_deref() == Some("")
        && wire.starsystem_id.as_deref() == Some("9999999")
        && wire.tw_rescue_market == Some(false)
        && wire.old_minor_faction_ids.as_deref() == Some(&[])
        && wire.power_id.is_none()
        && wire.power_state.is_none()
        && wire.security_level == Some(60)
}

fn normalize_rich(key: &str, wire: DigestWire) -> Result<DigestSystem, DigestError> {
    // Presence of all four fields was established before typed deserialization.
    // `Option` can still be `None` for an explicit JSON null, which is malformed.
    let address = wire
        .system_addr
        .ok_or_else(|| malformed(key.to_owned(), "systemAddr is null"))?;
    let raw = Coordinates {
        x: wire
            .x
            .ok_or_else(|| malformed(key.to_owned(), "x is null"))?,
        y: wire
            .y
            .ok_or_else(|| malformed(key.to_owned(), "y is null"))?,
        z: wire
            .z
            .ok_or_else(|| malformed(key.to_owned(), "z is null"))?,
    };

    if address == 0 {
        return Err(malformed(key.to_owned(), "systemAddr must be nonzero"));
    }
    let key_address = key.parse::<u64>().map_err(|_| {
        malformed(
            key.to_owned(),
            "outer key is not a decimal u64 system address",
        )
    })?;
    if key_address == 0 || key_address != address || key != address.to_string() {
        return Err(malformed(
            key.to_owned(),
            format!("outer key address {key:?} does not match systemAddr {address}"),
        ));
    }
    if !raw.x.is_finite() || !raw.y.is_finite() || !raw.z.is_finite() {
        return Err(malformed(key.to_owned(), "coordinates must all be finite"));
    }

    let coordinates = Coordinates {
        x: raw.x - id64::GALAXY_ORIGIN.x,
        y: raw.y - id64::GALAXY_ORIGIN.y,
        z: raw.z - id64::GALAXY_ORIGIN.z,
    };
    let parts = id64::decode(address as f64).map_err(|reason| {
        malformed(
            key.to_owned(),
            format!("systemAddr cannot be decoded: {reason}"),
        )
    })?;
    if !id64::contains(&parts, coordinates) {
        return Err(malformed(
            key.to_owned(),
            "transformed coordinates do not lie in the system address's boxel",
        ));
    }

    let status = DigestStatus {
        faction_id: wire.faction_id,
        minor_faction_id: wire.minor_faction_id,
        government_id: wire.government_id,
        development_level: wire.development_level,
        standard_of_living: wire.standard_of_living,
        population: wire.population,
        system_value: wire.system_value,
        tech_level: wire.tech_level,
        economies: wire.economies,
        state: wire.state,
        starsystem_id: wire.starsystem_id,
        tw_rescue_market: wire.tw_rescue_market,
        old_minor_faction_ids: wire.old_minor_faction_ids,
        power_id: wire.power_id,
        power_state: wire.power_state,
        security_level: wire.security_level,
    };

    Ok(DigestSystem {
        address,
        coordinates,
        status,
    })
}

fn malformed(key: String, reason: impl Into<String>) -> DigestError {
    DigestError::MalformedRow {
        key,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = 10_477_373_803;

    fn page(key: &str, row: &str) -> String {
        format!(r#"{{"systems":{{"{key}":{row}}}}}"#)
    }

    fn sol_row(status: &str) -> String {
        format!(r#"{{"systemAddr":{SOL},"x":49985,"y":40985,"z":24105{status}}}"#)
    }

    #[test]
    fn sol_is_transformed_to_the_galactic_origin() {
        let parsed = parse_page(&page(&SOL.to_string(), &sol_row(""))).unwrap();

        assert_eq!(parsed.primary_rows, 1);
        assert_eq!(parsed.overlay_rows, 0);
        assert_eq!(parsed.sentinel_rows, 0);
        assert_eq!(parsed.systems[0].address, SOL);
        assert_eq!(
            parsed.systems[0].coordinates,
            Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0
            }
        );
        assert!(parsed.is_terminal());
    }

    #[test]
    fn the_exact_sparse_overlay_is_ignored_and_does_not_drive_termination() {
        let document = page(
            "457137146195",
            r#"{"factionId":9,"governmentId":0,"securityLevel":0}"#,
        );
        let parsed = parse_page(&document).unwrap();

        assert!(parsed.systems.is_empty());
        assert_eq!(parsed.primary_rows, 0);
        assert_eq!(parsed.overlay_rows, 1);
        assert_eq!(parsed.sentinel_rows, 0);
        assert!(parsed.is_terminal());
    }

    #[test]
    fn a_sparse_lookalike_with_an_extra_field_invalidates_the_page() {
        let document = page(
            "457137146195",
            r#"{"factionId":9,"governmentId":0,"securityLevel":0,"population":0}"#,
        );
        assert!(matches!(
            parse_page(&document),
            Err(DigestError::MalformedRow { .. })
        ));
    }

    #[test]
    fn population_zero_is_a_real_status_value() {
        let parsed = parse_page(&page(&SOL.to_string(), &sol_row(r#","population":0"#))).unwrap();
        assert_eq!(parsed.systems.len(), 1);
        assert_eq!(parsed.systems[0].status.population, Some(0));
    }

    #[test]
    fn a_nullable_secondary_economy_is_valid() {
        let parsed = parse_page(&page(
            &SOL.to_string(),
            &sol_row(r#","economies":[7,null]"#),
        ))
        .unwrap();
        assert_eq!(parsed.systems[0].status.economies, Some([Some(7), None]));
    }

    #[test]
    fn minor_faction_ids_above_two_to_the_53_remain_exact() {
        const FACTION: u64 = 72_060_832_334_024_995;
        let parsed = parse_page(&page(
            &SOL.to_string(),
            &sol_row(&format!(r#","minorfactionId":{FACTION}"#)),
        ))
        .unwrap();
        assert_eq!(parsed.systems[0].status.minor_faction_id, Some(FACTION));
    }

    #[test]
    fn an_outer_key_address_must_match_the_rich_row() {
        let error = parse_page(&page("1", &sol_row(""))).unwrap_err();
        assert!(matches!(error, DigestError::MalformedRow { .. }));
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn partial_coordinates_invalidate_the_whole_page() {
        let row = format!(r#"{{"systemAddr":{SOL},"x":49985,"y":40985}}"#);
        let error = parse_page(&page(&SOL.to_string(), &row)).unwrap_err();
        assert!(matches!(error, DigestError::MalformedRow { .. }));
        assert!(error.to_string().contains("partially present"));
    }

    #[test]
    fn nonfinite_coordinates_are_rejected_explicitly() {
        let wire = DigestWire {
            system_addr: Some(SOL),
            x: Some(f64::INFINITY),
            y: Some(40_985.0),
            z: Some(24_105.0),
            ..DigestWire::default()
        };
        let error = normalize_rich(&SOL.to_string(), wire).unwrap_err();
        assert!(error.to_string().contains("finite"));

        // JSON itself has no infinity literal; an exponent that overflows f64
        // must likewise fail rather than becoming a coordinate silently.
        let document = page(
            &SOL.to_string(),
            &format!(r#"{{"systemAddr":{SOL},"x":1e400,"y":40985,"z":24105}}"#),
        );
        assert!(parse_page(&document).is_err());
    }

    #[test]
    fn coordinates_must_belong_to_the_address_boxel() {
        let row = format!(r#"{{"systemAddr":{SOL},"x":59985,"y":40985,"z":24105}}"#);
        let error = parse_page(&page(&SOL.to_string(), &row)).unwrap_err();
        assert!(error.to_string().contains("boxel"));
    }

    #[test]
    fn the_observed_sentinel_is_counted_but_not_normalized() {
        let sentinel = r#"{
            "factionId":0,"minorfactionId":0,"governmentId":0,
            "developmentLevel":60,"standardOfLiving":50,"population":900000,
            "systemValue":40,"techLevel":50,"economies":[0,null],"state":"",
            "starsystem_id":"9999999","systemAddr":0,"tw_rescueMarket":false,
            "oldMinorFactionIDs":[],"x":1000,"y":-999,"z":-999,"securityLevel":60
        }"#;
        let parsed = parse_page(&page("", sentinel)).unwrap();

        assert!(parsed.systems.is_empty());
        assert_eq!(
            parsed.primary_rows, 1,
            "the sentinel occupies a primary-table slot"
        );
        assert_eq!(parsed.overlay_rows, 0);
        assert_eq!(parsed.sentinel_rows, 1);
    }

    #[test]
    fn a_sentinel_lookalike_is_not_silently_accepted() {
        let lookalike = r#"{
            "factionId":0,"minorfactionId":0,"governmentId":0,
            "developmentLevel":60,"standardOfLiving":50,"population":900001,
            "systemValue":40,"techLevel":50,"economies":[0,null],"state":"",
            "starsystem_id":"9999999","systemAddr":0,"tw_rescueMarket":false,
            "oldMinorFactionIDs":[],"x":1000,"y":-999,"z":-999,"securityLevel":60
        }"#;
        assert!(matches!(
            parse_page(&page("", lookalike)),
            Err(DigestError::MalformedRow { .. })
        ));
    }

    #[test]
    fn duplicate_rich_member_ids_are_an_error_even_in_raw_json() {
        let row = sol_row("");
        let document = format!(r#"{{"systems":{{"{SOL}":{row},"{SOL}":{row}}}}}"#);
        assert!(matches!(
            parse_page(&document),
            Err(DigestError::DuplicateAddress { address: SOL })
        ));
    }

    #[test]
    fn exactly_full_primary_count_requires_another_page() {
        let page = DigestPage {
            systems: Vec::new(),
            primary_rows: PRIMARY_PAGE_SIZE,
            overlay_rows: 359,
            sentinel_rows: 0,
        };
        assert!(!page.is_terminal());
    }
}
