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
#[expect(
    clippy::too_many_lines,
    reason = "it is one string; splitting it hides the layout"
)]
pub fn route_usage() -> String {
    let n = |value: f64| js::format_integer(value);
    format!(
        "edm route — provably optimal trade routes from live market data

Usage
  edm route <system|station> [options]

Sweeps every market within a radius for live game-internal API prices, then finds
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

Quick commodity lookup
  --quick <n>              for every --item, and every commodity in --category,
                           score every Ardent seller-buyer pair by estimated
                           credits per hour (spread × cargo / travel time),
                           keep the N best hops, verify those markets live
                           with Frontier, then report the best live buy and
                           sell location for each commodity
  --item <a,b,...>         commodity names for --quick; each is checked
                           against Ardent's catalogue and refused if unknown
  --qty <t>                minimum seller stock and published buyer demand for --quick;
                           default ceil(10% of the hold; 1,232 t if unknown)
                           --quick defaults --shape to one-way: it looks up where
                           to buy and sell, not a cycle. A gold hop can outpay a
                           metals round trip. Pass --shape round-trip for a
                           return cargo. --verify-systems is incompatible with
                           --quick.

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
  --carrier-access <p>     any | open | proven   default open, with --carriers.
                           Reads each candidate carrier's docking access live
                           from the game-internal API — one request per carrier,
                           priced and confirmed in a plan of its own before any
                           of them is sent, so a door that will not open never
                           costs a market read. `open` drops the carriers
                           Frontier reports as squadron-, friends- or owner-only,
                           and the ones closed to you specifically because of
                           your notoriety; `proven` also drops any this run
                           could not read; `any` asks nothing and ranks them
                           all. Your own journal overrides all of it: a carrier
                           that answered this ship DockingDenied is dropped
                           however open Frontier calls it, and one you have
                           actually docked at is kept however closed — which is
                           the only way a squadron carrier you are in the
                           squadron of survives `open`. Verdicts are cached for
                           15 minutes
  --settlements            consider Odyssey settlements; requires --pad small
                           because they cannot berth a large ship
  --min-supply <n>         default 1        --min-demand <n>   default 1
  --category <a,b,c>       only these commodity categories. Metals, Minerals,
                           Foods, Chemicals, Machinery, Medicines, Technology,
                           Textiles, Consumer Items, Industrial Materials,
                           Salvage, Weapons, Waste, Narcotics, Slaves.
                           With --quick, this names the commodities to look up
                           rather than requiring --item
  --include-illegal        rank commodities a market marks illegal *there*.
                           Off by default: such a trade is refused at the
                           counter (HTTP 401) and the black-market path needs a
                           station service the market payload does not report,
                           which is how a route once ended at a station
                           publishing 68,433 tons of demand it could not accept
  --verify-systems         enrich/filter known candidates in official batches of 5 and
                           apply empirical quantity-aware prices (heuristic); radius <= 40

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
                           it found and says it did not prove it. Under --follow
                           it bounds one round, not the session
  --follow <s>             with --quick: instead of printing once, re-read the
                           markets behind the ranked routes every <s> seconds
                           and print the ranking again. Minimum {min_follow}s —
                           a round re-reads every market in the ranking. Each
                           round re-evaluates the whole original shortlist, so a
                           route that a dead market removed can come back when
                           it restocks. --max-requests becomes the session
                           ceiling and is enforced live; there is no other bound
                           unless you set one. Not --watch, which is the retry
                           switch on `trade` and `market`
  --follow-rounds <n>      stop after n rounds rather than at the ceiling
  --max-age <minutes>      rank from cached prices younger than this, default
                           {max_age}. It bounds only which markets are
                           *considered*: the markets behind the routes actually
                           printed are re-read live and the ranking redone, so a
                           day-old cache costs recall, never accuracy. Fleet
                           carriers are never served from it at any age — their
                           quantities drain in a way a station's demand does not
  --no-cache               ignore the cache entirely   --refresh   re-poll everything
  --cache-dir <path>       default $XDG_CACHE_HOME/edm/route
  --ardent-queries <n>     enumeration budget, default {ardent}
  --fast-estimate          reserved; currently refused because safe estimation needs market IDs
  --dry-run                print the plan and stop

Sharing what it reads
  --eddn                   relay every market this run polls to EDDN, as it is
                           polled. --eddn-test uses the gateway's test schema
  --eddn-rps <n>           messages per second to EDDN, default {eddn_rps}, paced
                           separately from --rps
  --eddn-max-age <m>       suppress a repeat relay of the same market for this
                           long, default {eddn_age}. A listing served from the
                           local price cache is never relayed at any age: it was
                           read earlier, and republishing it would stamp that old
                           reading with the current time

Local commander state
  Journal/Status/Cargo files are discovered locally (or via EDM_JOURNAL_DIR).
  When flags are absent they supply the reference system, free cargo space,
  credits and jump range. Explicit flags and positionals always win.

Output
  One line per market as it lands, unless --json. The sweep is minutes long on a
  wide radius, and silence is indistinguishable from a stall. The search after
  it says where it has got to and what the best rate so far is, once it has been
  working for more than a couple of seconds.
  --json                   one document, for piping; silences the progress lines
  --from <system>          where the ship is now, for the `To start` column.
                           Defaults to the commander's own position when the
                           journal supplies one. This is not the positional:
                           the positional says where to *search*, and searching
                           a region 300 Ly away does not move the ship. The
                           approach is not part of the lap or the rate — the
                           model starts its clock in the source system
  --from-here              every route must depart from the market you are
                           docked at, taken from the journal; undocked, from any
                           market in your current system (aliases --here,
                           --depart-here). Answers what you can load right now,
                           which the region-wide ranking cannot: it nominates by
                           price, so the berth under your ship is almost never
                           in any commodity's top page. The origin is pinned by
                           removing supply everywhere else, so --top counts
                           routes from here rather than routes it kept
  --by-profit              order by credits per run and ignore travel time
                           entirely (aliases --ignore-distance, --raw-profit).
                           The default ranks by credits per hour, so a rich hop
                           three hundred light years out loses to a thin one
                           next door; this is the flag for when the flying is
                           not the constraint — a hauler already heading that
                           way, or a hold that has to be filled once. Ly and
                           To start are still shown, they just stop deciding
  --per-hour               show the credits-per-hour column. The ranking is
                           ordered by it either way; it is hidden so the table
                           has room for Stock/Demand, and because it is the
                           column that most often means nothing — it annualises
                           a lap whose supply may be a fleet carrier's fixed
                           pot, which does not restock
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
        min_follow = n(config::MIN_FOLLOW_SECONDS),
        max_age = n(config::DEFAULT_MAX_AGE_MINUTES),
        eddn_age = n(config::DEFAULT_EDDN_MAX_AGE_MINUTES),
        eddn_rps = n(config::DEFAULT_EDDN_RPS),
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
        assert!(
            !base.contains("edm route"),
            "the pinned text must not mention route"
        );
        assert!(
            !route.contains("Frontier market API client"),
            "and not the reverse"
        );
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
            "--quick <n>              for every --item, and every commodity in --category,",
            "score every Ardent seller-buyer pair by estimated",
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
