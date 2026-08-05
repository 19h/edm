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
pub mod usage;

pub use access::{Cli, CliError, EnvSnapshot, POISON_TYPE_ERROR};
pub use flag::{Flag, Literal, boolean_literal, normalize};
pub use parse::{ArgError, Args, Value, parse};
pub use usage::usage;

/// The commands `main` will dispatch on (`market-request.ts:3148`).
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
