//! Combat zones as `/2.0/elite/starsystem` body sites.
//!
//! The system map — including the Frontline Solutions overlay — draws space
//! conflict zones from generated `Warzone_PointRace_{Low,Med,High}_01` sites.
//! Intensity is the site's `tags["0"]` (and the matching `scriptName`); there
//! is no separate `intensity` field. On-foot settlement warzones use
//! `Warzone_Settlement` with a difficulty/intensity tag pair.

use crate::js;
use crate::js::json::{JsObject, JsValue};

use super::read::Read;
use super::starsystem::lookup_faction;

/// Space CZ vs on-foot settlement warzone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneKind {
    Space,
    Settlement,
}

impl ZoneKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Space => "space",
            Self::Settlement => "settlement",
        }
    }
}

/// Nav-panel intensity. `Med` is the payload's own spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intensity {
    Low,
    Med,
    High,
}

impl Intensity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Med => "Med",
            Self::High => "High",
        }
    }

    /// High first, then Med, then Low.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::High => 0,
            Self::Med => 1,
            Self::Low => 2,
        }
    }
}

/// One conflict zone extracted from a starsystem payload.
#[derive(Clone, Debug, PartialEq)]
pub struct CombatZone {
    pub site_id: f64,
    pub kind: ZoneKind,
    pub intensity: Intensity,
    /// Settlement only: Easy / Medium / Hard.
    pub difficulty: Option<String>,
    /// Player-facing location. Settlement warzones are the settlement market
    /// itself, so this is that market's `name` (trailing `+` conflict markers
    /// stripped). Space CZs have none — their `name` is a `$Warzone_…;` token.
    pub name: Option<String>,
    pub primary_faction: Option<String>,
    pub secondary_faction: Option<String>,
    pub dist_ls: Option<f64>,
    pub gameplay: String,
    /// System-level `minorfaction_state` (`war`, `civilwar`, …).
    pub conflict: Option<String>,
}

impl CombatZone {
    #[must_use]
    pub fn sides(&self) -> String {
        match (&self.primary_faction, &self.secondary_faction) {
            (Some(left), Some(right)) => format!("{left} vs {right}"),
            (Some(left), None) => left.clone(),
            (None, Some(right)) => right.clone(),
            (None, None) => String::new(),
        }
    }

    /// Table cell for the location column.
    #[must_use]
    pub fn location(&self) -> &str {
        match self.kind {
            ZoneKind::Settlement => self.name.as_deref().unwrap_or(""),
            ZoneKind::Space => self.name.as_deref().unwrap_or("Conflict zone"),
        }
    }
}

/// Reads combat zones out of a decoded starsystem document.
///
/// Settlement warzones are omitted unless `include_settlements` is set. The
/// walk is by shape rather than path: the sites sit in a numbered map whose
/// parent key has already drifted once in captured payloads.
#[must_use]
pub fn read_combat_zones(value: &JsValue, include_settlements: bool) -> Vec<CombatZone> {
    let factions = find_named_record(value, "minorFactions");
    let conflict = find_string_field(value, "minorfaction_state")
        .map(str::to_owned)
        .filter(|state| !state.is_empty());

    let mut zones = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk(
        value,
        0,
        factions,
        conflict.as_deref(),
        include_settlements,
        &mut seen,
        &mut zones,
    );
    zones
}

fn walk(
    value: &JsValue,
    depth: usize,
    factions: Option<&JsObject>,
    conflict: Option<&str>,
    include_settlements: bool,
    seen: &mut std::collections::HashSet<u64>,
    zones: &mut Vec<CombatZone>,
) {
    if depth > 12 {
        return;
    }
    if let Some(items) = value.as_array() {
        for entry in items {
            walk(
                entry,
                depth + 1,
                factions,
                conflict,
                include_settlements,
                seen,
                zones,
            );
        }
        return;
    }
    let Some(record) = value.as_record() else {
        return;
    };

    if let Some(zone) = read_site(record, factions, conflict, include_settlements)
        && seen.insert(zone.site_id.to_bits())
    {
        zones.push(zone);
    }

    for (_, child) in record.iter() {
        walk(
            child,
            depth + 1,
            factions,
            conflict,
            include_settlements,
            seen,
            zones,
        );
    }
}

