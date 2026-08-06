//! `edm eddn <kind>` — relay data to EDDN on purpose, rather than as a side
//! effect of a sweep \[C33\].
//!
//! `route --eddn` publishes what a *route search* happened to read, which is
//! the wrong shape for the job of filling in a region whose data has gone
//! stale: it is bounded by a radius, filtered to berths a big ship can use, and
//! it spends a search on markets it is only visiting to publish them.
//!
//! This is the other direction. You name what to refresh — a market id, or a
//! file of system names — and every market under it is read and relayed.
//!
//! Only `market` exists today. It is a *word*, not a flag, because the thing
//! after `eddn` selects which of Frontier's endpoints is read, and the ones
//! that might follow (shipyard, outfitting) are different requests returning
//! different documents, not options on this one.

use super::access::{Cli, CliError};
use super::flag::Flag;
use crate::js;
use crate::js::text;

/// Which endpoint an `eddn` run reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// `/2.0/elite/market/list` — commodity prices, EDDN's `commodity/3`.
    Market,
}

impl Feed {
    /// The word that selects it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Market => "market",
        }
    }
}

/// One thing to refresh.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A market, named directly.
    Market(f64),
    /// A system, whose markets are looked up.
    System(String),
}

/// What an `edm eddn` run was asked to do.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedConfig {
    pub feed: Feed,
    pub targets: Vec<Target>,
    /// `--eddn-test`: the gateway accepts it and does not relay it onward.
    pub test: bool,
    /// How long a market stays suppressed after this machine relayed it.
    pub eddn_max_age_minutes: f64,
    /// Messages per second to EDDN, paced apart from the Companion API.
    pub eddn_rate_per_second: f64,
    pub rate_per_second: f64,
    pub workers: u32,
    pub deadline_seconds: f64,
    pub max_requests: f64,
    pub confirmed: bool,
    pub dry_run: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub cache_dir: Option<String>,
}

/// `edm eddn --help`.
///
/// Its own text for the same reason `route`'s is: the ported usage is pinned
/// byte for byte against captured Bun output and a new command must not add a
/// character to it \[C25\].
#[must_use]
pub fn feed_usage() -> String {
    use super::config::{DEFAULT_EDDN_MAX_AGE_MINUTES, DEFAULT_RPS};
    format!(
        "edm eddn — publish market data to EDDN on purpose

Usage
  edm eddn market (--market-id <id> | --from-file <path>)

Reads every named market live from the Companion API and relays it to EDDN.
Built for filling in systems whose data has gone stale: you say which, rather
than hoping a route search happens to pass through them.

A listing served from the local price cache is never relayed, at any age — it
was read earlier, and republishing it would stamp that old reading with the
current time. So this always reads live.

What to import
  market                   commodity prices. The only kind so far

Which markets
  --market-id <id>         one market
  --from-file <path>       a text file, one target per line. A line of digits
                           is a market id; anything else is a system name, and
                           every market in that system is read. Blank lines and
                           # comments are skipped, order is kept, repeats are
                           dropped. No station filter: a settlement's prices are
                           as worth publishing as a starport's

Sending
  --eddn-test              the gateway accepts these and does not relay them on
  --eddn-max-age <m>       suppress a repeat of the same market for this long,
                           default {eddn_age}
  --rps <n>                Companion API requests per second, default {rps}
  --eddn-rps <n>           messages per second to EDDN, default {eddn_rps}, and
                           paced separately: they ride inside the market poll,
                           so --rps used to set this too. A 565-market import at
                           40/s earned this host a 403 from the gateway's proxy
  --concurrency <n>        workers behind the pacer, default 5
  --deadline <s>           how long the whole run may take
  --max-requests <n>       ceiling; above it nothing is sent
  --dry-run                resolve the list, say how many markets, and stop
  --verbose, -v            say what the pacer is doing

Examples
  edm eddn market --market-id 4306502403
  edm eddn market --from-file stale-systems.txt
  edm eddn market --from-file stale-systems.txt --dry-run
",
        eddn_age = js::format_integer(DEFAULT_EDDN_MAX_AGE_MINUTES),
        rps = js::format_integer(DEFAULT_RPS),
        eddn_rps = js::format_integer(super::config::DEFAULT_EDDN_RPS),
    )
}

/// The word after `eddn`.
///
/// Required and positional. A default would be a guess about which of
/// Frontier's endpoints the user meant, and the guess would be silent.
pub fn feed_of(positionals: &[String]) -> Result<Feed, CliError> {
    let Some(word) = positionals.first() else {
        return Err("eddn needs something to import: eddn market".to_owned().into());
    };
    match word.to_lowercase().as_str() {
        "market" => Ok(Feed::Market),
        other => Err(format!("eddn cannot import \"{other}\"; the kinds are: market").into()),
    }
}

