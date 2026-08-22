//! `parseArguments` (`game-internal-api.ts:897-955`).
//!
//! The grammar is small but none of it is conventional, so no argument-parsing
//! crate can express it: `--` is an unknown option rather than a terminator,
//! switches sometimes swallow the next token and sometimes do not, `--no-` is
//! matched before separators are stripped, and an empty token cannot become the
//! command. Each of those is a registered parity row, cited where it is
//! implemented.

use super::flag::{Flag, Literal, Table, boolean_literal, normalize};

/// What a flag holds once parsed.
///
/// The `Text`/`Bool` split mirrors the TypeScript's `string | boolean` map
/// value, and is constrained by the slot-type invariant: a slot for a flag with
/// [`Flag::takes_value`] can only ever hold `Text`, and one for a switch can
/// only hold `Bool` or `Poison`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// A switch, explicitly or implicitly set.
    Bool(bool),
    /// A value flag's raw, *untrimmed* text. Trimming happens on read.
    Text(Box<str>),
    /// A switch that swallowed `constructor` or `__proto__` \[R47\].
    ///
    /// In the TypeScript the flag map at this point holds a function or
    /// `Object.prototype` — a value that passed the `!== undefined` test in the
    /// parser and will fail `value.toLowerCase()` in `optionalSwitch`. The
    /// consequences are observable and not what a naive port produces: the
    /// token is consumed rather than left as a positional, and the run ends
    /// with an uncaught `TypeError` and exit code **1**, not the exit code 2 of
    /// a parse error.
    Poison,
}

/// A parse failure. Exits 2, printing the message and a blank line on stderr
/// and `USAGE` on stdout \[R49\].
///
/// Every variant carries the **raw name the user typed**, separators, case and
/// all — not the canonical spelling. That is the opposite of the accessor
/// errors in [`super::access`], and both rules are observable \[R46\].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArgError {
    /// `game-internal-api.ts:922`. The raw name still carries its `no-` prefix,
    /// so the message reads `--no- may only negate a switch, not --no-qty`.
    #[error("--no- may only negate a switch, not --{0}")]
    NegatedNonSwitch(Box<str>),
    /// `game-internal-api.ts:933`.
    #[error("--{0} requires a value")]
    RequiresValue(Box<str>),
    /// `game-internal-api.ts:939`.
    #[error("Unknown option --{0}")]
    UnknownOption(Box<str>),
    /// `game-internal-api.ts:942`. Only the `--switch=value` form can produce
    /// this; a bare switch followed by a non-literal simply does not consume it.
    #[error("--{0} expects true or false")]
    ExpectsBoolean(Box<str>),
}

/// A parsed command line: `ParsedArguments` (`game-internal-api.ts:792`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// The lowercased first bare word, or `"market"` when there was none.
    ///
    /// A `String` rather than an `Option`, deliberately. The TypeScript fills
    /// this slot only while `command === ""`, and assigning an *empty* token
    /// leaves that condition true — so an empty argv element is skipped over
    /// and the next bare word becomes the command. `edm "" Colonia` therefore
    /// reports `Unknown command "colonia"`, and an `Option` would have made
    /// that unrepresentable \[R45\].
    pub command: String,
    slots: Box<[Option<Value>]>,
    /// Bare words after the command, in order, unmodified.
    pub positionals: Vec<String>,
}

impl Args {
    /// The raw slot contents, or `None` when the flag never appeared.
    ///
    /// Repeats overwrite: the TypeScript uses `Map.set`, so the last occurrence
    /// of a flag wins regardless of which alias spelled it.
    #[must_use]
    pub fn get(&self, flag: Flag) -> Option<&Value> {
        self.slots[flag.index()].as_ref()
    }

    /// Iterates the populated slots in [`Flag`] discriminant order.
    pub fn iter(&self) -> impl Iterator<Item = (Flag, &Value)> {
        Flag::ALL
            .iter()
            .filter_map(|&flag| self.get(flag).map(|value| (flag, value)))
    }
}

/// Parses argv (already lossily decoded, and already stripped of the program
/// name and script path — the TypeScript passes `process.argv.slice(2)`).
///
/// Decoding is the caller's job precisely because it must be *lossy*: a
/// JavaScript `process.argv` substitutes U+FFFD for malformed bytes where
/// `std::env::args` would panic \[R55\].
pub fn parse(argv: &[String]) -> Result<Args, ArgError> {
    parse_with(argv, Table::Base)
}

