//! `edm route` — sweep a region for live prices, then rank what is in it.
//!
//! The sequencing here is the safeguard, and it is the reason this file reads
//! as a list of steps rather than a pipeline:
//!
//! 1. Resolve the reference and enumerate the region **through Ardent**, which
//!    is free, unmetered and CDN-fronted.
//! 2. Filter to markets a large ship can actually use, before anything is spent.
//! 3. **Print the plan and price it.** Above the ceiling, stop here.
//! 4. Only then poll the Companion API, paced, one request per market.
//!
//! Steps 1–3 cannot send a Frontier request at all, which is what makes
//! `expect-frontier-requests = 0` a provable assertion in the harness rather
//! than an assumption. A run that refuses is a run whose wire log is empty.

use edm_core::ardent::{self, Lookup, ReferenceSystem};
use edm_core::cli::config::{RouteConfig, Shape};
use edm_core::pace::{Bucket, Budget};
use edm_core::render::views::{self, RouteCoverage};
use edm_core::select;
use edm_core::spend::{Counts, SizePrior};

use crate::ardent::ArdentClient;
use crate::cmd::{App, CmdResult};
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs, Timer};
use edm_route::model::{Limits, ShipConfig};
use edm_route::num::{Credits, Tons};
use edm_route::report::RouteKind;
use edm_route::time::TimeModel;
use edm_route::view;

use crate::route::acquire;
use crate::route::ingest;
use crate::route::cache::Cache;
use crate::route::discover::{self, DEFAULT_ANCHOR_BUDGET};
use crate::route::pacer::{Pacer, Pacing};
use crate::route::plan::{self, Survey};

/// Above this many markets, the cache pre-pass says it is happening.
///
/// It reads and JSON-decodes one file per market, ~0.8 ms each, and it sits
/// between the filter and the spend gate where nothing else is printed — five
/// thousand markets is four seconds of silence in a place a reader would take
/// for a stall. Below the threshold it is milliseconds and the line would be
/// noise, which is its own kind of dishonesty: a progress report that fires
/// when there is no progress to report teaches people to ignore it.
const CACHE_NOTE_THRESHOLD: usize = 500;

/// How many Ardent market lists are read at once.
///
/// Ardent is CDN-fronted, unmetered and undocumented as to limits; sixteen
/// concurrent requests were measured returning 200 in 0.6 s total against
/// 330 ms each serially. Sixteen rather than more because this is somebody
/// else's free service and the gain past it is small — the win is going from
/// one to sixteen, not from sixteen to sixty-four.
const ARDENT_CONCURRENCY: usize = 16;

/// How long the optimiser may work in silence before it starts saying so.
///
/// Two seconds. Below it the search is over before a human could read a line,
/// and printing anyway would put three lines of scaffolding under every small
/// run — including the parity harness's, whose output is compared byte for
/// byte. Above it the run is one a user is waiting on.
const SOLVE_QUIET_MS: f64 = 2_000.0;

/// The floor on the gap between two search progress lines.
///
/// The graph build reports every few thousand supply rows, which at five
/// thousand markets is tens of times a second. A terminal is not a log.
const SOLVE_LINE_MS: f64 = 500.0;