/// One line of a `--from-file` list.
///
/// **All digits is a market id; anything else is a system name.** Elite system
/// names are never bare numbers — they are `Sol`, `Col 285 Sector HB-V c3-0`,
/// `LHS 1939` — so the two cannot be confused, and the rule needs no prefix or
/// flag to disambiguate.
///
/// Blank lines and `#` comments are skipped so a list can be annotated.
#[must_use]
pub fn parse_line(line: &str) -> Option<Target> {
    let line = text::js_trim(line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    if line.bytes().all(|b| b.is_ascii_digit()) {
        // Through `to_number`, so the id is read the same way every other
        // numeric argument in this program is.
        return Some(Target::Market(js::to_number(line)));
    }
    Some(Target::System(line.to_owned()))
}

/// Every target in a file, in order, deduplicated.
///
/// Order is kept because a hand-written list has an order the writer meant —
/// the systems they care about most are usually first, and a run that is cut
/// short by `--deadline` should have done those.
#[must_use]
pub fn parse_list(text: &str) -> Vec<Target> {
    let mut targets = Vec::new();
    for line in text.lines() {
        let Some(target) = parse_line(line) else { continue };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    targets
}

/// Read the command line. The file itself is read by the caller, which owns the
/// filesystem.
pub fn feed_config(cli: &Cli<'_>, file_contents: Option<&str>) -> Result<FeedConfig, CliError> {
    use super::config::{
        DEFAULT_DEADLINE_SECONDS, DEFAULT_EDDN_MAX_AGE_MINUTES, DEFAULT_EDDN_RPS, DEFAULT_RPS,
    };
    use crate::consts::{DEFAULT_CONCURRENCY, MAX_CONCURRENCY};
    use crate::spend::DEFAULT_MAX_REQUESTS;

    let args = cli.args();
    let feed = feed_of(&args.positionals)?;

    // The kind word is a positional, so anything after it would be a second
    // one — which is a typo, not a target list, because targets come from
    // `--market-id` or `--from-file`.
    if args.positionals.len() > 1 {
        return Err(format!(
            "eddn {} takes no other arguments; name what to import with --market-id or --from-file",
            feed.as_str()
        )
        .into());
    }

    // The **flag**, with no environment fallback. `MARKET_ID` in the
    // environment is a default for the ported commands, not a statement about
    // this one — and reading it here made `--from-file` unusable for anyone who
    // has one set, which is everyone who has ever run `edm market`. The
    // fallback is applied below, only when nothing else named a target.
    let flagged = cli.optional_value(Flag::MarketId, None);
    let targets = match (flagged, file_contents) {
        // Both is a contradiction, not a union: one of them is a mistake and
        // guessing which would run the wrong list.
        (Some(_), Some(_)) => {
            return Err("--market-id and --from-file are alternatives; give one".to_owned().into());
        }
        (Some(raw), None) => {
            vec![Target::Market(js::parse_unsigned_integer("--market-id", raw)?)]
        }
        (None, Some(text)) => {
            let targets = parse_list(text);
            if targets.is_empty() {
                return Err("--from-file held no system names or market ids".to_owned().into());
            }
            targets
        }
        (None, None) => match cli.optional_value(Flag::MarketId, Some("MARKET_ID")) {
            Some(raw) => vec![Target::Market(js::parse_unsigned_integer("MARKET_ID", raw)?)],
            None => {
                return Err("eddn needs --market-id <id> or --from-file <path>".to_owned().into());
            }
        },
    };

    Ok(FeedConfig {
        feed,
        targets,
        test: cli.switch_value(Flag::EddnTest, false)?,
        eddn_max_age_minutes: cli
            .optional_decimal(Flag::EddnMaxAge)?
            .unwrap_or(DEFAULT_EDDN_MAX_AGE_MINUTES),
        eddn_rate_per_second: cli
            .optional_decimal(Flag::EddnRps)?
            .unwrap_or(DEFAULT_EDDN_RPS),
        rate_per_second: cli.optional_decimal(Flag::Rps)?.unwrap_or(DEFAULT_RPS),
        workers: {
            let declared =
                cli.optional_number(Flag::Concurrency)?.unwrap_or(f64::from(DEFAULT_CONCURRENCY));
            js::js_max(1.0, js::js_min(f64::from(MAX_CONCURRENCY), declared)) as u32
        },
        deadline_seconds: cli
            .optional_decimal(Flag::Deadline)?
            .unwrap_or(DEFAULT_DEADLINE_SECONDS),
        max_requests: cli.optional_decimal(Flag::MaxRequests)?.unwrap_or(DEFAULT_MAX_REQUESTS),
        confirmed: cli.switch_value(Flag::Yes, false)?,
        dry_run: cli.switch_value(Flag::DryRun, false)?,
        verbose: cli.switch_value(Flag::Verbose, false)?,
        quiet: cli.switch_value(Flag::Json, false)?,
        cache_dir: cli.optional_value(Flag::CacheDir, None).map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The disambiguation rule, and why it needs no flag: an Elite system name
    /// is never a bare number.
    #[test]
    fn digits_are_a_market_and_anything_else_is_a_system() {
        assert_eq!(parse_line("4306502403"), Some(Target::Market(4_306_502_403.0)));
        assert_eq!(parse_line("Sol"), Some(Target::System("Sol".to_owned())));
        assert_eq!(
            parse_line("COL 285 SECTOR HB-V C3-0"),
            Some(Target::System("COL 285 SECTOR HB-V C3-0".to_owned()))
        );
        // A name with digits in it is still a name.
        assert_eq!(parse_line("LHS 1939"), Some(Target::System("LHS 1939".to_owned())));
    }

    /// A list is hand-written, so it gets the courtesies of a hand-written
    /// file: blank lines, comments, and stray whitespace.
    #[test]
    fn a_list_tolerates_being_hand_written() {
        let text = "\n\
             # the stale ones\n\
             COL 285 SECTOR HB-V C3-0\n\
             \n\
               COL 285 SECTOR HM-B A43-2  \n\
             4306502403\n";
        assert_eq!(
            parse_list(text),
            vec![
                Target::System("COL 285 SECTOR HB-V C3-0".to_owned()),
                Target::System("COL 285 SECTOR HM-B A43-2".to_owned()),
                Target::Market(4_306_502_403.0),
            ]
        );
    }

    /// Order is kept, because a hand-written list has an order the writer
    /// meant and a run cut short by `--deadline` should have done the top of
    /// it. Duplicates are dropped, because relaying one twice is the thing the
    /// suppression window exists to prevent.
    #[test]
    fn order_is_kept_and_repeats_are_not() {
        assert_eq!(
            parse_list("Sol\nLHS 1939\nSol\n"),
            vec![Target::System("Sol".to_owned()), Target::System("LHS 1939".to_owned())]
        );
    }

    /// `MARKET_ID` in the environment is a default for the ported commands. It
    /// must not make `--from-file` unusable for the very large set of people
    /// who have one set — which is everyone who has ever run `edm market`.
    #[test]
    fn a_market_id_in_the_environment_does_not_contradict_a_file() {
        use crate::cli::{Table, parse_with};
        let argv: Vec<String> =
            ["eddn", "market", "--from-file", "x"].iter().map(|s| (*s).to_owned()).collect();
        let parsed = parse_with(&argv, Table::Extended).expect("parses");
        let env = crate::cli::EnvSnapshot::from_pairs(
            [("MARKET_ID".to_owned(), "4306502403".to_owned())],
        );
        let config = feed_config(&Cli::new(&parsed, &env), Some("Sol\n")).expect("configures");
        assert_eq!(config.targets, vec![Target::System("Sol".to_owned())]);
    }

    /// And with nothing else named, it is still the convenience it is
    /// everywhere else in this program.
    #[test]
    fn a_market_id_in_the_environment_is_still_a_default() {
        use crate::cli::{Table, parse_with};
        let argv: Vec<String> = ["eddn", "market"].iter().map(|s| (*s).to_owned()).collect();
        let parsed = parse_with(&argv, Table::Extended).expect("parses");
        let env = crate::cli::EnvSnapshot::from_pairs(
            [("MARKET_ID".to_owned(), "4306502403".to_owned())],
        );
        let config = feed_config(&Cli::new(&parsed, &env), None).expect("configures");
        assert_eq!(config.targets, vec![Target::Market(4_306_502_403.0)]);
    }

    #[test]
    fn the_kind_is_required_and_named() {
        assert_eq!(feed_of(&["market".to_owned()]), Ok(Feed::Market));
        assert_eq!(
            feed_of(&[]).unwrap_err().message(),
            "eddn needs something to import: eddn market"
        );
        assert_eq!(
            feed_of(&["shipyard".to_owned()]).unwrap_err().message(),
            "eddn cannot import \"shipyard\"; the kinds are: market"
        );
    }
}