fn read_site(
    record: &JsObject,
    factions: Option<&JsObject>,
    conflict: Option<&str>,
    include_settlements: bool,
) -> Option<CombatZone> {
    let site_id = record.num("bodysiteId");
    if !js::safe_int(site_id) || site_id <= 0.0 {
        return None;
    }
    let script = record.string("scriptName");
    let gameplay = record.string("gameplay");
    let label = if script.is_empty() { gameplay } else { script };
    let kind = zone_kind(label)?;
    if kind == ZoneKind::Settlement && !include_settlements {
        return None;
    }

    let tags = record.record("tags");
    let (intensity, difficulty) = match kind {
        ZoneKind::Space => (space_intensity(tags, label)?, None),
        ZoneKind::Settlement => {
            let intensity = tag_intensity(tags, "1").or_else(|| tag_intensity(tags, "0"))?;
            let difficulty = tags
                .map(|tags| tags.string("0"))
                .filter(|value| !value.is_empty() && parse_intensity(value).is_none())
                .map(str::to_owned);
            (intensity, difficulty)
        }
    };

    let params = record.record("scriptParameters");
    let dist = record.num("distFromSystem");
    Some(CombatZone {
        site_id,
        kind,
        intensity,
        difficulty,
        name: site_name(record),
        primary_faction: faction_name(factions, params.and_then(|p| p.get("PrimaryFactionID"))),
        secondary_faction: faction_name(factions, params.and_then(|p| p.get("SecondaryFactionID"))),
        dist_ls: (dist > 0.0).then_some(dist),
        gameplay: label.to_owned(),
        conflict: conflict.map(str::to_owned),
    })
}

/// The market `name` on a settlement warzone, minus localisation tokens and the
/// trailing `+` / `++` the payload appends while the site is contested.
fn site_name(record: &JsObject) -> Option<String> {
    let raw = record.string("name");
    if raw.is_empty() || raw.starts_with('$') {
        return None;
    }
    let stripped = raw.trim_end_matches('+');
    let name = crate::js::text::js_trim(stripped);
    (!name.is_empty()).then(|| name.to_owned())
}

fn zone_kind(label: &str) -> Option<ZoneKind> {
    if label.starts_with("Warzone_PointRace_") {
        Some(ZoneKind::Space)
    } else if label == "Warzone_Settlement" {
        Some(ZoneKind::Settlement)
    } else {
        None
    }
}

fn space_intensity(tags: Option<&JsObject>, label: &str) -> Option<Intensity> {
    tag_intensity(tags, "0").or_else(|| {
        label
            .strip_prefix("Warzone_PointRace_")
            .and_then(|rest| rest.split('_').next())
            .and_then(parse_intensity)
    })
}

fn tag_intensity(tags: Option<&JsObject>, key: &str) -> Option<Intensity> {
    tags.map(|tags| tags.string(key))
        .filter(|value| !value.is_empty())
        .and_then(parse_intensity)
}

fn parse_intensity(raw: &str) -> Option<Intensity> {
    match raw {
        "Low" | "low" => Some(Intensity::Low),
        "Med" | "Medium" | "med" | "medium" => Some(Intensity::Med),
        "High" | "high" => Some(Intensity::High),
        _ => None,
    }
}

fn faction_name(factions: Option<&JsObject>, id: Option<&JsValue>) -> Option<String> {
    let name = lookup_faction(factions?, id)?.string("name");
    (!name.is_empty()).then(|| name.to_owned())
}

fn find_named_record<'a>(value: &'a JsValue, wanted: &str) -> Option<&'a JsObject> {
    find_record(value, 0, wanted)
}

fn find_record<'a>(value: &'a JsValue, depth: usize, wanted: &str) -> Option<&'a JsObject> {
    if depth > 12 {
        return None;
    }
    if let Some(items) = value.as_array() {
        return items.iter().find_map(|entry| find_record(entry, depth + 1, wanted));
    }
    let record = value.as_record()?;
    if let Some(hit) = record.record(wanted) {
        return Some(hit);
    }
    record
        .iter()
        .find_map(|(_, child)| find_record(child, depth + 1, wanted))
}

fn find_string_field<'a>(value: &'a JsValue, wanted: &str) -> Option<&'a str> {
    find_string(value, 0, wanted)
}

