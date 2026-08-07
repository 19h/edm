//! Pioneer Supplies inventory returned by `/2.0/elite/vendors/items`.
//!
//! The endpoint mixes station-specific premium stock with large static catalogues
//! (`outfitting` and `microresources`). A locator uses the live premium offers
//! and the market-specific ordinary persona outfitting, while ignoring static
//! price modifiers, upgrade recipes, and microresource catalogues.

use crate::js;
use crate::js::json::JsValue;

/// The two premium-stock buckets in the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VendorItemKind {
    Weapon,
    Suit,
}

impl VendorItemKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Weapon => "weapon",
            Self::Suit => "suit",
        }
    }
}

/// One premium offer slot or ordinary item at a Pioneer Supplies vendor.
#[derive(Clone, Debug, PartialEq)]
pub struct VendorItem {
    pub kind: VendorItemKind,
    /// Frontier's internal prototype name.
    pub symbol: String,
    /// Numeric in meaning, but observed as both a JSON string and a JSON number.
    pub id: String,
    pub grade: f64,
    /// `Some` for premium offer slots; ordinary outfitting is implicitly available.
    pub quantity: Option<f64>,
    pub price: f64,
    /// Only populated mod slots; `"Mod1": null` is an empty slot.
    pub mods: Vec<String>,
}

impl VendorItem {
    /// The player-facing name where Frontier's prototype is known.
    #[must_use]
    pub fn name(&self) -> &str {
        friendly_name(&self.symbol)
    }

    #[must_use]
    pub fn available(&self) -> bool {
        self.quantity.is_none_or(|quantity| quantity > 0.0)
    }

    #[must_use]
    pub const fn premium(&self) -> bool {
        self.quantity.is_some()
    }
}

/// Reads the `premiumstock.personaweapon` and `.personasuit` arrays.
///
/// Malformed rows are skipped independently.  Unavailable rows are retained:
/// the caller decides whether it is inspecting a response or locating stock.
#[must_use]
pub fn read_premium_items(value: &JsValue) -> Vec<VendorItem> {
    let Some(premium) = value
        .as_record()
        .and_then(|root| root.get("premiumstock"))
        .and_then(JsValue::as_record)
    else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for (key, kind) in [
        ("personaweapon", VendorItemKind::Weapon),
        ("personasuit", VendorItemKind::Suit),
    ] {
        let Some(rows) = premium.get(key).and_then(JsValue::as_array) else {
            continue;
        };
        items.extend(rows.iter().filter_map(|row| parse_item(row, kind)));
    }
    items
}

fn item_id(value: &JsValue) -> Option<String> {
    match value {
        JsValue::Str(value) => Some(value.to_string()),
        JsValue::Num(value) if value.is_finite() => Some(js::js_number(*value)),
        _ => None,
    }
}

fn parse_item(value: &JsValue, kind: VendorItemKind) -> Option<VendorItem> {
    let row = value.as_record()?;
    let symbol = row.get("name")?.as_str()?.to_owned();
    let id = item_id(row.get("id")?)?;
    let grade = row.get("class")?.as_f64()?;
    let quantity = row.get("quantity")?.as_f64()?;
    let price = row.get("credits_withmods_value")?.as_f64()?;
    let mods = row
        .get("mods")
        .and_then(JsValue::as_record)
        .map_or_else(Vec::new, |slots| {
            slots
                .iter()
                .filter_map(|(_, modifier)| {
                    if modifier.is_null() {
                        None
                    } else {
                        Some(
                            modifier
                                .as_str()
                                .map_or_else(|| modifier.stringify_compact(), str::to_owned),
                        )
                    }
                })
                .collect()
        });
    Some(VendorItem {
        kind,
        symbol,
        id,
        grade,
        quantity: Some(quantity),
        price,
        mods,
    })
}