/// [`parse`], against a chosen flag table.
///
/// Separated so a new command can add flag names without widening the grammar
/// every existing command is held to \[C26\]. `parse` is `parse_with(_,
/// Table::Base)` and is byte-for-byte what it always was.
pub fn parse_with(argv: &[String], table: Table) -> Result<Args, ArgError> {
    let mut slots: Box<[Option<Value>]> = vec![None; Flag::COUNT].into_boxed_slice();
    let mut positionals: Vec<String> = Vec::new();
    let mut command = String::new();

    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].as_str();
        index += 1;

        // The one short flag, and it is case-sensitive: `-H` falls through to
        // the bare-word branch below and becomes the command \[R38\].
        if token == "-h" {
            slots[Flag::Help.index()] = Some(Value::Bool(true));
            continue;
        }
        // `-v`, and only under the extended table. The ported grammar knows
        // exactly one single-dash token, `-h`; every other `-x` is a positional
        // so that `--qty -5` can take a negative value \[R44\]. Adding a second
        // one to the base table would change what `edm market -v` means, which
        // is currently "search for a system called -v".
        if token == "-v" && table == Table::Extended {
            slots[Flag::Verbose.index()] = Some(Value::Bool(true));
            continue;
        }

        let Some(body) = token.strip_prefix("--") else {
            // Assigning an empty token leaves `command === ""` true, so the
            // slot stays open and the *next* bare word takes it \[R45\].
            if command.is_empty() {
                command = token.to_lowercase();
            } else {
                positionals.push(token.to_owned());
            }
            continue;
        };

        // Only the *first* `=` splits, so `--item=a=b` carries the value
        // `a=b` \[R42\]. `=` is ASCII, so a byte index is a UTF-16 index here.
        let (raw_name, equals_value) = match body.find('=') {
            Some(at) => (&body[..at], Some(&body[at + 1..])),
            None => (body, None),
        };

        // `/^no-/i` against the **raw** name, before separators are stripped:
        // `--no-json` negates but `--no_json` is an unknown option, because by
        // the time `no_json` loses its underscore the test has already been
        // made. The regex has no `u` flag, so its case folding is ASCII \[R40\].
        let negated = raw_name.len() >= 3 && raw_name.as_bytes()[..3].eq_ignore_ascii_case(b"no-");
        let stem = if negated { &raw_name[3..] } else { raw_name };
        let canonical = Flag::resolve_in(&normalize(stem), table);

        if negated {
            match canonical {
                Some(flag) if !flag.takes_value() => {
                    // Any `=value` on a negation is silently discarded \[R40\].
                    slots[flag.index()] = Some(Value::Bool(false));
                }
                _ => return Err(ArgError::NegatedNonSwitch(raw_name.into())),
            }
            continue;
        }

        match canonical {
            Some(flag) if flag.takes_value() => {
                if let Some(value) = equals_value {
                    // `--qty=` stores the empty string rather than failing; it
                    // is the *accessor* that later treats blank as absent and
                    // falls through to the environment \[R43\].
                    slots[flag.index()] = Some(Value::Text(value.into()));
                    continue;
                }
                // A single leading `-` is accepted, so `--qty -5` takes `-5` as
                // its value; only a `--` prefix refuses \[R44\].
                match argv.get(index) {
                    Some(next) if !next.starts_with("--") => {
                        slots[flag.index()] = Some(Value::Text(next.as_str().into()));
                        index += 1;
                    }
                    _ => return Err(ArgError::RequiresValue(raw_name.into())),
                }
            }
            Some(flag) => {
                if let Some(value) = equals_value {
                    let Some(literal) = boolean_literal(value) else {
                        return Err(ArgError::ExpectsBoolean(raw_name.into()));
                    };
                    slots[flag.index()] = Some(literal_value(literal));
                    continue;
                }
                // A bare switch consumes the next token if and only if it reads
                // as a boolean literal, which is why `--detail 1` eats the `1`
                // but `--dry-run Colonia` leaves `Colonia` to be a
                // positional \[R39\].
                match argv.get(index).and_then(|next| boolean_literal(next)) {
                    Some(literal) => {
                        slots[flag.index()] = Some(literal_value(literal));
                        index += 1;
                    }
                    None => slots[flag.index()] = Some(Value::Bool(true)),
                }
            }
            // A bare `--` lands here with an empty `raw_name`, which is how it
            // becomes `Unknown option --` rather than an options
            // terminator \[R38\].
            None => return Err(ArgError::UnknownOption(raw_name.into())),
        }
    }

    Ok(Args {
        // `command || "market"` (`game-internal-api.ts:954`).
        command: if command.is_empty() {
            "market".to_owned()
        } else {
            command
        },
        slots,
        positionals,
    })
}

fn literal_value(literal: Literal) -> Value {
    match literal {
        Literal::Bool(b) => Value::Bool(b),
        Literal::Poison => Value::Poison,
    }
}