fn find_string<'a>(value: &'a JsValue, depth: usize, wanted: &str) -> Option<&'a str> {
    if depth > 12 {
        return None;
    }
    if let Some(items) = value.as_array() {
        return items.iter().find_map(|entry| find_string(entry, depth + 1, wanted));
    }
    let record = value.as_record()?;
    let direct = record.string(wanted);
    if !direct.is_empty() {
        return Some(direct);
    }
    record
        .iter()
        .find_map(|(_, child)| find_string(child, depth + 1, wanted))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = r#"{
      "starsystem": {
        "starsystem": {
          "name": "Arangorii",
          "minorfaction_state": "war",
          "minorFactions": {
            "4099303016811": {"id": 4099303016811, "name": "Scori Alliance"},
            "180145679199357291": {"id": 180145679199357291, "name": "East India Company"}
          }
        },
        "sites": {
          "2": {
            "bodysiteId": 3282400282,
            "gameplay": "Warzone_PointRace_Low_01",
            "scriptName": "Warzone_PointRace_Low_01",
            "distFromSystem": 440449.29453,
            "scriptParameters": {
              "PrimaryFactionID": 4099303016811,
              "SecondaryFactionID": 180145679199357291
            },
            "tags": {"0": "Low"},
            "name": "$Warzone_PointRace_Low;"
          },
          "3": {
            "bodysiteId": 3282400283,
            "scriptName": "Warzone_PointRace_Med_01",
            "distFromSystem": 1000,
            "scriptParameters": {
              "PrimaryFactionID": 4099303016811,
              "SecondaryFactionID": 180145679199357291
            },
            "tags": {"0": "Med"}
          },
          "4": {
            "bodysiteId": 3282400284,
            "scriptName": "Warzone_PointRace_High_01",
            "distFromSystem": 2000,
            "scriptParameters": {
              "PrimaryFactionID": 4099303016811,
              "SecondaryFactionID": 180145679199357291
            },
            "tags": {"0": "High"}
          },
          "5": {
            "id": 3872061696,
            "name": "Mordovski Tourism Lodge +",
            "poiType": "onFootSettlement",
            "bodysiteId": 99,
            "scriptName": "Warzone_Settlement",
            "distFromSystem": 2063.75,
            "scriptParameters": {
              "PrimaryFactionID": 4099303016811,
              "SecondaryFactionID": 180145679199357291
            },
            "tags": {"0": "Hard", "1": "Low"}
          }
        }
      }
    }"#;

    fn payload() -> JsValue {
        JsValue::parse(PAYLOAD).expect("fixture is JSON")
    }

    #[test]
    fn space_zones_carry_intensity_factions_and_conflict() {
        let zones = read_combat_zones(&payload(), false);
        assert_eq!(zones.len(), 3);
        assert!(zones.iter().all(|zone| zone.kind == ZoneKind::Space));
        assert_eq!(
            zones
                .iter()
                .map(|zone| zone.intensity)
                .collect::<Vec<_>>(),
            [Intensity::Low, Intensity::Med, Intensity::High]
        );
        assert_eq!(zones[0].site_id, 3_282_400_282.0);
        assert_eq!(zones[0].primary_faction.as_deref(), Some("Scori Alliance"));
        assert_eq!(
            zones[0].secondary_faction.as_deref(),
            Some("East India Company")
        );
        assert_eq!(zones[0].sides(), "Scori Alliance vs East India Company");
        assert_eq!(zones[0].conflict.as_deref(), Some("war"));
        assert_eq!(zones[2].intensity.rank(), 0);
    }

    #[test]
    fn settlements_are_opt_in_and_keep_the_difficulty_tag() {
        let space_only = read_combat_zones(&payload(), false);
        assert!(space_only.iter().all(|zone| zone.difficulty.is_none()));

        let with_settlements = read_combat_zones(&payload(), true);
        assert_eq!(with_settlements.len(), 4);
        let settlement = with_settlements
            .iter()
            .find(|zone| zone.kind == ZoneKind::Settlement)
            .expect("settlement retained");
        assert_eq!(settlement.intensity, Intensity::Low);
        assert_eq!(settlement.difficulty.as_deref(), Some("Hard"));
        assert_eq!(
            settlement.name.as_deref(),
            Some("Mordovski Tourism Lodge"),
            "trailing + conflict markers are not part of the settlement name"
        );
        assert_eq!(settlement.location(), "Mordovski Tourism Lodge");
        assert_eq!(
            with_settlements
                .iter()
                .find(|zone| zone.kind == ZoneKind::Space)
                .map(CombatZone::location),
            Some("Conflict zone")
        );
        assert!(
            space_only[0].name.is_none(),
            "$Warzone_…; tokens are not location names"
        );
    }

    #[test]
    fn empty_and_unrelated_payloads_are_empty() {
        assert!(read_combat_zones(&JsValue::Null, true).is_empty());
        let other = JsValue::parse(r#"{"starsystem":{"polities":{}}}"#).unwrap();
        assert!(read_combat_zones(&other, true).is_empty());
    }
}
