//! `edm route --help`.
//!
//! Kept apart from [`crate::cli::usage`] because that text is pinned byte for
//! byte against captured Bun output, and a new command must not add a character
//! to it \[C25\]. The same interpolation discipline applies here for the same
//! reason: a default that drifts away from its own documentation is worse than
//! an undocumented default.

use crate::cli::config;
use crate::js;
use crate::spend;

/// The route command's help text.
#[must_use]
#[expect(clippy::too_many_lines, reason = "it is one string; splitting it hides the layout")]
pub fn route_usage() -> String {
    let n = |value: f64| js::format_integer(value);
    format!(
        "edm route — provably optimal trade routes from live market data

Usage
  edm route <system|station> [options]

Sweeps every market within a radius for live Companion API prices, then finds
the most profitable routes in that data. The loop search is exact: it returns
the best repeatable route there is, not the best one it happened to find.

Search
  --radius <ly>            default {radius}, ceiling {max_radius} (Ardent's own clamp).
                           It bounds how far each *market* is from the
                           reference, not how long a leg may be: two markets
                           each within 40 Ly can be 80 Ly apart. A wide radius
                           buys more markets, and --max-requests is what refuses
                           when that is too many
  --shape <s>              one-way | round-trip | loop | loop:N   default round-trip
                           `loop` is the best repeatable cycle of any length, and
                           is solved exactly; `loop:N` bounds it to N stops
  --top <n>                default {top}
  --min-profit <cr>        ignore legs below this, default {min_profit}

Ship
  --cargo <t>              hold capacity; unbounded when omitted
  --credits <n>            starting balance; unbounded when omitted
  --jump <ly>              laden jump range, default {jump}

Which markets (all of these prune before anything is sent)
  --pad <S|M|L>            default L
  --max-star-distance <ls> default {star_distance}
  --station-types <a,b,c>  override the default starport set
  --include-carriers       fleet carriers are excluded by default: they jump
                           without warning, so a route through one can evaporate
                           between planning and flying
  --settlements            Odyssey settlements are excluded by default; they
                           cannot berth a large ship at all
  --min-supply <n>         default 1        --min-demand <n>   default 1
  --category <a,b,c>       only these commodity categories. Metals, Minerals,
                           Foods, Chemicals, Machinery, Medicines, Technology,
                           Textiles, Consumer Items, Industrial Materials,
                           Salvage, Weapons, Waste, Narcotics, Slaves
  --include-illegal        rank commodities a market marks illegal *there*.
                           Off by default: such a trade is refused at the
                           counter (HTTP 401) and the black-market path needs a
                           station service the market payload does not report,
                           which is how a route once ended at a station
                           publishing 68,433 tons of demand it could not accept
  --verify-systems         read each system's own market list from the Companion
                           API instead of trusting Ardent's. Authoritative, and
                           about 25x the transfer: a starsystem payload is
                           ~500 KB against a market's ~20 KB, and it exists to
                           find a market Ardent has never seen

Spending
  Every market in range costs one authenticated request. The plan is printed
  and priced before anything is sent.
  --max-requests <n>       ceiling, default {max_requests}; nothing is sent above it
  --yes                    required above {confirm} requests
  --rps <n>                requests per second, default {rps}
  --deadline <s>           how long the whole run may take, default {deadline}.
                           It bounds one market's retries — a market may not be
                           retried for longer than the run has left — and the
                           loop search, which gets whatever the sweep did not
                           spend. A search stopped by it reports the best route
                           it found and says it did not prove it
  --max-age <minutes>      reuse cached prices younger than this, default {max_age}
  --no-cache               ignore the cache entirely   --refresh   re-poll everything
  --cache-dir <path>       default $XDG_CACHE_HOME/edm/route
  --ardent-queries <n>     enumeration budget, default {ardent}
  --fast-estimate          skip the free pre-count and extrapolate instead
  --dry-run                print the plan and stop

Sharing what it reads
  --eddn                   relay every market this run polls to EDDN, as it is
                           polled. --eddn-test uses the gateway's test schema
  --eddn-max-age <m>       suppress a repeat relay of the same market for this
                           long, default {eddn_age}. A listing served from the
                           local price cache is never relayed at any age: it was
                           read earlier, and republishing it would stamp that old
                           reading with the current time

Output
  One line per market as it lands, unless --json. The sweep is minutes long on a
  wide radius, and silence is indistinguishable from a stall. The search after
  it says where it has got to and what the best rate so far is, once it has been
  working for more than a couple of seconds.
  --json                   one document, for piping; silences the progress lines
  --detail                 expand every leg of every route
  --verbose, -v            say what the pacer is doing too: throttles, retries
                           and the delay each one waited, rate changes, and the
                           reason a run stopped early. A sweep that is merely
                           slow and one that is being rate limited look the same
                           from outside; this is what tells them apart

Examples
  edm route Sol
  edm route \"Shinrarta Dezhra\" --radius 50 --cargo 784 --shape loop
  edm route Colonia --cargo 1232 --credits 500000000 --shape loop:4 --yes
  edm route Sol --radius 15 --dry-run
",
        radius = n(config::DEFAULT_RADIUS_LY),
        max_radius = n(spend::MAX_RADIUS_LY),
        top = n(config::DEFAULT_TOP),
        min_profit = n(config::DEFAULT_MIN_PROFIT),
        jump = n(config::DEFAULT_JUMP_RANGE_LY),
        star_distance = n(config::DEFAULT_MAX_STAR_DISTANCE_LS),
        max_requests = n(spend::DEFAULT_MAX_REQUESTS),
        confirm = n(spend::CONFIRM_THRESHOLD),
        rps = n(config::DEFAULT_RPS),
        deadline = n(config::DEFAULT_DEADLINE_SECONDS),
        max_age = n(config::DEFAULT_MAX_AGE_MINUTES),
        eddn_age = n(config::DEFAULT_EDDN_MAX_AGE_MINUTES),
        ardent = n(config::DEFAULT_ARDENT_QUERIES),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The base help text is pinned against captured Bun output, so any
    /// accidental sharing between the two would show up as a parity failure —
    /// but only if someone happened to run it. Assert the separation directly.
    #[test]
    fn route_help_is_separate_from_the_pinned_usage() {
        let route = route_usage();
        let base = crate::cli::usage();
        assert!(route.starts_with("edm route"));
        assert!(!base.contains("edm route"), "the pinned text must not mention route");
        assert!(!route.contains("Frontier market API client"), "and not the reverse");
    }

    /// Every advertised default is the one the program actually uses. A number
    /// typed into prose is a number that drifts.
    #[test]
    fn the_help_advertises_the_real_defaults() {
        let text = route_usage();
        for expected in [
            "--radius <ly>            default 30, ceiling 500",
            "default round-trip",
            "--max-requests <n>       ceiling, default 2,000",
            "required above 250 requests",
            "--rps <n>                requests per second, default 4",
        ] {
            assert!(text.contains(expected), "missing: {expected}\n\n{text}");
        }
    }

    /// The two defaults that carry the most weight say *why* in the help
    /// itself, because a user who does not know will assume the tool simply
    /// missed those markets.
    #[test]
    fn the_exclusions_explain_themselves() {
        let text = route_usage();
        assert!(text.contains("evaporate"), "carriers");
        assert!(text.contains("cannot berth a large ship"), "settlements");
    }
}
