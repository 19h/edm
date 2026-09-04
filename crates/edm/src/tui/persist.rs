//! Pins and the last search, on disk \[C53\].
//!
//! Beside the price cache, through the `Fs` port, as JSON this program's own
//! serializer writes. A missing file is empty; a file that cannot be read is
//! logged once and treated as empty, never as a reason to refuse to start.

use std::path::{Path, PathBuf};

use edm_core::ardent::ArdentStation;
use edm_core::domain::id64::Coordinates;
use edm_core::js::json::{JsObject, JsValue};
use edm_route::pin::PinKey;

use crate::ports::Fs;

use super::app::Pin;

const VERSION: f64 = 1.0;

/// Where the UI keeps its own files: `<cache root>/ui/`.
pub(crate) fn directory(cache_root: &Path) -> PathBuf {
    cache_root
        .parent()
        .map_or_else(|| cache_root.join("ui"), |parent| parent.join("ui"))
}

fn obj(fields: Vec<(&str, JsValue)>) -> JsValue {
    JsValue::Obj(JsObject::from_document_order(
        fields
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}

fn num(value: f64) -> JsValue {
    if value.is_finite() {
        JsValue::Num(value)
    } else {
        JsValue::Null
    }
}

fn text(value: &str) -> JsValue {
    JsValue::Str(value.into())
}

fn station_json(station: &ArdentStation) -> JsValue {
    obj(vec![
        ("marketId", num(station.market_id)),
        ("stationName", text(&station.station_name)),
        ("systemName", text(&station.system_name)),
        ("systemAddress", num(station.system_address)),
        (
            "stationType",
            station.station_type.as_deref().map_or(JsValue::Null, text),
        ),
        (
            "maxLandingPadSize",
            station.max_landing_pad_size.map_or(JsValue::Null, num),
        ),
        (
            "distanceToArrival",
            station.distance_to_arrival.map_or(JsValue::Null, num),
        ),
        ("x", num(station.coordinates.x)),
        ("y", num(station.coordinates.y)),
        ("z", num(station.coordinates.z)),
    ])
}

fn station_from_json(value: &JsValue) -> Option<ArdentStation> {
    let object = value.as_object()?;
    let string = |key: &str| object.get(key).and_then(JsValue::as_str).map(ToOwned::to_owned);
    let number = |key: &str| object.get(key).and_then(JsValue::as_f64);
    Some(ArdentStation {
        market_id: number("marketId")?,
        station_name: string("stationName")?,
        system_name: string("systemName")?,
        system_address: number("systemAddress").unwrap_or(0.0),
        station_type: string("stationType"),
        max_landing_pad_size: number("maxLandingPadSize"),
        distance_to_arrival: number("distanceToArrival"),
        coordinates: Coordinates {
            x: number("x").unwrap_or(f64::NAN),
            y: number("y").unwrap_or(f64::NAN),
            z: number("z").unwrap_or(f64::NAN),
        },
    })
}

fn argv_json(argv: &[String]) -> JsValue {
    JsValue::Arr(argv.iter().map(|token| text(token)).collect())
}

fn argv_from_json(value: &JsValue) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|token| token.as_str().map(ToOwned::to_owned))
        .collect()
}