/// Reads ordinary grade-1 persona outfitting that is available at this market.
#[must_use]
pub fn read_outfitting_items(value: &JsValue) -> Vec<VendorItem> {
    let Some(outfitting) = value
        .as_record()
        .and_then(|root| root.get("outfitting"))
        .and_then(JsValue::as_record)
    else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for (key, kind) in [
        ("personaweapon", VendorItemKind::Weapon),
        ("personasuit", VendorItemKind::Suit),
    ] {
        let Some(rows) = outfitting.get(key).and_then(JsValue::as_record) else {
            continue;
        };
        items.extend(rows.iter().filter_map(|(_, value)| {
            let row = value.as_record()?;
            Some(VendorItem {
                kind,
                symbol: row.get("name")?.as_str()?.to_owned(),
                id: item_id(row.get("id")?)?,
                grade: row.get("class")?.as_f64()?,
                quantity: None,
                price: row.get("credits_basevalue")?.as_f64()?,
                mods: Vec::new(),
            })
        }));
    }
    items
}

/// Frontier's symbols are stable but not useful search results on their own.
#[must_use]
pub fn friendly_name(symbol: &str) -> &str {
    match symbol {
        "Wpn_S_Pistol_Plasma_Charged" => "Manticore Tormentor",
        "Wpn_M_AssaultRifle_Plasma_FAuto" => "Manticore Oppressor",
        "Wpn_M_Sniper_Plasma_Charged" => "Manticore Executioner",
        "Wpn_M_Shotgun_Plasma_DoubleBarrel" => "Manticore Intimidator",
        "Wpn_S_Pistol_Laser_SAuto" => "Takada Zenith",
        "Wpn_M_AssaultRifle_Laser_FAuto" => "Takada Aphelion",
        "Wpn_M_SubMachineGun_Laser_FAuto" => "Takada Eclipse",
        "Wpn_S_Pistol_Kinetic_SAuto" => "Karma P-15",
        "Wpn_M_AssaultRifle_Kinetic_FAuto" => "Karma AR-50",
        "Wpn_M_SubMachineGun_Kinetic_FAuto" => "Karma C-44",
        "Wpn_M_Launcher_Rocket_SAuto" => "Karma L-6",
        name if name.starts_with("TacticalSuit_") => "Dominator Suit",
        name if name.starts_with("UtilitySuit_") => "Maverick Suit",
        name if name.starts_with("ExplorationSuit_") => "Artemis Suit",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mixed_ids_and_only_populated_mod_slots() {
        let payload = JsValue::parse(
            r#"{"premiumstock":{"personaweapon":[
                {"name":"Wpn_S_Pistol_Laser_SAuto","id":"128937288","class":3,
                 "quantity":1,"credits_withmods_value":1250000,
                 "mods":{"Mod1":null,"Mod2":"weapon_mod_stability"}},
                {"name":"Wpn_S_Pistol_Plasma_Charged","id":"128937281","class":2,
                 "quantity":0,"credits_withmods_value":250000,"mods":{"Mod1":null}}
            ],"personasuit":[
                {"name":"ExplorationSuit_Class2","id":128958661,"class":2,
                 "quantity":1,"credits_withmods_value":750000,"mods":{}}
            ]},"outfitting":{"personaweapon":{"128937271":{
                "id":"128937271","name":"Wpn_M_AssaultRifle_Kinetic_FAuto",
                "class":1,"credits_basevalue":125000
            }}}}"#,
        )
        .expect("fixture is JSON");

        let items = read_premium_items(&payload);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].name(), "Takada Zenith");
        assert_eq!(items[0].mods, ["weapon_mod_stability"]);
        assert_eq!(items[2].id, "128958661");
        assert_eq!(items[2].name(), "Artemis Suit");
        assert_eq!(items.iter().filter(|item| item.available()).count(), 2);

        let ordinary = read_outfitting_items(&payload);
        assert_eq!(ordinary.len(), 1);
        assert_eq!(ordinary[0].name(), "Karma AR-50");
        assert!(ordinary[0].available());
        assert!(!ordinary[0].premium());
    }

    #[test]
    fn bartender_and_missing_stock_are_empty() {
        let bartender =
            JsValue::parse(r#"{"premiumstock":null,"microresources":{"Item":{"1":{"id":1}}}}"#)
                .unwrap();
        assert!(read_premium_items(&bartender).is_empty());
        assert!(read_premium_items(&JsValue::Null).is_empty());
    }
}
