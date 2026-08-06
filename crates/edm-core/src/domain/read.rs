//! The coercions every payload read goes through.
//!
//! `game-internal-api.ts` never trusts the game-internal API's shape. It reads through
//! four total functions that substitute a default rather than failing, which is
//! why a drifted payload degrades into empty tables instead of an exception.
//! Reproducing them exactly matters more than it looks: several of the defaults
//! are load-bearing further down.

use crate::js::json::{JsObject, JsValue};
use crate::js::text;

/// The reading half of `game-internal-api.ts`'s payload helpers (ts:562-580).
pub trait Read {
    /// `readNumber` — a finite JSON number, else `0`.
    ///
    /// Note what falls through to zero: a missing key, `null`, a numeric
    /// *string*, and a bool. There is no coercion; `"5"` reads as `0`.
    fn num(&self, key: &str) -> f64;

    /// `readString` — a JSON string, else `""`.
    fn string(&self, key: &str) -> &str;

    /// `readBoolean` — `=== true || === 1`, and nothing else.
    ///
    /// Not to be confused with the `> 0` test the commodity flags use; see
    /// [`Read::positive`]. R17.
    fn flag(&self, key: &str) -> bool;

    /// `readNumber(k) > 0`, which is how `consumer`, `producer` and `rare` are
    /// derived.
    ///
    /// The consequence is worth stating: a payload carrying `"rare": true`
    /// reads as **false**, because `readNumber` turns a bool into `0`. That is
    /// the TypeScript's behaviour and it is reproduced deliberately. R17.
    fn positive(&self, key: &str) -> bool;

    /// `asRecord(source[key])` — an object, and specifically not an array.
    fn record(&self, key: &str) -> Option<&JsObject>;

    /// `Array.isArray(source[key]) ? source[key] : []`.
    fn list(&self, key: &str) -> &[JsValue];

    /// `key in source` — presence, not non-nullness. R18.
    fn present(&self, key: &str) -> bool;
}

impl Read for JsObject {
    fn num(&self, key: &str) -> f64 {
        match self.get(key) {
            Some(JsValue::Num(n)) if n.is_finite() => *n,
            _ => 0.0,
        }
    }

    fn string(&self, key: &str) -> &str {
        match self.get(key) {
            Some(JsValue::Str(s)) => s,
            _ => "",
        }
    }

    fn flag(&self, key: &str) -> bool {
        match self.get(key) {
            Some(JsValue::Bool(true)) => true,
            Some(JsValue::Num(n)) => *n == 1.0,
            _ => false,
        }
    }

    fn positive(&self, key: &str) -> bool {
        self.num(key) > 0.0
    }

    fn record(&self, key: &str) -> Option<&JsObject> {
        self.get(key).and_then(JsValue::as_object)
    }

    fn list(&self, key: &str) -> &[JsValue] {
        self.get(key).and_then(JsValue::as_array).unwrap_or(&[])
    }

    fn present(&self, key: &str) -> bool {
        self.has(key)
    }
}

/// JavaScript's `a || b` over a chain of numbers.
///
/// Falsy means `0`, `-0` or `NaN` — so a legitimate `-5` or `Infinity` stops
/// the chain while a zero keeps falling. Used by the two id-resolution sites,
/// which differ: `toCommodity` (ts:603) has three terms and `readMarketPoints`
/// (ts:2734) has only two. R16.
#[must_use]
pub fn or_else(value: f64, fallback: impl FnOnce() -> f64) -> f64 {
    if crate::js::truthy(value) { value } else { fallback() }
}

/// JavaScript's `a || b` over strings, where only `""` is falsy.
#[must_use]
pub fn or_else_str<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

/// `String.prototype.trim` applied to a field read out of a payload.
#[must_use]
pub fn trimmed(value: &str) -> &str {
    text::js_trim(value)
}