/// The pins as JSON.
pub(crate) fn pins_json(pins: &[Pin]) -> String {
    obj(vec![
        ("version", num(VERSION)),
        (
            "pins",
            JsValue::Arr(
                pins.iter()
                    .map(|pin| {
                        obj(vec![
                            ("key", pin.key.to_json()),
                            ("label", text(&pin.label)),
                            ("pinnedAt", num(pin.pinned_at_ms)),
                            ("argv", argv_json(&pin.argv)),
                            (
                                "stations",
                                JsValue::Arr(pin.stations.iter().map(station_json).collect()),
                            ),
                            (
                                "last",
                                match &pin.last {
                                    Some(last) => obj(vec![
                                        ("perHour", num(last.per_hour as f64)),
                                        ("profit", num(last.profit as f64)),
                                        ("refreshedAt", num(last.refreshed_at_ms)),
                                        (
                                            "unpriced",
                                            last.unpriced.as_deref().map_or(JsValue::Null, text),
                                        ),
                                    ]),
                                    None => JsValue::Null,
                                },
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
    .stringify(2)
}

/// What was last known about a pin, kept so the list has content before the
/// first refresh.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LastKnown {
    pub per_hour: i64,
    pub profit: i64,
    pub refreshed_at_ms: f64,
    pub unpriced: Option<String>,
}

/// The pins in a file's text; malformed entries are skipped.
pub(crate) fn pins_from_json(source: &str) -> Result<Vec<Pin>, String> {
    let document = JsValue::parse(source).map_err(|error| error.to_string())?;
    let object = document.as_object().ok_or("pins.json is not an object")?;
    let version = object.get("version").and_then(JsValue::as_f64).unwrap_or(0.0);
    if version != VERSION {
        return Err(format!("pins.json is version {version}, and this build reads {VERSION}"));
    }
    let entries = object
        .get("pins")
        .and_then(JsValue::as_array)
        .ok_or("pins.json has no pins array")?;
    let mut pins = Vec::new();
    for entry in entries {
        let Some(record) = entry.as_object() else { continue };
        let Some(key) = record.get("key").and_then(PinKey::from_json) else { continue };
        let Some(argv) = record.get("argv").and_then(argv_from_json) else { continue };
        let stations: Vec<ArdentStation> = record
            .get("stations")
            .and_then(JsValue::as_array)
            .map(|list| list.iter().filter_map(station_from_json).collect())
            .unwrap_or_default();
        if stations.len() != key.stations.len() {
            continue;
        }
        let last = record.get("last").and_then(JsValue::as_object).map(|last| LastKnown {
            per_hour: last.get("perHour").and_then(JsValue::as_f64).unwrap_or(0.0) as i64,
            profit: last.get("profit").and_then(JsValue::as_f64).unwrap_or(0.0) as i64,
            refreshed_at_ms: last.get("refreshedAt").and_then(JsValue::as_f64).unwrap_or(0.0),
            unpriced: last.get("unpriced").and_then(JsValue::as_str).map(ToOwned::to_owned),
        });
        let label = record
            .get("label")
            .and_then(JsValue::as_str)
            .map_or_else(|| key.describe(&[]), ToOwned::to_owned);
        pins.push(Pin::restored(key, label, argv, stations, record.get("pinnedAt").and_then(JsValue::as_f64).unwrap_or(0.0), last));
    }
    Ok(pins)
}

pub(crate) fn last_search_json(argv: &[String]) -> String {
    obj(vec![("version", num(VERSION)), ("argv", argv_json(argv))]).stringify(2)
}

pub(crate) fn last_search_from_json(source: &str) -> Option<Vec<String>> {
    let document = JsValue::parse(source).ok()?;
    let object = document.as_object()?;
    (object.get("version").and_then(JsValue::as_f64) == Some(VERSION))
        .then(|| object.get("argv").and_then(argv_from_json))
        .flatten()
}

/// Write `text` at `path`, creating the directory.
pub(crate) fn write<F: Fs>(fs: &F, path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    fs.write(path, text)
        .map_err(|error| format!("writing {}: {error}", path.display()))
}

/// Read `path`, or `None` when there is no such file.
pub(crate) fn read<F: Fs>(fs: &F, path: &Path) -> Option<String> {
    fs.read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use edm_route::pin::PinKind;

    #[test]
    fn pins_round_trip_and_a_malformed_entry_is_skipped() {
        let station = |id: f64, name: &str| ArdentStation {
            market_id: id,
            station_name: name.to_owned(),
            system_name: "Sol".to_owned(),
            system_address: 10_477_373_803.0,
            station_type: Some("Coriolis".to_owned()),
            max_landing_pad_size: Some(3.0),
            distance_to_arrival: Some(505.0),
            coordinates: Coordinates { x: 0.0, y: 0.0, z: 0.0 },
        };
        let key = PinKey {
            kind: PinKind::OneWay,
            stations: vec![128_016_384, 128_016_576],
            commodities: vec!["gold".to_owned()],
        };
        let argv: Vec<String> = ["route", "Sol", "--quick", "3", "--item", "gold"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let pin = Pin::restored(
            key.clone(),
            "Galileo > Titan City (gold)".to_owned(),
            argv.clone(),
            vec![station(128_016_384.0, "Galileo"), station(128_016_576.0, "Titan City")],
            1_700_000_000_000.0,
            Some(LastKnown {
                per_hour: 1_234_567,
                profit: 98_765,
                refreshed_at_ms: 1_700_000_100_000.0,
                unpriced: None,
            }),
        );
        let text = pins_json(&[pin]);
        let back = pins_from_json(&text).expect("parses");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].key, key);
        assert_eq!(back[0].argv, argv);
        assert_eq!(back[0].stations[1].station_name, "Titan City");
        assert_eq!(back[0].last.as_ref().map(|l| l.per_hour), Some(1_234_567));

        // A pin whose stations do not match its key is dropped, not trusted.
        let broken = text.replace(r#""marketId": 128016576"#, r#""marketId": "x""#);
        assert!(pins_from_json(&broken).expect("still parses").is_empty());
        assert!(pins_from_json(r#"{"version": 2, "pins": []}"#).is_err());
    }

    #[test]
    fn the_last_search_round_trips() {
        let argv: Vec<String> = vec!["sell".to_owned(), "--stops".to_owned(), "2".to_owned()];
        assert_eq!(last_search_from_json(&last_search_json(&argv)), Some(argv));
        assert_eq!(last_search_from_json("{}"), None);
    }
}
