//! The accessor family (`market-request.ts:980-1027`).
//!
//! Nothing in the program reads a flag directly. Six small functions sit
//! between the parse result and every command, and they carry more behaviour
//! than their names suggest: which flags fall back to the environment, when
//! blank counts as absent, and which spelling appears in a complaint.

use crate::js;

use super::flag::Flag;
use super::parse::{Args, Value};

/// The message a poisoned switch produces when `optionalSwitch` reaches
/// `value.toLowerCase()` on a function or on `Object.prototype` \[R47\].
///
/// The text belongs to the JavaScript engine, not to us, and it is the whole
/// observable difference between exit 1 and exit 2 for
/// `edm --detail constructor`.
///
/// Measured, not guessed: blessed from bun 1.2.3 by running the real
/// `parseArguments`/`optionalSwitch` over `["market", "--detail",
/// "constructor"]` and over `[..., "__proto__"]`, which produce the same
/// message. The parenthetical is `JavaScriptCore` diagnostic detail and could
/// move on a Bun upgrade, so tests compare against this constant rather than
/// against a literal.
pub const POISON_TYPE_ERROR: &str =
    "value.toLowerCase is not a function. (In 'value.toLowerCase()', 'value.toLowerCase' is undefined)";

/// An error thrown while *reading* an option, as opposed to while parsing one.
///
/// These reach the user through `main`'s catch, which prints `error.message`
/// alone and sets exit code 1 \[R82\]. The message is the entire payload —
/// there is no cause chain to preserve, because the TypeScript has none.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CliError(Box<str>);

impl CliError {
    /// The message, exactly as it will be printed.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        Self(message.into_boxed_str())
    }
}

/// The process environment, sampled once.
///
/// A `Vec` of pairs rather than a map, and **first-wins** on insertion. A
/// duplicate name is possible in a raw environment block, and `getenv` returns
/// the first match while collecting into a `HashMap` would keep the last
/// \[R55\]. Values arrive already lossily decoded, for the same reason argv
/// does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvSnapshot(Vec<(String, String)>);

impl EnvSnapshot {
    /// An empty environment.
    #[must_use]
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Snapshots name/value pairs in iteration order, keeping the first
    /// binding of each name.
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut entries: Vec<(String, String)> = Vec::new();
        for (name, value) in pairs {
            let name = name.into();
            if entries.iter().any(|(existing, _)| *existing == name) {
                continue;
            }
            entries.push((name, value.into()));
        }
        Self(entries)
    }

    /// `process.env[name]`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

/// Reads options out of a parsed command line and an environment snapshot.
#[derive(Clone, Copy, Debug)]
pub struct Cli<'a> {
    args: &'a Args,
    env: &'a EnvSnapshot,
}

impl<'a> Cli<'a> {
    /// Binds a parse result to the environment it will be read against.
    #[must_use]
    pub fn new(args: &'a Args, env: &'a EnvSnapshot) -> Self {
        Self { args, env }
    }

    /// The parse result being read.
    #[must_use]
    pub fn args(&self) -> &'a Args {
        self.args
    }

    /// `optionalValue` (`market-request.ts:980`).
    ///
    /// Three things here are easy to get wrong. The stored value is trimmed on
    /// *read*, not on parse. A present-but-blank flag is treated as absent and
    /// falls through to the environment, so `--qty= ` behaves exactly like no
    /// `--qty` at all \[R56\]. And the environment name is itself tested for
    /// truthiness, so an empty name means "no environment fallback".
    ///
    /// It cannot fail. `market-request.ts:986` throws
    /// `{display} requires a value` when the slot holds a boolean, which the
    /// slot-type invariant makes unconstructible: every caller passes a flag
    /// with [`Flag::takes_value`], and only those flags can hold text \[C18\].
    #[must_use]
    pub fn optional_value(&self, flag: Flag, environment: Option<&str>) -> Option<&'a str> {
        debug_assert!(flag.takes_value(), "C18: optionalValue is only ever called on a VALUE_FLAG");

        if let Some(Value::Text(raw)) = self.args.get(flag) {
            let trimmed = js::text::js_trim(raw);
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }

        let name = environment.filter(|name| !name.is_empty())?;
        let value = js::text::js_trim(self.env.get(name)?);
        // `?.trim() || undefined` — a whitespace-only variable is not a value.
        if value.is_empty() { None } else { Some(value) }
    }

    /// `requireValue` (`market-request.ts:990`).
    ///
    /// The two messages differ by whether the option has an environment
    /// fallback to mention.
    pub fn require_value(&self, flag: Flag, environment: Option<&str>) -> Result<&'a str, CliError> {
        if let Some(value) = self.optional_value(flag, environment) {
            return Ok(value);
        }
        let display = flag.display();
        Err(match environment.filter(|name| !name.is_empty()) {
            // ts:995
            Some(name) => format!("Missing {display} (or {name} in the environment)").into(),
            // ts:996
            None => format!("Missing required option {display}").into(),
        })
    }

    /// `optionalNumber` (`market-request.ts:1002`).
    ///
    /// No environment fallback: the TypeScript passes none, so `--qty` reads
    /// the flag alone even though the same flag's sibling accessors would
    /// consult the environment.
    pub fn optional_number(&self, flag: Flag) -> Result<Option<f64>, CliError> {
        match self.optional_value(flag, None) {
            // The complaint names the canonical spelling, not the alias the
            // user typed \[R46\].
            Some(value) => js::parse_unsigned_integer(flag.display(), value).map(Some).map_err(CliError::from),
            None => Ok(None),
        }
    }

    /// `optionalDecimal` (`market-request.ts:1008`) — for values like
    /// `--interval` that may be fractional.
    ///
    /// The conversion is `Number(string)`, not a Rust float parse, so it
    /// accepts `0x10`, ` 1.5 ` and `Infinity` and rejects `inf` and `1_0`
    /// \[R10\]. `Infinity` then fails the finiteness test.
    pub fn optional_decimal(&self, flag: Flag) -> Result<Option<f64>, CliError> {
        let Some(raw) = self.optional_value(flag, None) else {
            return Ok(None);
        };
        let value = js::to_number(raw);
        if !value.is_finite() || value <= 0.0 {
            // ts:1012
            return Err(format!("{} must be a positive number", flag.display()).into());
        }
        Ok(Some(value))
    }

    /// `optionalSwitch` (`market-request.ts:1016`).
    ///
    /// The only accessor that can fail on a switch, and it fails for one
    /// reason: the slot was poisoned by a token that resolved through
    /// `Object.prototype` \[R47\].
    pub fn optional_switch(&self, flag: Flag) -> Result<Option<bool>, CliError> {
        debug_assert!(!flag.takes_value(), "C18: optionalSwitch is only ever called on a BOOLEAN_FLAG");

        #[expect(
            clippy::match_same_arms,
            reason = "the Text arm is unreachable, not a duplicate of None; merging them would \
                      hide which of the two answers C18 is claiming"
        )]
        match self.args.get(flag) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(Value::Poison) => Err(CliError::from(POISON_TYPE_ERROR.to_owned())),
            // `market-request.ts:1021` re-reads a *string* slot as a boolean
            // literal and throws `{display} expects true or false` when it is
            // not one. Unconstructible for the same reason as ts:986: text only
            // ever lands in a value flag's slot \[C18\].
            Some(Value::Text(_)) => Ok(None),
        }
    }

    /// `switchValue` (`market-request.ts:1025`).
    pub fn switch_value(&self, flag: Flag, fallback: bool) -> Result<bool, CliError> {
        Ok(self.optional_switch(flag)?.unwrap_or(fallback))
    }
}
