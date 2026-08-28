//! `edm sell` — planning the disposal of the hold \[C41\].
//!
//! Kept apart from [`crate::cli::usage`] for the reason C25 gives: that text is
//! pinned byte for byte against captured Bun output and a new command must not
//! add a character to it.

use crate::cli::access::{Cli, CliError};
use crate::cli::flag::Flag;
use crate::js;

/// How many markets one disposal may spread across, by default.
///
/// Three covers what actually happens — dump it all in one place, or split when
/// one buyer's demand cannot take the hold — and keeps the enumeration
/// instantaneous. It is also the search bound, which is why it is a flag and
/// not a constant buried in the solver.
pub const DEFAULT_STOPS: f64 = 3.0;

/// The most stops the search will accept.
///
/// Four, because the ordered-path count is a falling factorial: at `--top 20`
/// per commodity, four stops over sixty candidates is already tens of millions
/// of orderings, and the answer at that size is a milk run nobody flies.
pub const MAX_STOPS: f64 = 4.0;

/// Everything `edm sell` needs from the command line.
#[derive(Clone, Debug, PartialEq)]
pub struct SellConfig {
    /// Where the ship is. `None` means "read it from the journal".
    pub origin: Option<String>,
    /// Only plan these commodities, rather than the whole hold.
    pub items: Vec<String>,
    /// What an hour is worth, in credits. `None` derives it from the best
    /// single stop that clears the hold.
    pub worth: Option<f64>,
    pub stops: usize,
    pub radius_ly: f64,
    pub top: usize,
    pub min_demand: f64,
}

/// `--stops`, refused rather than clamped.
///
/// The clamp/refuse choice follows `vendor`'s `--radius`, which returns an error
/// rather than quietly using the ceiling: a bound the user asked for and did not
/// get is a bound they will reason with anyway.
pub fn stops(cli: &Cli<'_>) -> Result<usize, CliError> {
    let asked = cli.optional_number(Flag::Stops)?.unwrap_or(DEFAULT_STOPS);
    if !(1.0..=MAX_STOPS).contains(&asked) {
        return Err(format!(
            "--stops must be between 1 and {}; the search is a falling factorial and past that it is a milk run, not a plan",
            js::js_number(MAX_STOPS),
        )
        .into());
    }
    Ok(asked as usize)
}

/// `--worth`, in credits per hour.
pub fn worth(cli: &Cli<'_>) -> Result<Option<f64>, CliError> {
    let Some(value) = cli.optional_decimal(Flag::Worth)? else {
        return Ok(None);
    };
    if value < 0.0 || !value.is_finite() {
        return Err("--worth must be a non-negative number of credits per hour"
            .to_owned()
            .into());
    }
    Ok(Some(value))
}

/// Everything the command needs, read once.
pub fn sell_config(cli: &Cli<'_>) -> Result<SellConfig, CliError> {
    let radius_ly = cli
        .optional_decimal(Flag::Radius)?
        .unwrap_or(super::config::DEFAULT_RADIUS_LY);
    if radius_ly <= 0.0 || radius_ly > crate::spend::MAX_RADIUS_LY {
        return Err(format!(
            "--radius must be between 0 and {}",
            js::js_number(crate::spend::MAX_RADIUS_LY)
        )
        .into());
    }
    let items: Vec<String> = cli
        .optional_value(Flag::Item, None)
        .map(|raw| {
            raw.split(',')
                .map(|name| crate::ardent::normalise_commodity_name(crate::js::text::js_trim(name)))
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(SellConfig {
        origin: cli.optional_value(Flag::From, None).map(ToOwned::to_owned),
        items,
        worth: worth(cli)?,
        stops: stops(cli)?,
        radius_ly,
        top: cli
            .optional_number(Flag::Top)?
            .unwrap_or(super::config::DEFAULT_TOP) as usize,
        min_demand: cli.optional_decimal(Flag::MinDemand)?.unwrap_or(1.0),
    })
}

/// The help text.
#[must_use]
pub fn sell_usage() -> String {
    let n = |value: f64| js::format_integer(value);
    format!(
        "edm sell — where to sell what you are already carrying

Usage
  edm sell [options]

Reads the hold from your journal, finds who buys it, and plans the disposal:
which markets to visit, in what order, and how much to leave at each.
This command plans only; `edm trade` is what transacts.

The plan is chosen by credits minus time, not by credits per hour. A disposal
is finite, and ranking a finite task by its own rate pays you to stop early:
800 t in 19 minutes beats 1,232 t in 41 on credits per hour, while leaving 432 t
aboard. So a further stop joins the plan exactly when it earns more than your
time is worth, and the alternatives table shows you the ones it refused and by
how much, so you can move the bar rather than argue with the answer.

What you are carrying
  Read from Cargo.json beside the journal, or EDM_JOURNAL_DIR. Stolen tons and
  mission cargo are excluded and named: this program cannot see fence prices, so
  a plan including them would be a plan that gets refused at the counter.
  --item <a,b,...>         only these commodities, rather than the whole hold

The decision
  --worth <cr/h>           what an hour of your time is worth. A stop is taken
                           only if it earns more than this for the time it costs.
                           Defaults to the rate of the best single stop that
                           clears the hold — beat the obvious thing that finishes
                           the job
  --stops <n>              how many markets to spread across, default {stops},
                           maximum {max_stops}. This is the search bound

Which buyers
  --radius <ly>            default {radius}, Ardent's own clamp is {max_radius}
  --top <n>                buyers kept per commodity, default {top}
  --min-demand <t>         ignore buyers publishing less than this, default 1
  --from <system>          where the ship is, if the journal does not say
  --carriers               include fleet carriers; --carrier-access filters them
  --pad, --station-types, --max-star-distance, --include-illegal   as `edm route`

Spending
  Every candidate costs one authenticated request to verify. Nothing in the plan
  is a price this run did not read.
  --max-requests <n>, --yes, --rps, --deadline, --max-age, --no-cache, --refresh

Output
  --detail                 every nominated buyer and why it is not in the plan
  --json                   one document

Examples
  edm sell
  edm sell --worth 50000000 --stops 2
  edm sell --item tritium --radius 60
",
        stops = n(DEFAULT_STOPS),
        max_stops = n(MAX_STOPS),
        radius = n(super::config::DEFAULT_RADIUS_LY),
        max_radius = n(crate::spend::MAX_RADIUS_LY),
        top = n(super::config::DEFAULT_TOP),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_help_says_what_the_command_will_not_do() {
        let text = sell_usage();
        assert!(text.contains("plans only; `edm trade`"), "{text}");

    }

    /// The objective is the part a reader is most likely to assume wrongly, so
    /// the help states it and states why.
    #[test]
    fn the_help_explains_why_it_is_not_credits_per_hour() {
        let text = sell_usage();
        assert!(text.contains("credits minus time"));
        assert!(text.contains("pays you to stop early"));
    }

    #[test]
    fn the_help_says_stolen_cargo_is_excluded() {
        assert!(sell_usage().contains("Stolen tons"));
    }
}
