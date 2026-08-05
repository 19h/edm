//! The market model: what a Companion API payload means once it has been read.

pub mod batch;
pub mod eddn;
pub mod id64;
pub mod read;
pub mod starsystem;
pub mod trade;

use crate::js::json::{JsObject, JsValue};
use crate::js::{self, text};
use read::Read;

/// One row of a market's commodity listing.
///
/// Borrows out of the parsed document rather than owning its strings, so
/// reading a market costs one `Vec` allocation and no string allocations. Every
/// numeric field is `f64` because the Companion API's numbers arrive as
/// JavaScript numbers and reach the wire the same way — a stock level or a unit
/// price can legitimately be fractional. See F3.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Commodity<'a> {
    pub id: f64,
    pub name: &'a str,
    pub category: &'a str,
    pub stock: f64,
    pub stock_bracket: f64,
    pub buy_price: f64,
    pub sell_price: f64,
    pub fence_price: f64,
    pub demand: f64,
    pub demand_bracket: f64,
    pub mean_price: f64,
    pub consumer: bool,
    pub producer: bool,
    pub rare: bool,
    pub illegal: bool,
}

/// `toCommodity` (ts:600).
fn to_commodity<'a>(key: &'a str, source: &'a JsObject) -> Commodity<'a> {
    Commodity {
        // Three fallbacks, and the last one exists only to turn NaN into 0.
        // `readMarketPoints` has the same shape with two. R16.
        id: read::or_else(source.num("id"), || read::or_else(js::to_number(key), || 0.0)),
        name: read::or_else_str(source.string("name"), key),
        category: read::or_else_str(read::trimmed(source.string("categoryname")), "Uncategorised"),
        stock: source.num("stock"),
        stock_bracket: source.num("stockBracket"),
        buy_price: source.num("buyPrice"),
        sell_price: source.num("sellPrice"),
        fence_price: source.num("fencePrice"),
        demand: source.num("demand"),
        demand_bracket: source.num("demandBracket"),
        mean_price: source.num("meanPrice"),
        // `> 0`, not a boolean read: a payload saying `"rare": true` yields
        // false here. R17.
        consumer: source.positive("consumer"),
        producer: source.positive("producer"),
        rare: source.positive("rare"),
        // Any non-empty legality string means illegal; `0` and `null` do not.
        illegal: !source.string("legality").is_empty(),
    }
}

/// A market listing that parsed into something usable.
#[derive(Clone, Debug)]
pub struct MarketSnapshot<'a> {
    /// The whole payload, kept because the summary table probes it for keys the
    /// model does not name — `credits`, `debt`, `lastModified`, `allowsDumping`.
    pub payload: &'a JsObject,
    pub commodities: Vec<Commodity<'a>>,
    pub inventory: &'a [JsValue],
}

/// `parseMarketSnapshot` (ts:758) — `None` when the payload is not a market
/// listing, so the caller can fall back to printing it raw.
///
/// The document is parsed by the caller so that the snapshot can borrow from
/// it; a parse failure and a shape mismatch both end up as `None`, which is the
/// same fallback the TypeScript takes for either.
#[must_use]
pub fn parse_market_snapshot(document: &JsValue) -> Option<MarketSnapshot<'_>> {
    let payload = document.as_record()?;
    // An *array* named `commodities` fails here, because `asRecord` rejects
    // arrays. That is deliberate in the original.
    let raw = payload.record("commodities")?;

    let commodities: Vec<Commodity<'_>> = raw
        .iter()
        .filter_map(|(key, value)| value.as_record().map(|record| to_commodity(key, record)))
        .collect();
    if commodities.is_empty() {
        return None;
    }

    Some(MarketSnapshot { payload, commodities, inventory: payload.list("inventory") })
}

impl<'a> MarketSnapshot<'a> {
    /// The commodity with this id, compared as `f64` exactly as the original
    /// does.
    #[must_use]
    pub fn by_id(&self, id: f64) -> Option<&Commodity<'a>> {
        self.commodities.iter().find(|c| c.id == id)
    }

    /// `readCredits` (ts:2047) — `None` only when the key is *absent*.
    ///
    /// A present-but-null `credits` reads as `Some(0.0)`, which then clamps
    /// every buy to nothing through `floor(0 / price)`. Modelling this as
    /// `Option<f64>` keyed on nullness instead of presence would silently
    /// disable the affordability clamp and spend money the commander does not
    /// have. R18.
    #[must_use]
    pub fn credits(&self) -> Option<f64> {
        self.payload.present("credits").then(|| self.payload.num("credits"))
    }
}

/// `cargoUsed` (ts:1759) — total units aboard, which is what a hold capacity is
/// measured against.
#[must_use]
pub fn cargo_used(inventory: &[JsValue]) -> f64 {
    inventory
        .iter()
        .filter_map(JsValue::as_record)
        .map(|item| item.num("qty"))
        .sum()
}

/// `heldQuantity` (ts:1791) — units of one commodity aboard, matching the
/// stolen flag of the intended trade.
///
/// The name match is case-insensitive through full Unicode lowercasing, so
/// U+212A (KELVIN SIGN) in a payload would match a `k`. R32.
#[must_use]
pub fn held_quantity(inventory: &[JsValue], commodity: &Commodity<'_>, stolen: bool) -> f64 {
    let wanted = commodity.name.to_lowercase();
    inventory
        .iter()
        .filter_map(JsValue::as_record)
        .filter(|item| item.string("commodity").to_lowercase() == wanted)
        .filter(|item| item.flag("stolen") == stolen)
        .map(|item| item.num("qty"))
        .sum()
}

/// `findCommodity` (ts:1737).
///
/// Two quirks are reproduced. The separator strip (`/[\s_-]/g`) is applied to
/// the *needle only*, so typing a commodity's full name with its spaces can
/// never match a listing that has them. And two exact matches do not resolve —
/// the exact branch requires exactly one, so a tie falls through to the
/// substring branch. R93.
pub fn find_commodity<'a, 'b>(
    commodities: &'b [Commodity<'a>],
    token: &str,
) -> Result<&'b Commodity<'a>, String> {
    if !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()) {
        let wanted = js::to_number(token);
        return commodities
            .iter()
            .find(|c| c.id == wanted)
            .ok_or_else(|| format!("No commodity with id {token} at this market"));
    }

    let needle: String = token
        .chars()
        .filter(|c| !(text::is_js_whitespace(*c) || *c == '_' || *c == '-'))
        .collect::<String>()
        .to_lowercase();

    let exact: Vec<&Commodity<'a>> =
        commodities.iter().filter(|c| c.name.to_lowercase() == needle).collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }

    let partial: Vec<&Commodity<'a>> =
        commodities.iter().filter(|c| c.name.to_lowercase().contains(&needle)).collect();
    match partial.len() {
        1 => Ok(partial[0]),
        0 => Err(format!("No commodity matching \"{token}\" at this market")),
        n => {
            let names: Vec<&str> = partial.iter().take(8).map(|c| c.name).collect();
            Err(format!(
                "\"{token}\" matches {n} commodities: {}{}",
                names.join(", "),
                if n > 8 { ", ..." } else { "" },
            ))
        }
    }
}
