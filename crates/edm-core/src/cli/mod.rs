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
pub mod feed;
pub mod flag;
pub mod parse;
pub mod route_usage;
pub mod usage;
pub mod sell;
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
pub const EXTENDED_COMMANDS: [&str; 4] = ["route", "eddn", "vendor", "sell"];

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
    /// The extended table's own complaint, when argv opens with an extension
    /// command and that reading failed.
    ///
    /// Without it, `edm route --quick --radius 200` reports `Unknown option
    /// --quick`, which is false: `--quick` is a route option that was simply
    /// given no value. The base table cannot say so — route-only names are
    /// invisible to it by construction \[C26\] — so the base error is the
    /// wrong answer to print, in the same way and for the same reason that
    /// `parsed.route` is consulted before it.
    pub misread: Option<ArgError>,
    /// Which extension the misread belongs to, when there is one.
    ///
    /// The complaint comes from the extended table, so the help that explains
    /// it has to come from the same place. Printing the ported `usage()` after
    /// a route-only flag's error tells the reader the flag does not exist --
    /// reported from a live run where `--follow` with no value printed the Bun
    /// help, which has never mentioned it \[C45\].
    pub misread_command: Option<String>,
}

/// The help text a command should print, ported or extended.
#[must_use]
pub fn usage_for(command: &str) -> String {
    match command {
        "eddn" => super::cli::feed::feed_usage(),
        "vendor" => super::cli::vendor::vendor_usage(),
        "sell" => super::cli::sell::sell_usage(),
        "route" => route_usage(),
        _ => usage(),
    }
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
    let extended = parse_with(argv, Table::Extended);
    // Keyed on the *first token* rather than on the parsed command, because
    // there is no parsed command when the parse failed. That under-fires —
    // `edm --json route --quick` keeps the base message — and never over-fires,
    // which is the direction that matters: a leading bare word is always the
    // command, so a ported command's argv can never reach this and no pinned
    // message can change.
    let (misread, misread_command) = match (&extended, argv.first()) {
        (Err(error), Some(first)) if is_extended_command(&first.to_ascii_lowercase()) => {
            (Some(error.clone()), Some(first.to_ascii_lowercase()))
        }
        _ => (None, None),
    };
    let route = extended
        .ok()
        .filter(|args| is_extended_command(&args.command));
    Parsed {
        base: parse(argv),
        route,
        misread,
        misread_command,
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_owned()).collect()
    }

    /// The complaint and the help have to come from the same table. A
    /// route-only flag's error printed under the ported usage tells the reader
    /// the flag does not exist -- which is what `--follow` with no value did
    /// \[C45\].
    #[test]
    fn a_misread_extension_names_the_command_whose_help_explains_it() {
        for (tokens, expected) in [
            (&["route", "--follow"][..], "route"),
            (&["sell", "--worth"][..], "sell"),
            (&["vendor", "--radius"][..], "vendor"),
            (&["eddn", "--rps"][..], "eddn"),
        ] {
            let parsed = parse_dispatch(&argv(tokens));
            assert_eq!(
                parsed.misread_command.as_deref(),
                Some(expected),
                "{tokens:?}"
            );
            assert!(
                usage_for(expected).contains(&format!("edm {expected}"))
                    || expected == "eddn",
                "{expected} must have its own help"
            );
        }
    }

    /// And a ported command keeps the ported help, byte for byte: R49 pins it.
    #[test]
    fn a_ported_flag_error_carries_no_extension_command() {
        let parsed = parse_dispatch(&argv(&["trade", "--qty"]));
        assert!(parsed.misread_command.is_none());
        assert!(parsed.misread.is_none());
    }

    /// A route-only flag left without its value used to be reported as unknown,
    /// which is the one thing it is not. The base table cannot see route names
    /// at all [C26], so its error is the wrong reading of this argv.
    #[test]
    fn a_route_flag_missing_its_value_is_not_reported_as_unknown() {
        for tokens in [
            &["route", "--quick", "--radius", "200", "--eddn", "FROG"][..],
            &["route", "--radius", "--top", "5", "Sol"][..],
            &["vendor", "--radius", "--json"][..],
        ] {
            let parsed = parse_dispatch(&argv(tokens));
            assert!(
                parsed.route.is_none(),
                "{tokens:?} does not parse as an extension"
            );
            let misread = parsed
                .misread
                .expect("the extended reading explains itself");
            assert!(
                misread.to_string().ends_with("requires a value"),
                "{tokens:?}: {misread}"
            );
        }
    }

    /// And a name that really is unknown keeps saying so, in both tables.
    #[test]
    fn an_unknown_name_is_still_unknown_under_either_table() {
        let parsed = parse_dispatch(&argv(&["route", "--bogus", "5", "Sol"]));
        assert_eq!(
            parsed.misread.map(|error| error.to_string()),
            Some("Unknown option --bogus".to_owned())
        );
    }

    /// The guard is the leading token, so no ported command's message can move.
    /// `market` is not an extension, so its base error stands whatever the
    /// extended table would have made of the same argv.
    #[test]
    fn a_ported_commands_message_is_never_replaced() {
        for tokens in [
            &["market", "--radius", "--top", "5"][..],
            &["market", "--bogus"][..],
            &["trade", "--qty"][..],
        ] {
            assert!(
                parse_dispatch(&argv(tokens)).misread.is_none(),
                "{tokens:?} is a ported command line"
            );
        }
    }
}
