//! The command line: a hand-written parser for a grammar no off-the-shelf
//! crate can express, and the typed configuration built from it.
//!
//! Reading the whole command line is two passes with two different failure
//! modes, and keeping them apart is the point of this module's shape:
//!
//! 1. [`parse`] turns argv into [`Args`] — a command, a slot per [`Flag`], and
//!    the leftover bare words. It fails with [`ArgError`], which the binary
//!    reports on stderr followed by a blank line, prints [`usage`] on stdout
//!    after, and exits **2** \[R49\].
//! 2. [`Cli`] reads those slots on demand, coercing and validating as each
//!    command asks for what it needs. It fails with [`CliError`], which is an
//!    ordinary thrown error: message printed alone, exit **1** \[R82\].
//!
//! The order in which step 2 happens is itself observable, because a command
//! that fails on its fourth option has already performed the side effects of
//! the first three \[R50\]. Nothing here reads eagerly.
//!
//! Neither the environment nor argv is fetched by this crate. Both arrive as
//! already lossily decoded `String`s, because a JavaScript program substitutes
//! U+FFFD where `std::env::args` panics \[R55\].

pub mod access;
pub mod config;
pub mod flag;
pub mod parse;
pub mod feed;
pub mod route_usage;
pub mod usage;
pub mod vendor;

pub use access::{Cli, CliError, EnvSnapshot, POISON_TYPE_ERROR};
pub use flag::{Flag, Literal, Table, boolean_literal, normalize};
pub use parse::{ArgError, Args, Value, parse, parse_with};
pub use route_usage::route_usage;
pub use usage::usage;

/// The commands `main` will dispatch on (`game-internal-api.ts:3148`).
///
/// Checked *after* both the `help` command and the `--help` switch, which is
/// why `edm bogus --help` prints the help text and exits 0 instead of
/// complaining about `bogus` \[R48\].
pub const KNOWN_COMMANDS: [&str; 4] = ["market", "list", "markets", "trade"];

/// Is this the command of a run that will actually do something?
#[must_use]
pub fn is_known_command(command: &str) -> bool {
    KNOWN_COMMANDS.contains(&command)
}

/// Commands this port adds, which the TypeScript does not have \[C25, C33, C35\].
///
/// Kept disjoint from [`KNOWN_COMMANDS`] rather than appended to it, because
/// that constant's contents and R48's ordering around it are both pinned by the
/// parity harness. Bun rejects each extension as an unknown command.
pub const EXTENDED_COMMANDS: [&str; 3] = ["route", "eddn", "vendor"];

#[must_use]
pub fn is_extended_command(command: &str) -> bool {
    EXTENDED_COMMANDS.contains(&command)
}

/// Both readings of one argv.
///
/// A command line has to be parsed before anyone knows which command it is, and
/// which command it is decides which grammar applies. So parse it twice and
/// pick — cheaply, since parsing is a linear walk over a handful of tokens.
#[derive(Debug)]
pub struct Parsed {
    /// What [`parse`] produced. Always computed, and always what a ported
    /// command uses — including its error, verbatim.
    pub base: Result<Args, ArgError>,
    /// `Some` only when the extended parse succeeded and named an extension.
    pub route: Option<Args>,
}

/// Parses against both tables and decides which reading governs.
///
/// The rule: **use the extended parse if and only if its command is exactly one
/// of [`EXTENDED_COMMANDS`]; otherwise use the base parse verbatim.**
///
/// The interesting case is `edm --radius route Sol`. Under the extended table
/// `--radius` swallows `route`, so the command becomes `Sol`, so the rule falls
/// back to base — which correctly reports `Unknown option --radius`. A rule
/// keyed on "did the extended parse succeed" instead would have silently
/// accepted it.
#[must_use]
pub fn parse_dispatch(argv: &[String]) -> Parsed {
    let route = parse_with(argv, Table::Extended)
        .ok()
        .filter(|args| is_extended_command(&args.command));
    Parsed { base: parse(argv), route }
}
