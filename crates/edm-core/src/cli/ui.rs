//! `edm ui` — the interactive terminal front end \[C53\].
//!
//! Kept apart from [`crate::cli::usage`] for the reason C25 gives: that text is
//! pinned byte for byte against captured Bun output and a new command must not
//! add a character to it.

use crate::cli::access::{Cli, CliError};
use crate::cli::flag::Flag;
use crate::js;

/// Seconds between background re-prices when `--follow` is not given.
///
/// A minute: a pinned route's markets are a handful of requests, carrier
/// verdicts cache for fifteen anyway, and a commander mid-flight does not need
/// a price to be fresher than the jump they are making.
pub const DEFAULT_REFRESH_SECONDS: f64 = 60.0;

/// Everything `edm ui` needs from the command line.
///
/// Everything a *search* needs is not here: the form inside the UI builds a
/// `route` or `sell` argv and parses it exactly as the command line would, so
/// every flag those commands take is taken there, with the same validation.
#[derive(Clone, Debug, PartialEq)]
pub struct UiConfig {
    /// Seconds between background re-prices of a pinned route (`--follow`).
    pub refresh_seconds: f64,
    /// The session request ceiling, enforced live (`--max-requests`).
    pub max_requests: f64,
    /// `--cache-dir`, when given.
    pub cache_dir: Option<String>,
    /// `--from-file <path>`: where pins are kept, instead of the cache root.
    pub pins_file: Option<String>,
    /// `--yes`: searches above the confirmation threshold proceed without a
    /// modal.
    pub confirmed: bool,
}

/// Everything the command needs, read once.
pub fn ui_config(cli: &Cli<'_>) -> Result<UiConfig, CliError> {
    Ok(UiConfig {
        refresh_seconds: super::config::follow_seconds(cli)?.unwrap_or(DEFAULT_REFRESH_SECONDS),
        max_requests: cli
            .optional_number(Flag::MaxRequests)?
            .unwrap_or(crate::spend::DEFAULT_MAX_REQUESTS),
        cache_dir: cli.optional_value(Flag::CacheDir, None).map(ToOwned::to_owned),
        pins_file: cli.optional_value(Flag::FromFile, None).map(ToOwned::to_owned),
        confirmed: cli.switch_value(Flag::Yes, false)?,
    })
}

/// The help text.
#[must_use]
pub fn ui_usage() -> String {
    let n = |value: f64| js::format_integer(value);
    format!(
        "edm ui — route, survey and sell, interactively

Usage
  edm ui [options]

A full-screen front end over `edm route`, `edm route --quick` and `edm sell`.
Fill in a search, watch it run, pin the route you mean to fly, and the pinned
route alone is kept fresh in the background: its prices, stock and demand, the
docking access of any carrier on it, and where your ship is relative to it.

This command plans only. It shows the `edm trade` commands for a route and can
copy them to the clipboard; nothing here transacts.

Screens
  1 Search     the form; Enter runs it, Tab moves between fields
  2 Results    the ranking; Enter pins a route and opens it, p pins in place
  3 Detail     one pinned route, refreshed on its own; R refreshes now
  4 Pins       every pinned route and when it is next due
  5 Sell       the disposal plan for what is aboard
  L Log        what the pipeline said, as it would have on the console
  ? Help       the keys for the screen you are on

Refreshing
  --follow <s>             seconds between background re-prices, default
                           {refresh}, minimum {min_follow}. One pinned route
                           costs its markets per round
  --max-requests <n>       the session ceiling, default {max_requests}. Counted
                           live across every search and refresh; nothing is
                           sent above it
  --yes                    searches above {confirm} requests proceed without
                           asking

Files
  --cache-dir <path>       the price and access cache, as `edm route`
  --from-file <path>       where pins are kept, default beside the cache
  Every search inside the UI takes the flags `edm route` and `edm sell` take,
  and is checked exactly as those commands check them.

Examples
  edm ui
  edm ui --follow 45 --max-requests 500
",
        refresh = n(DEFAULT_REFRESH_SECONDS),
        min_follow = n(super::config::MIN_FOLLOW_SECONDS),
        max_requests = n(crate::spend::DEFAULT_MAX_REQUESTS),
        confirm = n(crate::spend::CONFIRM_THRESHOLD),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::access::EnvSnapshot;

    fn config(argv: &[&str]) -> Result<UiConfig, CliError> {
        let owned: Vec<String> = argv.iter().map(|a| (*a).to_owned()).collect();
        let parsed = crate::cli::parse_dispatch(&owned);
        let args = parsed.route.expect("ui must parse");
        let env = EnvSnapshot::empty();
        ui_config(&Cli::new(&args, &env))
    }

    #[test]
    fn the_help_says_what_the_command_will_not_do() {
        let text = ui_usage();
        assert!(text.contains("nothing here transacts"), "{text}");
        assert!(text.contains("edm ui"), "{text}");
    }

    #[test]
    fn the_refresh_interval_is_follow_with_the_same_floor() {
        assert_eq!(config(&["ui"]).unwrap().refresh_seconds, DEFAULT_REFRESH_SECONDS);
        assert_eq!(config(&["ui", "--follow", "45"]).unwrap().refresh_seconds, 45.0);
        assert!(config(&["ui", "--follow", "5"]).is_err());
        assert_eq!(config(&["ui", "--max-requests", "50"]).unwrap().max_requests, 50.0);
        assert!(config(&["ui", "--yes"]).unwrap().confirmed);
    }
}