/// Run the command.
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence, and the order is the safeguard: everything free \
              and shown before anything is spent. Splitting it hides that."
)]
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs, T: Timer>(
    app: &App<'_, H, C, E, F>,
    config: &RouteConfig,
    timer: &T,
) -> CmdResult {
    let out = app.out;
    // Everything else this run writes goes to stderr from here on \[C28\].
    if config.json {
        out.stdout_is_a_document();
    }
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);

    // Nothing below this point may run on a name that was never resolved: an
    // enumeration centred on the wrong system is a complete, confident answer
    // about the wrong region.
    // Before anything at all — not merely before anything is *sent*. A radius
    // past the ceiling is a fact about the argv and cannot become acceptable
    // once the region is known, so enumerating first spends minutes of Ardent
    // queries to reach a conclusion that was available immediately.
    if let Some(refusal) = plan::preflight(config) {
        plan::refuse(out, config, &refusal);
        return Ok(());
    }

    let note = |text: String| {
        if !config.quiet {
            out.line(&text);
        }
    };
    note(format!("resolving \"{}\" through Ardent...", config.reference));
    let centre = resolve(&ardent, &config.reference).await?;

    let budget = if config.ardent_queries == 0 { DEFAULT_ANCHOR_BUDGET } else { config.ardent_queries };
    note(format!(
        "enumerating systems within {} Ly of {}...",
        edm_core::js::js_number(config.radius_ly),
        centre.name
    ));
    let anchor_report = (!config.quiet).then_some(move |p: &discover::AnchorProgress<'_>| {
        out.progress(&format!(
            "  anchor {}/{}: {} systems known, complete to {} Ly ({})",
            edm_core::js::format_integer(f64::from(p.anchor)),
            edm_core::js::format_integer(f64::from(p.budget)),
            edm_core::js::format_integer(p.systems_known as f64),
            edm_core::js::js_number(edm_core::js::js_round(p.complete_to_ly)),
            p.system,
        ));
    });
    // Built here rather than beside the price lookup, because the atlas shares
    // its root and the enumeration below is the first thing that wants it.
    let cache = cache_for(app, config);
    // The galaxy's shape, cached locally: systems do not move, and station
    // lists change on the order of days. Before this, a repeat run over the
    // same region re-made every /nearby and /markets request from scratch —
    // hundreds of round trips at a wide radius, all of them for answers that
    // had not changed.
    let atlas = crate::route::atlas::Atlas::new(cache.root(), config.cache, config.refresh);
    let now_ms = app.ports.clock.now_ms();
    let enumeration = discover::enumerate(
        &ardent,
        &atlas,
        &app.ports.fs,
        now_ms,
        &centre,
        config.radius_ly,
        budget,
        anchor_report.as_ref().map(|f| f as discover::AnchorReport<'_>),
    )
    .await
    .map_err(|error| format!("enumerating systems around {}: {error}", centre.name))?;

    // One free `/markets` per system, then the filter. Both happen before the
    // plan is priced, so the plan's market count is measured rather than
    // extrapolated — `--fast-estimate` is the flag that trades this away.
    note(format!(
        "{} systems; reading their market lists ({} at a time)...",
        edm_core::js::format_integer(enumeration.systems.len() as f64),
        edm_core::js::format_integer(ARDENT_CONCURRENCY as f64),
    ));
    // A counter rather than a line per system: eight thousand lines is not a
    // progress report, it is the output. One line rewritten in place is, and
    // `Out::progress` already clamps to the terminal width \[R33\].
    let gather_report = (!config.quiet).then_some(
        move |done: usize, total: usize, found: usize| {
            if done.is_multiple_of(64) || done == total {
                out.progress(&format!(
                    "  {} / {} systems read, {} stations found",
                    edm_core::js::format_integer(done as f64),
                    edm_core::js::format_integer(total as f64),
                    edm_core::js::format_integer(found as f64),
                ));
            }
        },
    );
    let (stations, systems_with_markets) = if config.fast_estimate {
        (Vec::new(), enumeration.systems.len())
    } else {
        gather(
            &ardent,
            &atlas,
            &app.ports.fs,
            now_ms,
            &enumeration,
            gather_report.as_ref().map(|f| f as &dyn Fn(_, _, _)),
        )
        .await
    };

    let selection = select::select(stations, config, &centre.coordinates);

    // Before the gate, not after it: the cache decides how many requests the
    // sweep will actually send, and a plan that priced twenty-two and then
    // sent none is a plan nobody can check. A few file reads.
    let cache = cache_for(app, config);
    if config.cache && selection.keep.len() >= CACHE_NOTE_THRESHOLD {
        note(format!(
            "checking the cache for {} markets...",
            edm_core::js::format_integer(selection.keep.len() as f64)
        ));
    }
    let prepared = acquire::prepare(
        &cache,
        &app.ports.fs,
        &selection.keep,
        app.ports.clock.now_ms(),
    );
    // Zero unless `--verify-systems`. Ardent's market ids are usable directly —
    // step 0 established that the Companion API answers for a market the
    // commander is not docked at — and a starsystem payload is ~500 KB against
    // a market's ~20 KB, so reading one per system to rediscover ids we already
    // have would be twenty-five times the transfer for the same prices.
    let systems_to_read = if !config.verify_systems {
        0
    } else if config.fast_estimate {
        systems_with_markets
    } else {
        systems_holding(&selection)
    };

    let survey = Survey {
        complete_to_ly: enumeration.complete_to_ly,
        ardent_requests: enumeration.ardent_requests,
        counts: Counts {
            systems: enumeration.systems.len(),
            systems_to_read,
            stations_known: selection.considered,
            markets_to_poll: selection.keep.len(),
            cached_fresh: prepared.hits.fresh,
        },
        exclusions: selection.exclusions.clone(),
    };

    let decision = plan::gate(out, config, &survey, SizePrior::default());
    if !decision.proceeds() {
        return Ok(());
    }

    // Everything above this line is free. Everything below it costs a request.
    // Validated once, here, rather than per request: a malformed `--nonce`
    // must fail before a single market is polled, not on the hundredth.
    let stamp_overrides = app.stamp_overrides()?;
    // `--language` reaches the wire unvalidated, so a non-ASCII value changes
    // the envelope's byte length \[R65\]. Read once, before the sweep, so that
    // is a single decision rather than one per system.
    let query = edm_core::cli::config::starsystem_query(
        &app.cli,
        edm_core::cli::config::CachedTimestamp::SweepZero,
    )
    .map_err(|error| error.message().to_owned())?;
    let started_ms = app.ports.clock.now_ms();
    // `EDM_JITTER` pins the backoff fraction so a retry scenario's attempt
    // count is reproducible \[C29\]. Unset, this is the real entropy.
    let entropy = crate::ports::PinnedJitter {
        inner: &app.ports.entropy,
        unit: app.overrides.jitter.unwrap_or(f64::NAN),
    };
    let pacer = Pacer::new(pacing(config), &app.ports.clock, timer, &entropy);
    // Built before `Cx` so the closures can borrow what they name. The report
    // needs the station list to say which system a market is in, because a
    // market id is not something anyone recognises.
    let total = prepared.cached.len() + prepared.to_poll.len();
    let stations = &selection.keep;
    let report = |job: &crate::route::pool::Job,
                  outcome: &crate::route::pool::Outcome,
                  attempts: u32,
                  completed: usize| {
        let system = stations
            .iter()
            .find(|s| matches!(job, crate::route::pool::Job::Market { market_id, .. } if *market_id == s.market_id))
            .map_or("", |s| s.system_name.as_str());
        out.line(&views::sweep_line(&views::SweepLine {
            completed,
            total,
            station: job.label(),
            system,
            status: outcome.status,
            tradable: outcome.tradable,
            // Attempt zero is the cache pass: it made no request, so it has no
            // status to print and must not claim one.
            from_cache: attempts == 0,
            attempts,
        }));
    };
    let trace = |event: &views::PaceEvent<'_>| out.line(&views::pace_line(event));

    // `--eddn` relays each market as it is polled, never from the cache: a
    // cached listing was read earlier, and republishing it would stamp that old
    // reading with the current time.
    let eddn_options = config
        .eddn
        .then(|| edm_core::cli::config::eddn_config(&app.cli, &app.session.credentials))
        .transpose()
        .map_err(|error| error.message().to_owned())?;
    let relayed_log = crate::route::relay::Relayed::new(cache.root(), config.eddn_max_age_minutes);
    let eddn = eddn_options.as_ref().map(|options| acquire::Eddn {
        options,
        url: &app.overrides.eddn_url,
        relayed: &relayed_log,
        stations: &selection.keep,
    });
    let relay_tally = std::cell::RefCell::new(crate::route::relay::Tally::default());

    let sweep_cx = acquire::Cx {
        http: app.http,
        clock: &app.ports.clock,
        // The pinned wrapper, which delegates `nonce_bytes` untouched — so the
        // nonces are still the real thing and only the jitter is fixed.
        entropy: &entropy,
        fs: &app.ports.fs,
        out,
        origin: &app.overrides.origin,
        credentials: &app.credentials,
        headers: &app.headers,
        method_override: app.session.method_override.as_deref(),
        nonce_override: stamp_overrides.nonce,
        frontier_time_override: stamp_overrides.frontier_time,
        request_time_override: stamp_overrides.request_time,
        cache: &cache,
        relayed: &relay_tally,
        eddn: eddn.as_ref(),
        workers: config.workers as usize,
        quiet: config.json,
        verify_systems: config.verify_systems,
        language: &query.language,
        report: (!config.quiet).then_some(&report as crate::route::pool::Report<'_>),
        trace: (config.verbose && !config.quiet).then_some(&trace as crate::route::pool::Trace<'_>),
        total,
    };
    // Only the systems that still hold a candidate market are worth an
    // authoritative read; the rest were emptied by the filter and a 500 KB
    // payload would confirm nothing.
    let systems: Vec<(String, f64)> = holding_systems(&enumeration, &selection);
    let acquired = acquire::sweep(&sweep_cx, &pacer, prepared, &systems).await;

    let coverage = coverage_of(&Measured {
        survey: &survey,
        selection: &selection,
        acquired: &acquired,
        enumeration: &enumeration,
        spent: pacer.spent(),
        eddn: config.eddn,
        priced: ingest::priced(&acquired.listings),
        breaker_tripped: pacer.tripped().is_some(),
        elapsed_seconds: (app.ports.clock.now_ms() - started_ms) / 1000.0,
    });
    // Under `--json` the coverage block is inside the document instead; on
    // stderr it would be the same information twice.
    if !config.json {
        out.aside(&views::route_coverage(&coverage));
    }

    // Read before `rank` consumes the listings; the ranking cannot change it.
    let unreached =
        !acquired.unreached.is_empty() || acquired.tally.markets_out_of_time > 0;

    // The optimiser has no clock and no output. Both are lent to it here, and
    // `edm_route::watch` explains why they cannot be anywhere else.
    //
    // `--deadline` is the *run's* budget, and the search is part of the run:
    // "how long the whole sweep may take" already reads as a limit on the
    // thing the user started, and the phase that turned out to be the long one
    // is the search. A second wall-clock flag would let two limits contradict
    // each other and would leave the default answer to "how long may the
    // search take" at "forever" — which is what it was. Whatever the sweep
    // left is what the optimiser gets; when that is nothing, the search hands
    // back the best route it holds and claims nothing about it.
    let deadline_ms = started_ms + config.deadline_seconds * 1000.0;
    let clock = &app.ports.clock;
    let search_expired = || clock.now_ms() >= deadline_ms;
    // Progress is throttled here rather than in the pure crate, because only
    // this side owns a clock — and the rule needs one. A line is worth showing
    // once the search has been silent long enough for the silence to be the
    // problem, and a radius-10 sweep solves in under a millisecond and would
    // otherwise gain three lines of noise. Measured 2026-08-06: a radius-100
    // sweep spends 127 s in the graph build and up to minutes in a single
    // Dinkelbach round, so this threshold never delays a report anyone is
    // waiting for.
    let solving_since = std::cell::Cell::new(f64::NAN);
    let last_line_ms = std::cell::Cell::new(f64::NEG_INFINITY);
    // Whether a build counter has been shown, which is the only condition under
    // which finishing one is worth a line.
    let build_shown = std::cell::Cell::new(false);
    let report_progress = |event: edm_route::watch::Event| {
        let now = clock.now_ms();
        if solving_since.get().is_nan() {
            solving_since.set(now);
        }
        // Never throttled: `Abandoned` withdraws a claim, and a withdrawn claim
        // nobody saw is worse than no progress line at all.
        //
        // A *completed* build is urgent for the opposite reason — it is the
        // line that says a counter stopped because the phase ended rather than
        // because it hung — but only when a counter was shown. The closing
        // report arrives microseconds after the previous one, so the throttle
        // suppressed it every time and a real radius-100 run watched the count
        // stop at 119/154; making it unconditional instead gave a two-market
        // sweep a build line it had no reason to print.
        let urgent = match event {
            edm_route::watch::Event::Abandoned => true,
            edm_route::watch::Event::Building { done, total, .. } => {
                done == total && build_shown.get()
            }
            _ => false,
        };
        if !urgent
            && (now - solving_since.get() < SOLVE_QUIET_MS
                || now - last_line_ms.get() < SOLVE_LINE_MS)
        {
            return;
        }
        last_line_ms.set(now);
        if matches!(event, edm_route::watch::Event::Building { .. }) {
            build_shown.set(true);
        }
        out.progress(&view::progress(event));
    };
    let watch = edm_route::watch::Watch::unlimited().until(&search_expired);
    // Under `--json` stdout is one document \[C28\], so there is nowhere for a
    // progress line to go.
    let watch = if config.quiet { watch } else { watch.reporting(&report_progress) };

    rank(out, config, acquired, &selection.keep, &coverage, watch);

    // Set, not merely raised. `exchange::send` assigns exit 1 for every non-2xx
    // it sees, which is R75 and is exactly right for the ported commands — but
    // a route sweep *expects* some non-2xx: HTTP 410 means a station has no
    // commodity market, which is an answer, not a failure. Route decides its
    // own exit code from what it actually reached, and it is the last word.
    //
    // A market in radius that was never read is not a market that ranked badly,
    // and that is the one thing this code reports.
    out.set_exit(if unreached || coverage.breaker_tripped {
        crate::out::EXIT_FAILURE
    } else {
        0
    });
    Ok(())
}

/// Everything the coverage block is assembled from.
struct Measured<'a> {
    survey: &'a Survey,
    selection: &'a select::Selection,
    acquired: &'a acquire::Acquired,
    enumeration: &'a discover::Enumeration,
    spent: crate::route::pacer::Spent,
    /// Whether this run was relaying at all, which is what decides between "0
    /// relayed" and no row.
    eddn: bool,
    /// Counted before ingest consumes the listings.
    priced: usize,
    breaker_tripped: bool,
    elapsed_seconds: f64,
}

/// What the run reached, and what it did not.
fn coverage_of(m: &Measured<'_>) -> RouteCoverage {
    RouteCoverage {
        systems_total: m.survey.counts.systems_to_read,
        systems_read: m.survey.counts.systems_to_read,
        systems_failed: 0,
        markets_found: m.selection.keep.len(),
        markets_polled: m.acquired.listings.len(),
        markets_priced: m.priced,
        // The unreached, plus any the run's `--deadline` cut off before they
        // were attempted — which is what they are.
        markets_failed: m.acquired.unreached.len() + m.acquired.tally.markets_out_of_time,
        markets_absent: m.acquired.tally.markets_absent,
        eddn: m.eddn.then_some(edm_core::render::views::EddnCoverage {
            sent: m.acquired.relayed.sent,
            failed: m.acquired.relayed.failed,
            recent: m.acquired.relayed.recent,
            cached: m.acquired.relayed.cached,
            unnamed: m.acquired.relayed.unnamed,
        }),
        cache_hits: m.acquired.cache.fresh,
        requests_sent: m.spent.requests,
        throttled: m.spent.throttled,
        elapsed_seconds: m.elapsed_seconds,
        truncated_to_ly: m.enumeration.truncated.then_some(m.enumeration.complete_to_ly),
        breaker_tripped: m.breaker_tripped,
        ranked: true,
    }
}

/// Solve, and print what the search will actually claim.
fn rank(
    out: &crate::out::Out,
    config: &RouteConfig,
    acquired: acquire::Acquired,
    stations: &[ardent::ArdentStation],
    coverage: &RouteCoverage,
    watch: edm_route::watch::Watch<'_>,
) {
    // By value: `ingest::markets` drops each payload as it builds its `Market`,
    // and at five thousand markets those payloads are ~2.3 GiB that would
    // otherwise stay resident through the graph build.
    let (markets, commodities, crossing) =
        ingest::markets(acquired.listings, stations, &ingest::floors(config));
    // Not under `--json`: a diagnostic in the middle of the stream is exactly
    // what R76 does to the ported commands, and C28 says route's document is
    // one well-formed document or nothing.
    if crossing.non_integral > 0 && !config.json {
        out.line(&format!(
            "{} commodity rows carried a non-integral price or quantity and were skipped",
            edm_core::js::format_integer(f64::from(crossing.non_integral))
        ));
    }

    // Only the shape that was asked for — and it is *searched* for, not merely
    // printed. `solve` used to run all three every time, so a plain
    // `edm route Sol --radius 100`, whose default shape is a round trip, spent
    // tens of minutes on a loop search whose result was then discarded
    // unprinted. Reported from a live run, 2026-08-05.
    let kind = match config.shape {
        Shape::OneWay => RouteKind::SingleHop,
        Shape::RoundTrip => RouteKind::RoundTrip,
        Shape::Loop | Shape::BoundedLoop(_) => RouteKind::Loop { stops: 0 },
    };
    let solution = edm_route::solve(
        &markets,
        time_model(config),
        &ship(config),
        &limits(config),
        edm_route::Wanted::only(kind),
        watch,
    );
    let routes = match kind {
        RouteKind::SingleHop => &solution.single,
        RouteKind::RoundTrip => &solution.round_trip,
        RouteKind::Loop { .. } => &solution.loops,
    };

    if config.json {
        // All three keys are always present, but only the requested one is
        // populated — a key that appears and disappears with a flag is the
        // harder thing to consume, and searching the other two would cost
        // minutes for output nobody asked for.
        let document = edm_route::json::document(
            &solution,
            &markets,
            &commodities,
            coverage_json(coverage, &crossing),
        );
        out.document(&document.stringify(2));
        return;
    }

    out.emit(&view::ranking(kind, routes, &markets, &commodities));
    // The ranking names stations; `edm trade` wants a market id. Without this
    // the answer stops one step short of being usable.
    out.emit(&view::trade_commands(
        routes,
        &markets,
        &commodities,
        config.cargo.map(|tons| tons as i64),
    ));

    if config.detail {
        for route in routes {
            out.emit(&view::legs(route, &markets, &commodities));
        }
    }
}

/// The coverage block, as JSON.
///
/// Carried inside the document rather than printed beside it, because what a
/// sweep failed to reach is part of the answer: a consumer that reads `loops`
/// without reading `coverage.marketsFailed` has drawn a conclusion from a
/// region it did not see all of.
fn coverage_json(
    coverage: &RouteCoverage,
    crossing: &ingest::Crossing,
) -> edm_core::js::json::JsValue {
    use edm_core::js::json::{JsObject, JsValue};
    let n = |value: usize| JsValue::Num(value as f64);
    JsValue::Obj(JsObject::from_document_order(vec![
        ("systemsRead".into(), n(coverage.systems_read)),
        ("systemsTotal".into(), n(coverage.systems_total)),
        ("systemsFailed".into(), n(coverage.systems_failed)),
        ("marketsFound".into(), n(coverage.markets_found)),
        ("marketsPolled".into(), n(coverage.markets_polled)),
        ("marketsPriced".into(), n(coverage.markets_priced)),
        ("marketsFailed".into(), n(coverage.markets_failed)),
        ("marketsAbsent".into(), n(coverage.markets_absent)),
        ("cacheHits".into(), n(coverage.cache_hits)),
        ("requestsSent".into(), n(coverage.requests_sent)),
        ("throttled".into(), n(coverage.throttled)),
        ("elapsedSeconds".into(), JsValue::Num(coverage.elapsed_seconds)),
        (
            "completeToLy".into(),
            coverage.truncated_to_ly.map_or(JsValue::Null, JsValue::Num),
        ),
        ("breakerTripped".into(), JsValue::Bool(coverage.breaker_tripped)),
        ("rowsSkippedNonIntegral".into(), JsValue::Num(f64::from(crossing.non_integral))),
        ("notes".into(), JsValue::Arr(coverage.notes().into_iter().map(|note| JsValue::Str(note.into())).collect())),
    ]))
}

/// The travel model, with every constant a flag.
fn time_model(config: &RouteConfig) -> TimeModel {
    TimeModel { jump_range_ly: config.jump_range_ly, ..TimeModel::default() }
}

/// The ship, or the absence of one.
///
/// An omitted `--cargo` or `--credits` means *unbounded*, not zero: the answer
/// is then the best route the data admits for a ship that can carry it, which
/// is the right default for "what is out there".
fn ship(config: &RouteConfig) -> ShipConfig {
    ShipConfig {
        cargo: config.cargo.map_or(ShipConfig::default().cargo, |tons| Tons(tons as i64)),
        credits: config
            .credits
            .map_or(ShipConfig::default().credits, |credits| Credits(credits as i64)),
    }
}

fn limits(config: &RouteConfig) -> Limits {
    Limits {
        top_n: config.top,
        min_profit: Credits(config.min_profit as i64),
        max_stops: match config.shape {
            Shape::BoundedLoop(k) => Some(k),
            _ => None,
        },
        ..Limits::default()
    }
}

/// The pacing a run is built with.
fn pacing(config: &RouteConfig) -> Pacing {
    Pacing {
        bucket: Bucket {
            rate: config.rate_per_second,
            // One burst token, not a bucketful: a wide sweep that opened with
            // sixteen simultaneous requests would look exactly like the abuse
            // the rate limit exists to stop, and the burst buys nothing when
            // there are a thousand markets to get through.
            burst: 1.0,
            min_rate: edm_core::js::js_min(config.rate_per_second, 0.5),
        },
        budget: Budget {
            // A job may not keep being retried for longer than the run it
            // belongs to has in total. Without this, `--deadline 5` would
            // still let one market burn two minutes of retries.
            per_job_ms: edm_core::js::js_min(
                Budget::default().per_job_ms,
                config.deadline_seconds * 1000.0,
            ),
            run_deadline_ms: config.deadline_seconds * 1000.0,
            ..Budget::default()
        },
        ..Pacing::default()
    }
}

/// Where this run's cache lives, and how it is used.
///
/// The two environment variables come from the run's own [`EnvSnapshot`], not
/// from `std::env` — first-wins, lossily decoded, and *scrubbed by the parity
/// harness* \[R55\]. Reading the process environment directly here would give
/// the cache a home the rest of the program cannot see.
fn cache_for<H, C, E, F>(app: &App<'_, H, C, E, F>, config: &RouteConfig) -> Cache {
    let root = Cache::locate(
        app.cli.env("XDG_CACHE_HOME"),
        app.cli.env("HOME"),
        config.cache_dir.as_deref(),
    );
    Cache::new(root, config.max_age_minutes, config.cache, config.refresh)
}

/// The reference system, as a point to enumerate around.
///
/// `Lookup::Auto` so `edm route "Jaques Station"` works — but the radius is
/// measured from the *system*, which is the only thing a light year is a
/// distance between, and the station name only ever selects it.
async fn resolve<H: HttpTransport>(
    ardent: &ArdentClient<'_, H>,
    reference: &str,
) -> Result<ReferenceSystem, String> {
    Ok(ardent.resolve_location(reference, Lookup::Auto).await?.system)
}

/// One `/markets` per enumerated system, concurrently and out loud.
///
/// **This is the phase that dominates a wide sweep.** Ardent answers in about
/// 330 ms, so at radius 100 around Sol — 8,156 systems — a serial loop is
/// forty-five minutes, and the first version of this function was a serial loop
/// that printed nothing at all. Sixteen at a time is 0.6 s for sixteen,
/// measured, which turns the same work into roughly three minutes.
///
/// Free and unmetered, so this is not paced and failures are not retried: a
/// system whose market list does not answer contributes nothing, and the plan
/// reports a smaller region rather than a wrong one. The Companion API is what
/// the pacer and the spend gate exist for.
///
/// Returns the stations and how many systems answered at all.
async fn gather<H: HttpTransport, F: Fs>(
    ardent: &ArdentClient<'_, H>,
    atlas: &crate::route::atlas::Atlas,
    fs: &F,
    now_ms: f64,
    enumeration: &discover::Enumeration,
    report: Option<&dyn Fn(usize, usize, usize)>,
) -> (Vec<ardent::ArdentStation>, usize) {
    use futures_util::StreamExt as _;

    let total = enumeration.systems.len();
    let done = std::cell::Cell::new(0usize);
    let answered = std::cell::Cell::new(0usize);
    let found = std::cell::Cell::new(0usize);

    let results = futures_util::stream::iter(enumeration.systems.iter().map(|system| {
        // `system_markets` places the rows at these coordinates itself, which
        // is the only reason a `/markets` row has any position at all.
        let reference = ReferenceSystem {
            name: system.name.clone(),
            address: system.address,
            coordinates: system.coordinates,
        };
        let (done, answered, found) = (&done, &answered, &found);
        async move {
            let stations =
                ardent.system_markets_cached(atlas, fs, now_ms, &reference).await.ok();
            done.set(done.get() + 1);
            if let Some(stations) = &stations {
                answered.set(answered.get() + 1);
                found.set(found.get() + stations.len());
            }
            // Reported from inside the task, in completion order, so a long
            // gather visibly moves rather than sitting silent for minutes.
            if let Some(report) = report {
                report(done.get(), total, found.get());
            }
            stations
        }
    }))
    // `buffered`, not `buffer_unordered`: both run sixteen at once, but this
    // one yields in *input* order. The station list order reaches the poll
    // order, the progress lines and the harness's byte diff, and a sweep whose
    // output depended on which of sixteen requests came back first would not be
    // reproducible. Ordering the results costs nothing here — every request is
    // already in flight.
    .buffered(ARDENT_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut stations = Vec::new();
    for mut batch in results.into_iter().flatten() {
        stations.append(&mut batch);
    }
    (stations, answered.get())
}

/// The systems `--verify-systems` reads, with the addresses those reads need.
fn holding_systems(
    enumeration: &discover::Enumeration,
    selection: &select::Selection,
) -> Vec<(String, f64)> {
    let mut wanted: Vec<&str> = selection.keep.iter().map(|s| s.system_name.as_str()).collect();
    wanted.sort_unstable();
    wanted.dedup();
    enumeration
        .systems
        .iter()
        .filter(|system| wanted.binary_search(&system.name.as_str()).is_ok())
        .map(|system| (system.name.clone(), system.address))
        .collect()
}

/// How many systems still hold a market worth reading.
///
/// This is the number the plan prices, not the number of systems in radius:
/// near Sol the filter empties most of them, and a starsystem read is the
/// larger of the two request kinds by a factor of twenty-five.
fn systems_holding(selection: &select::Selection) -> usize {
    let mut names: Vec<&str> = selection.keep.iter().map(|s| s.system_name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use edm_core::ardent::ArdentStation;
    use edm_core::domain::id64::Coordinates;

    fn station(system: &str) -> ArdentStation {
        ArdentStation {
            market_id: 1.0,
            station_name: "S".to_owned(),
            system_name: system.to_owned(),
            station_type: Some("Coriolis".to_owned()),
            max_landing_pad_size: None,
            distance_to_arrival: None,
            coordinates: Coordinates { x: 0.0, y: 0.0, z: 0.0 },
        }
    }

    /// A starsystem read is ~500 KB against a market's ~20 KB, so pricing one
    /// per system in radius rather than per system that still holds a market
    /// would overstate the transfer by more than the market reads cost.
    #[test]
    fn only_systems_that_still_hold_a_market_are_priced() {
        let selection = select::Selection {
            keep: vec![station("Sol"), station("Sol"), station("Alpha Centauri")],
            exclusions: Vec::new(),
            considered: 40,
        };
        assert_eq!(systems_holding(&selection), 2);
    }

    #[test]
    fn an_empty_region_prices_no_system_reads() {
        assert_eq!(systems_holding(&select::Selection::default()), 0);
    }
}
