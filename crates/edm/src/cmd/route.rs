//! `edm route` — sweep a region for live prices, then rank what is in it.
//!
//! The sequencing here is the safeguard, and it is the reason this file reads
//! as a list of steps rather than a pipeline:
//!
//! 1. Resolve/enumerate the reference region through free Ardent discovery.
//! 2. Filter its markets and print a spend plan before authenticated work.
//! 3. With `--verify-systems`, separately gate a complete daily-digest crawl,
//!    official five-system candidate batches, and authoritative market reads.
//! 4. Poll exact listings only after the final quantity-read gate.
//!
//! Every cold-cache phase is priced before its first Frontier request. A
//! refused phase therefore sends none of that phase's requests, while the
//! coverage report accounts for discovery already approved in an earlier one.

use std::collections::HashMap;

use edm_core::ardent::{self, Lookup, ReferenceSystem};
use edm_core::cli::config::{Pad, RouteConfig, Shape};
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
use crate::route::cache::Cache;
use crate::route::discover::{self, DEFAULT_ANCHOR_BUDGET};
use crate::route::ingest;
use crate::route::pacer::{Pacer, Pacing};
use crate::route::plan::{self, Survey};

mod quick;

/// Above this many markets, the cache pre-pass says it is happening.
///
/// It reads and JSON-decodes one file per market, ~0.8 ms each, and it sits
/// between the filter and the spend gate where nothing else is printed — five
/// thousand markets is four seconds of silence in a place a reader would take
/// for a stall. Below the threshold it is milliseconds and the line would be
/// noise, which is its own kind of dishonesty: a progress report that fires
/// when there is no progress to report teaches people to ignore it.
const CACHE_NOTE_THRESHOLD: usize = 500;
const DIGEST_SOURCE: &str = "frontier-daily-digest-v1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SpecialOpportunities {
    rescue_systems: usize,
    colonisation_markets: usize,
    stateful_markets: usize,
    commodity_override_markets: usize,
}

/// What bounded the candidate universe before the normal live ranking.
///
/// This is deliberately carried to JSON rather than inferred from counts: a
/// price-index prefix that happened to yield the same number of markets as a
/// small regional survey is still not a complete regional answer.
#[derive(Clone, Debug)]
pub(super) struct QuickProvenance {
    pub commodities: Vec<String>,
    pub markets_per_side: usize,
    pub seller_minimum: f64,
    pub buyer_minimum: f64,
    pub candidate_rows: usize,
    pub market_ids: Vec<f64>,
    pub unpublished_buyer_candidates: usize,
    /// Requested commodities that produced no candidate. A consumer that reads
    /// only the routes cannot otherwise tell a commodity that lost on price
    /// from one that was never in the answer.
    pub commodities_without_candidates: Vec<String>,
    /// The subset of those for which Ardent's index returned no row at all,
    /// which is what a misspelt `--item` looks like.
    pub commodities_absent_from_index: Vec<String>,
    /// Per commodity, the best live seller and buyer among the polled markets.
    /// Empty until the sweep has run, because until then there is no live price
    /// to report.
    pub best_live: Vec<quick::BestLive>,
}

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
    // The old implementation skipped station discovery and then tried to rank
    // an empty market set. That is not an estimate: it is a confident false
    // negative, and with `--verify-systems` it could even claim reads that were
    // never sent. Refuse before *any* lookup until a conservative estimator and
    // mandatory exact second gate exist.
    if config.fast_estimate {
        out.error("--fast-estimate is unavailable: it cannot safely estimate market IDs; omit it for an exact, spend-gated survey");
        out.set_exit(crate::out::EXIT_FAILURE);
        return Ok(());
    }
    // A quick lookup asks Ardent's commodity index for exact market ids, so it
    // bypasses regional enumeration entirely. It still reaches the same
    // authenticated poller, cache writer and optional EDDN relay below that
    // mode's boundary. Dispatch before the regional `--verify-systems` rule:
    // quick mode gives that incompatible flag its own precise explanation.
    // Before either search: a hold of nought tonnes has no answer, and the one
    // the optimiser would give is a statement about the market rather than about
    // the ship. Reachable from an explicit `--cargo 0` and from a ship that
    // really has no rack.
    if config.cargo == Some(0.0) {
        return Err(
            "the hold has no capacity, so every leg would carry nothing. Pass --cargo <t> with the capacity you will be flying"
                .to_owned(),
        );
    }
    if config.quick.is_some() {
        return quick::run(app, config, timer).await;
    }
    if config.verify_systems && config.radius_ly > edm_core::consts::MARKETDATA_DISTANCE_LY_FALLBACK
    {
        out.error(
            "--verify-systems follows Frontier's 40 Ly marketdata policy; use --radius 40 or less",
        );
        out.set_exit(crate::out::EXIT_FAILURE);
        return Ok(());
    }

    let note = |text: String| {
        if !config.quiet {
            out.line(&text);
        }
    };
    note(format!(
        "resolving \"{}\" through Ardent...",
        config.reference
    ));
    let centre = resolve(&ardent, &config.reference).await?;

    let budget = if config.ardent_queries == 0 {
        DEFAULT_ANCHOR_BUDGET
    } else {
        config.ardent_queries
    };
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
        anchor_report
            .as_ref()
            .map(|f| f as discover::AnchorReport<'_>),
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
    let gather_report =
        (!config.quiet).then_some(move |done: usize, total: usize, found: usize| {
            if done.is_multiple_of(64) || done == total {
                out.progress(&format!(
                    "  {} / {} systems read, {} stations found",
                    edm_core::js::format_integer(done as f64),
                    edm_core::js::format_integer(total as f64),
                    edm_core::js::format_integer(found as f64),
                ));
            }
        });
    let (mut stations, _systems_with_markets) = gather(
        &ardent,
        &atlas,
        &app.ports.fs,
        now_ms,
        &enumeration,
        gather_report.as_ref().map(|f| f as &dyn Fn(_, _, _)),
    )
    .await?;

    let route_started_ms = app.ports.clock.now_ms();
    let mut opportunities = SpecialOpportunities::default();
    let mut digest_requests = 0usize;
    let mut official_topology = None;
    if config.verify_systems {
        let digest_cache =
            crate::route::digest::Cache::new(cache.root(), config.cache, config.refresh);
        let cached = digest_cache.get(&app.ports.fs, DIGEST_SOURCE, app.ports.clock.now_ms());
        if let Some(snapshot) = cached {
            official_topology = Some(snapshot);
        } else {
            // The terminal page is unknowable on a cold cache. Gate the strict
            // crawler's full hard cap before page zero, never a guessed prefix.
            let crawl_survey = Survey {
                complete_to_ly: enumeration.complete_to_ly,
                price_index: false,
                ardent_requests: enumeration.ardent_requests,
                counts: Counts {
                    systems: enumeration.systems.len(),
                    systems_to_read: crate::route::digest::PAGE_CAP,
                    stations_known: 0,
                    markets_to_poll: 0,
                    cached_fresh: 0,
                },
                exclusions: Vec::new(),
            };
            match plan::gate(
                out,
                config,
                &crawl_survey,
                SizePrior {
                    // Live pages are roughly 1.5 MiB at the 4,000-row size.
                    system_bytes: 1.5 * 1024.0 * 1024.0,
                    market_bytes: SizePrior::default().market_bytes,
                    spread: SizePrior::default().spread,
                },
            ) {
                plan::Decision::Sweep(_) => {}
                plan::Decision::Stopped(_) | plan::Decision::Refused(_) => return Ok(()),
            }
            note("crawling Frontier's complete populated-system digest...".to_owned());
            let first = std::cell::Cell::new(true);
            let requests = std::cell::Cell::new(0usize);
            let snapshot = digest_cache
                .get_or_crawl(
                    &app.ports.fs,
                    DIGEST_SOURCE,
                    app.ports.clock.now_ms(),
                    |page| {
                        let first = &first;
                        let requests = &requests;
                        async move {
                            if !first.replace(false) {
                                timer.sleep_ms(1_000.0 / config.rate_per_second).await;
                            }
                            requests.set(requests.get() + 1);
                            let stamp = app.stamp().map_err(|error| error.clone())?;
                            let request = app.prepare(
                                edm_core::consts::STARSYSTEM_DAILY_DIGEST,
                                crate::game_api::daily_digest_fields(
                                    app.cli
                                        .optional_value(edm_core::cli::Flag::Language, None)
                                        .unwrap_or("en"),
                                    u32::try_from(page)
                                        .map_err(|_| "daily-digest page overflow".to_owned())?,
                                    &app.credentials,
                                    stamp.frontier_time,
                                ),
                                stamp,
                            );
                            let exchange = app
                                .send(
                                    &request,
                                    crate::exchange::SendOptions {
                                        quiet: true,
                                        ignore_dry_run: false,
                                    },
                                )
                                .await
                                .ok_or_else(|| {
                                    "daily-digest request produced no response".to_owned()
                                })?;
                            if !(200..300).contains(&exchange.status) {
                                return Err(format!("daily-digest HTTP {}", exchange.status));
                            }
                            exchange.decrypted.ok_or_else(|| {
                                "daily-digest response could not be decoded".to_owned()
                            })
                        }
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            digest_requests = requests.get();
            official_topology = Some(snapshot);
        }
    }

    if let Some(topology) = &official_topology {
        opportunities.rescue_systems = topology
            .systems()
            .iter()
            .filter(|system| system.status.tw_rescue_market == Some(true))
            .count();
    }

    let mut selection = select::select(stations, config, &centre.coordinates);

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
    // step 0 established that the game-internal API answers for a market the
    // commander is not docked at — and a starsystem payload is ~500 KB against
    // a market's ~20 KB, so reading one per system to rediscover ids we already
    // have would be twenty-five times the transfer for the same prices.
    let official_addresses = if let Some(topology) = &official_topology {
        topology
            .within_radius(centre.coordinates, config.radius_ly)
            .into_iter()
            .map(|system| system.system.address)
            .collect()
    } else if config.verify_systems {
        selected_system_addresses(&selection)?
    } else {
        Vec::new()
    };
    // One official bulk request covers five exact system addresses. The spend
    // gate prices the batches before any one of them is sent.
    let systems_to_read = crate::route::marketdata::batches(
        &official_addresses,
        edm_core::consts::MARKETDATA_BATCH_MAX,
    )
    .len();

    let survey = Survey {
        complete_to_ly: enumeration.complete_to_ly,
        price_index: false,
        ardent_requests: enumeration.ardent_requests,
        counts: Counts {
            systems: enumeration.systems.len(),
            systems_to_read: systems_to_read + digest_requests,
            stations_known: selection.considered,
            markets_to_poll: if config.verify_systems {
                0
            } else {
                selection.keep.len()
            },
            cached_fresh: if config.verify_systems {
                0
            } else {
                prepared.hits.fresh
            },
        },
        exclusions: selection.exclusions.clone(),
    };

    let prior = if config.verify_systems {
        // One marketdata batch may contain five systems. Budget transfer at the
        // old per-system starsystem size rather than pretending batching makes
        // the bytes disappear.
        SizePrior {
            system_bytes: 5.0 * SizePrior::default().system_bytes,
            ..SizePrior::default()
        }
    } else {
        SizePrior::default()
    };
    let decision = plan::gate(out, config, &survey, prior);
    if !decision.proceeds() {
        return Ok(());
    }

    let started_ms = route_started_ms;
    let mut candidate_demand_prices = HashMap::new();
    let mut official_systems_read = 0usize;
    let mut official_systems_failed = 0usize;
    let mut official_requests = 0usize;

    if config.verify_systems {
        note(format!(
            "officially enriching {} systems in batches of {}...",
            edm_core::js::format_integer(official_addresses.len() as f64),
            edm_core::js::format_integer(edm_core::consts::MARKETDATA_BATCH_MAX as f64),
        ));
        let rules = edm_core::domain::resources::FinanceRules::default();
        let official_cache = crate::route::marketdata::Cache::new(
            cache.root().to_path_buf(),
            config.max_age_minutes,
            config.cache,
            config.refresh,
        );
        let first = std::cell::Cell::new(true);
        let requests = std::cell::Cell::new(0usize);
        let official = crate::route::marketdata::acquire(
            &official_addresses,
            &official_cache,
            &app.ports.fs,
            app.ports.clock.now_ms(),
            rules,
            |batch| {
                let first = &first;
                let requests = &requests;
                async move {
                    if !first.replace(false) {
                        timer.sleep_ms(1_000.0 / config.rate_per_second).await;
                    }
                    requests.set(requests.get() + 1);
                    let stamp = app.stamp().map_err(|error| error.clone())?;
                    let fields = crate::game_api::marketdata_fields(
                        &batch,
                        &app.credentials,
                        stamp.frontier_time,
                    )?;
                    let request =
                        app.prepare(edm_core::consts::STARSYSTEM_MARKETDATA, fields, stamp);
                    let exchange = app
                        .send(
                            &request,
                            crate::exchange::SendOptions {
                                quiet: true,
                                ignore_dry_run: false,
                            },
                        )
                        .await
                        .ok_or_else(|| "marketdata request produced no response".to_owned())?;
                    if !(200..300).contains(&exchange.status) {
                        return Err(format!("marketdata HTTP {}", exchange.status));
                    }
                    exchange
                        .decrypted
                        .ok_or_else(|| "marketdata response could not be decoded".to_owned())
                }
            },
        )
        .await;
        official_requests = requests.get();
        official_systems_read = official.systems.len();
        official_systems_failed = official.missing.len();
        if !official.failed_batches.is_empty() || !official.missing.is_empty() {
            return Err(format!(
                "official marketdata incomplete: {} systems missing across {} failed batches",
                official.missing.len(),
                official.failed_batches.len(),
            ));
        }
        opportunities.colonisation_markets = official
            .systems
            .iter()
            .flat_map(|system| &system.markets)
            .filter(|market| market.colonisation_template.is_some())
            .count();
        opportunities.stateful_markets = official
            .systems
            .iter()
            .flat_map(|system| &system.markets)
            .filter(|market| !market.market_state.is_empty())
            .count();
        opportunities.commodity_override_markets = official
            .systems
            .iter()
            .flat_map(|system| &system.markets)
            .filter(|market| market.commodity_overrides_only)
            .count();
        if let Some(topology) = &official_topology {
            stations = official_stations(topology, &official.systems);
            selection = select::select(stations, config, &centre.coordinates);
        }
        let removed = apply_official_enrichment(
            &mut selection,
            &official.systems,
            config,
            &mut candidate_demand_prices,
        );
        if removed > 0 {
            note(format!(
                "{} official candidates failed exact pad/service checks and were excluded",
                edm_core::js::format_integer(removed as f64),
            ));
        }
    }

    // Official enrichment can only remove known candidates, never add an
    // un-gated market. Recompute the verified-price cache pass for that subset.
    let prepared = acquire::prepare(
        &cache,
        &app.ports.fs,
        &selection.keep,
        app.ports.clock.now_ms(),
    );

    if config.verify_systems {
        // Discovery has now exposed the exact market set. Gate authoritative
        // quantity reads separately; candidate rows can never satisfy them.
        let exact_survey = Survey {
            complete_to_ly: enumeration.complete_to_ly,
            price_index: false,
            ardent_requests: enumeration.ardent_requests,
            counts: Counts {
                systems: official_addresses.len(),
                systems_to_read: digest_requests + official_requests,
                stations_known: selection.considered,
                markets_to_poll: selection.keep.len(),
                cached_fresh: prepared.hits.fresh,
            },
            exclusions: selection.exclusions.clone(),
        };
        match plan::gate(out, config, &exact_survey, prior) {
            plan::Decision::Sweep(_) => {}
            plan::Decision::Stopped(_) | plan::Decision::Refused(_) => return Ok(()),
        }
    }

    // The exact-price phase starts here. Discovery requests above had their own
    // explicit gates; no candidate payload can cross this boundary as a listing.
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
    // EDDN's own bucket. A burst that is fine for Frontier is not fine for a
    // shared community service, and before this the two shared one rate.
    let eddn_bucket = edm_core::pace::Bucket {
        rate: config.eddn_rate_per_second,
        burst: 1.0,
        min_rate: edm_core::js::js_min(config.eddn_rate_per_second, 0.5),
    };
    let eddn_tokens = std::cell::RefCell::new(edm_core::pace::BucketState::new(
        eddn_bucket,
        app.ports.clock.now_ms(),
    ));
    let eddn = eddn_options.as_ref().map(|options| acquire::Eddn {
        options,
        url: &app.overrides.eddn_url,
        relayed: &relayed_log,
        stations: &selection.keep,
        bucket: eddn_bucket,
        tokens: &eddn_tokens,
    });
    let relay_tally = std::cell::RefCell::new(crate::route::relay::Tally::default());

    let sweep_cx = acquire::Cx {
        http: app.http,
        clock: &app.ports.clock,
        timer,
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
        // Official verification already happened through batched marketdata;
        // do not enter the legacy lossy starsystem follow-on path.
        verify_systems: false,
        language: &query.language,
        report: (!config.quiet).then_some(&report as crate::route::pool::Report<'_>),
        trace: (config.verbose && !config.quiet).then_some(&trace as crate::route::pool::Trace<'_>),
        total,
    };
    // Only the systems that still hold a candidate market are worth an
    // authoritative read; the rest were emptied by the filter and a 500 KB
    // payload would confirm nothing.
    let systems: Vec<(String, f64)> = Vec::new();
    let acquired = acquire::sweep(&sweep_cx, &pacer, prepared, &systems).await;

    let at_ms = app.ports.clock.now_ms();
    let coverage = coverage_of(&Measured {
        selection: &selection,
        acquired: &acquired,
        enumeration: &enumeration,
        spent: pacer.spent(),
        eddn: config.eddn,
        priced: ingest::priced(&acquired.listings),
        breaker_tripped: pacer.tripped().is_some(),
        elapsed_seconds: (at_ms - started_ms) / 1000.0,
        at_ms,
        official_systems_total: official_addresses.len(),
        official_systems_read,
        official_systems_failed,
        official_requests: official_requests + digest_requests,
    });
    if !config.json
        && (opportunities.rescue_systems > 0
            || opportunities.colonisation_markets > 0
            || opportunities.stateful_markets > 0
            || opportunities.commodity_override_markets > 0)
    {
        out.line(&format!(
            "special opportunities observed: {} rescue systems, {} colonisation markets, {} stateful markets, {} override-only markets",
            opportunities.rescue_systems,
            opportunities.colonisation_markets,
            opportunities.stateful_markets,
            opportunities.commodity_override_markets,
        ));
    }

    // Under `--json` the coverage block is inside the document instead; on
    // stderr it would be the same information twice.
    if !config.json {
        out.aside(&views::route_coverage(&coverage));
    }

    // Read before `rank` consumes the listings; the ranking cannot change it.
    let unreached = !acquired.unreached.is_empty() || acquired.tally.markets_out_of_time > 0;

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
    let watch = if config.quiet {
        watch
    } else {
        watch.reporting(&report_progress)
    };

    rank(
        out,
        config,
        acquired,
        &selection.keep,
        &candidate_demand_prices,
        &coverage,
        opportunities,
        None,
        watch,
    );

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
    at_ms: f64,
    official_systems_total: usize,
    official_systems_read: usize,
    official_systems_failed: usize,
    official_requests: usize,
}

/// What the run reached, and what it did not.
fn coverage_of(m: &Measured<'_>) -> RouteCoverage {
    RouteCoverage {
        systems_total: m.official_systems_total,
        systems_read: m.official_systems_read,
        systems_failed: m.official_systems_failed,
        markets_found: m.selection.keep.len(),
        // Live requests which reached a terminal answer. Cached listings are
        // priced below but were not polled during this run.
        markets_polled: m.acquired.tally.markets_polled + m.acquired.tally.markets_absent,
        markets_priced: m.priced,
        // `Tally` is the typed terminal ledger; it includes retry exhaustion,
        // deadline cut-offs and queued jobs retired by the breaker exactly once.
        markets_failed: m.acquired.tally.markets_failed,
        markets_absent: m.acquired.tally.markets_absent,
        eddn: m.eddn.then_some(edm_core::render::views::EddnCoverage {
            sent: m.acquired.relayed.sent,
            failed: m.acquired.relayed.failed,
            recent: m.acquired.relayed.recent,
            cached: m.acquired.relayed.cached,
            unnamed: m.acquired.relayed.unnamed,
            abandoned: m.acquired.relayed.abandoned,
        }),
        cache_hits: m.acquired.cache.fresh,
        requests_sent: m.spent.requests + m.official_requests,
        throttled: m.spent.throttled,
        elapsed_seconds: m.elapsed_seconds,
        oldest_observed_ms: m
            .acquired
            .listings
            .iter()
            .filter_map(|listing| listing.observed_at_ms)
            .min_by(f64::total_cmp),
        newest_observed_ms: m
            .acquired
            .listings
            .iter()
            .filter_map(|listing| listing.observed_at_ms)
            .fold(None::<f64>, |newest, observed| {
                Some(newest.map_or(observed, |current| {
                    if current.total_cmp(&observed).is_lt() {
                        observed
                    } else {
                        current
                    }
                }))
            }),
        observation_time_unknown: m
            .acquired
            .listings
            .iter()
            .filter(|listing| listing.observed_at_ms.is_none())
            .count(),
        measured_at_ms: m.at_ms,
        truncated_to_ly: m
            .enumeration
            .truncated
            .then_some(m.enumeration.complete_to_ly),
        breaker_tripped: m.breaker_tripped,
        ranked: true,
        eddn_refusal: m.acquired.relayed.first_refusal.clone(),
    }
}

/// Solve, and print what the search will actually claim.
#[allow(
    clippy::too_many_arguments,
    reason = "orchestration boundary keeps ranking inputs explicit"
)]
fn rank(
    out: &crate::out::Out,
    config: &RouteConfig,
    acquired: acquire::Acquired,
    stations: &[ardent::ArdentStation],
    candidate_demand_prices: &HashMap<(i64, i64), i64>,
    coverage: &RouteCoverage,
    opportunities: SpecialOpportunities,
    quick: Option<&QuickProvenance>,
    watch: edm_route::watch::Watch<'_>,
) {
    // By value: `ingest::markets` drops each payload as it builds its `Market`,
    // and at five thousand markets those payloads are ~2.3 GiB that would
    // otherwise stay resident through the graph build.
    let (markets, commodities, crossing) = ingest::markets_with_candidates(
        acquired.listings,
        stations,
        &ingest::floors(config),
        candidate_demand_prices,
    );
    // Not under `--json`: a diagnostic in the middle of the stream is exactly
    // what R76 does to the ported commands, and C28 says route's document is
    // one well-formed document or nothing.
    if crossing.non_integral > 0 && !config.json {
        out.line(&format!(
            "{} commodity rows carried a non-integral price or quantity and were skipped",
            edm_core::js::format_integer(f64::from(crossing.non_integral))
        ));
    }
    if crossing.invalid_identity > 0 && !config.json {
        out.line(&format!(
            "{} listings lacked a stable market/system identity or finite coordinates and were skipped",
            edm_core::js::format_integer(f64::from(crossing.invalid_identity))
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
    let mut solution = edm_route::solve(
        &markets,
        time_model(config),
        &ship(config),
        &limits(config),
        edm_route::Wanted::only(kind),
        watch,
    );
    if !candidate_demand_prices.is_empty() {
        for route in solution
            .single
            .iter_mut()
            .chain(solution.round_trip.iter_mut())
            .chain(solution.loops.iter_mut())
        {
            route.mark_bulk_price_estimated();
        }
    }
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
            coverage_json(coverage, &crossing, opportunities, quick),
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
    opportunities: SpecialOpportunities,
    quick: Option<&QuickProvenance>,
) -> edm_core::js::json::JsValue {
    use edm_core::js::json::{JsObject, JsValue};
    let n = |value: usize| JsValue::Num(value as f64);
    let mut fields = vec![
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
        (
            "elapsedSeconds".into(),
            JsValue::Num(coverage.elapsed_seconds),
        ),
        (
            "oldestMarketObservedAt".into(),
            coverage
                .oldest_observed_ms
                .map_or(JsValue::Null, JsValue::Num),
        ),
        (
            "newestMarketObservedAt".into(),
            coverage
                .newest_observed_ms
                .map_or(JsValue::Null, JsValue::Num),
        ),
        (
            "marketObservationTimeUnknown".into(),
            n(coverage.observation_time_unknown),
        ),
        (
            "completeToLy".into(),
            coverage.truncated_to_ly.map_or(JsValue::Null, JsValue::Num),
        ),
        (
            "breakerTripped".into(),
            JsValue::Bool(coverage.breaker_tripped),
        ),
        (
            "rowsSkippedNonIntegral".into(),
            JsValue::Num(f64::from(crossing.non_integral)),
        ),
        (
            "listingsSkippedInvalidIdentity".into(),
            JsValue::Num(f64::from(crossing.invalid_identity)),
        ),
        (
            "specialOpportunities".into(),
            JsValue::Obj(JsObject::from_document_order(vec![
                ("rescueSystems".into(), n(opportunities.rescue_systems)),
                (
                    "colonisationMarkets".into(),
                    n(opportunities.colonisation_markets),
                ),
                ("statefulMarkets".into(), n(opportunities.stateful_markets)),
                (
                    "commodityOverrideMarkets".into(),
                    n(opportunities.commodity_override_markets),
                ),
            ])),
        ),
    ];
    if let Some(quick) = quick {
        fields.push(("quickLookup".into(), quick_lookup_json(quick)));
    }
    fields.push((
        "notes".into(),
        JsValue::Arr(
            coverage
                .notes()
                .into_iter()
                .map(|note| JsValue::Str(note.into()))
                .collect(),
        ),
    ));
    JsValue::Obj(JsObject::from_document_order(fields))
}

/// The provenance block for `--quick`, split out because it is a document of
/// its own: what was asked for, what bounded the answer, and what the live
/// reads found.
fn quick_lookup_json(quick: &QuickProvenance) -> edm_core::js::json::JsValue {
    use edm_core::js::json::{JsObject, JsValue};
    let n = |value: usize| JsValue::Num(value as f64);
    let strings = |values: &[String]| {
        JsValue::Arr(
            values
                .iter()
                .cloned()
                .map(Into::into)
                .map(JsValue::Str)
                .collect(),
        )
    };
    JsValue::Obj(JsObject::from_document_order(vec![
        ("commodities".into(), strings(&quick.commodities)),
        ("marketsPerSide".into(), n(quick.markets_per_side)),
        ("minimumSupply".into(), JsValue::Num(quick.seller_minimum)),
        ("minimumDemand".into(), JsValue::Num(quick.buyer_minimum)),
        ("candidateRows".into(), n(quick.candidate_rows)),
        (
            "marketIds".into(),
            JsValue::Arr(quick.market_ids.iter().copied().map(JsValue::Num).collect()),
        ),
        (
            "unpublishedBuyerCandidates".into(),
            n(quick.unpublished_buyer_candidates),
        ),
        (
            "bestLive".into(),
            JsValue::Arr(quick.best_live.iter().map(best_live_json).collect()),
        ),
        (
            "commoditiesWithoutCandidates".into(),
            strings(&quick.commodities_without_candidates),
        ),
        (
            "commoditiesAbsentFromIndex".into(),
            strings(&quick.commodities_absent_from_index),
        ),
        (
            "indexPageCap".into(),
            n(edm_core::cli::config::QUICK_LOOKUP_MAX_MARKETS_PER_SIDE),
        ),
        ("includesReferenceSystem".into(), JsValue::Bool(true)),
        ("completeRegionalSurvey".into(), JsValue::Bool(false)),
    ]))
}

/// One best live seller or buyer.
fn best_live_json(entry: &quick::BestLive) -> edm_core::js::json::JsValue {
    use edm_core::js::json::{JsObject, JsValue};
    JsValue::Obj(JsObject::from_document_order(vec![
        (
            "commodity".into(),
            JsValue::Str(entry.commodity.clone().into()),
        ),
        ("name".into(), JsValue::Str(entry.display.clone().into())),
        (
            "side".into(),
            JsValue::Str(entry.direction.market_role().into()),
        ),
        ("price".into(), JsValue::Num(entry.price)),
        // Null rather than zero: an unreported demand is a quantity nobody
        // published, not a market that buys nothing.
        (
            "quantity".into(),
            if entry.unpublished {
                JsValue::Null
            } else {
                JsValue::Num(entry.volume)
            },
        ),
        (
            "quantityUnpublished".into(),
            JsValue::Bool(entry.unpublished),
        ),
        (
            "indexPrice".into(),
            entry.index_price.map_or(JsValue::Null, JsValue::Num),
        ),
        ("marketId".into(), JsValue::Num(entry.market_id)),
        (
            "stationName".into(),
            JsValue::Str(entry.station.clone().into()),
        ),
        (
            "systemName".into(),
            JsValue::Str(entry.system.clone().into()),
        ),
        ("distanceLy".into(), JsValue::Num(entry.distance_ly)),
    ]))
}

/// The travel model, with every constant a flag.
fn time_model(config: &RouteConfig) -> TimeModel {
    TimeModel {
        jump_range_ly: config.jump_range_ly,
        ..TimeModel::default()
    }
}

/// The ship, or the absence of one.
///
/// An omitted `--cargo` or `--credits` means *unbounded*, not zero: the answer
/// is then the best route the data admits for a ship that can carry it, which
/// is the right default for "what is out there".
fn ship(config: &RouteConfig) -> ShipConfig {
    ShipConfig {
        cargo: config
            .cargo
            .map_or(ShipConfig::default().cargo, |tons| Tons(tons as i64)),
        credits: config
            .credits
            .map_or(ShipConfig::default().credits, |credits| {
                Credits(credits as i64)
            }),
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
    Ok(ardent
        .resolve_location(reference, Lookup::Auto)
        .await?
        .system)
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
/// reports a smaller region rather than a wrong one. The game-internal API is what
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
) -> Result<(Vec<ardent::ArdentStation>, usize), String> {
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
            let result = ardent
                .system_markets_cached(atlas, fs, now_ms, &reference)
                .await;
            done.set(done.get() + 1);
            if let Ok(stations) = &result {
                answered.set(answered.get() + 1);
                found.set(found.get() + stations.len());
            }
            // Reported from inside the task, in completion order, so a long
            // gather visibly moves rather than sitting silent for minutes.
            if let Some(report) = report {
                report(done.get(), total, found.get());
            }
            result
                .map_err(|error| format!("reading Ardent markets for {}: {error}", reference.name))
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
    for result in results {
        let mut batch = result?;
        stations.append(&mut batch);
    }
    Ok((stations, answered.get()))
}

fn selected_system_addresses(selection: &select::Selection) -> Result<Vec<u64>, String> {
    let mut addresses = Vec::new();
    for station in &selection.keep {
        let address = station.system_address;
        if !address.is_finite()
            || address.fract() != 0.0
            || !(1.0..=9_007_199_254_740_992.0).contains(&address)
        {
            return Err(format!(
                "{} / {} has no exact official system address",
                station.system_name, station.station_name,
            ));
        }
        addresses.push(address as u64);
    }
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

fn official_stations(
    topology: &crate::route::digest::Snapshot,
    systems: &[edm_core::domain::marketdata::SystemMarketData],
) -> Vec<ardent::ArdentStation> {
    let coordinates = topology
        .systems()
        .iter()
        .map(|system| (system.address, system.coordinates))
        .collect::<HashMap<_, _>>();
    let mut stations = Vec::new();
    for system in systems {
        let Some(&coords) = coordinates.get(&system.address) else {
            continue;
        };
        if system.address > 9_007_199_254_740_992 {
            continue;
        }
        for market in &system.markets {
            if market.market_id == 0 || market.market_id > 9_007_199_254_740_992 {
                continue;
            }
            // `service_commodities=2` is the official fleet-carrier service;
            // pad shape distinguishes Odyssey settlements from outposts.
            let station_type = if market.commodities_service == 2 {
                "Fleet Carrier"
            } else if market.small_pads && !market.medium_pads && !market.large_pads {
                "Odyssey Settlement"
            } else if !market.large_pads {
                "Outpost"
            } else {
                "Starport"
            };
            stations.push(ardent::ArdentStation {
                market_id: market.market_id as f64,
                station_name: market.name.clone(),
                system_name: system.name.clone(),
                system_address: system.address as f64,
                station_type: Some(station_type.to_owned()),
                max_landing_pad_size: Some(if market.large_pads {
                    3.0
                } else if market.medium_pads {
                    2.0
                } else {
                    1.0
                }),
                distance_to_arrival: market.arrival_ls.is_finite().then_some(market.arrival_ls),
                coordinates: coords,
            });
        }
    }
    stations.sort_by(|left, right| {
        left.system_address
            .total_cmp(&right.system_address)
            .then_with(|| left.market_id.total_cmp(&right.market_id))
    });
    stations
}

/// Pair official prices with selected official/Ardent candidates and enforce
/// exact pad and commodity-service access. Official discovery is selected only
/// after its own gate and receives a second gate before authoritative reads.
fn apply_official_enrichment(
    selection: &mut select::Selection,
    systems: &[edm_core::domain::marketdata::SystemMarketData],
    config: &RouteConfig,
    candidate_demand_prices: &mut HashMap<(i64, i64), i64>,
) -> usize {
    let before = selection.keep.len();
    let mut kept = Vec::with_capacity(before);
    for mut station in selection.keep.drain(..) {
        let address = station.system_address as u64;
        let Some(system) = systems.iter().find(|system| system.address == address) else {
            continue;
        };
        let market_id = station.market_id as u64;
        let Some(market) = system
            .markets
            .iter()
            .find(|market| market.market_id == market_id)
        else {
            continue;
        };
        let pad_ok = match config.pad {
            Pad::Large => market.large_pads,
            Pad::Medium => market.large_pads || market.medium_pads,
            Pad::Small => market.large_pads || market.medium_pads || market.small_pads,
        };
        if !pad_ok || market.commodities_service <= 0 {
            continue;
        }

        station.system_name.clone_from(&system.name);
        station.station_name.clone_from(&market.name);
        station.distance_to_arrival = market.arrival_ls.is_finite().then_some(market.arrival_ls);
        station.max_landing_pad_size = Some(if market.large_pads {
            3.0
        } else if market.medium_pads {
            2.0
        } else {
            1.0
        });
        if let Ok(market_id) = i64::try_from(market.market_id) {
            for commodity in &market.commodities {
                let Some(price) = commodity.demand_price() else {
                    continue;
                };
                let Ok(commodity_id) = i64::try_from(commodity.commodity_id) else {
                    continue;
                };
                if price > 0 {
                    candidate_demand_prices.insert((market_id, commodity_id), price);
                }
            }
        }
        kept.push(station);
    }
    let removed = before - kept.len();
    selection.keep = kept;
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use edm_core::ardent::ArdentStation;
    use edm_core::domain::id64::Coordinates;

    fn station(system: &str, address: f64) -> ArdentStation {
        ArdentStation {
            market_id: address,
            station_name: "S".to_owned(),
            system_name: system.to_owned(),
            system_address: address,
            station_type: Some("Coriolis".to_owned()),
            max_landing_pad_size: None,
            distance_to_arrival: None,
            coordinates: Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        }
    }

    #[test]
    fn official_system_addresses_are_exact_deduplicated_and_sorted() {
        let selection = select::Selection {
            keep: vec![
                station("Sol", 10_477_373_803.0),
                station("Sol", 10_477_373_803.0),
                station("Alpha Centauri", 22_655_943_295.0),
            ],
            exclusions: Vec::new(),
            considered: 3,
        };
        assert_eq!(
            selected_system_addresses(&selection).expect("exact IDs"),
            vec![10_477_373_803, 22_655_943_295]
        );
        let invalid = select::Selection {
            keep: vec![station("Unknown", f64::NAN)],
            ..select::Selection::default()
        };
        assert!(selected_system_addresses(&invalid).is_err());
    }

    #[test]
    fn an_empty_region_prices_no_official_batches() {
        assert!(
            selected_system_addresses(&select::Selection::default())
                .expect("empty is valid")
                .is_empty()
        );
    }

    fn route_config(argv: &[&str]) -> RouteConfig {
        let owned = argv
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        let parsed = edm_core::cli::parse_with(&owned, edm_core::cli::Table::Extended)
            .expect("route parses");
        let env = edm_core::cli::EnvSnapshot::empty();
        edm_core::cli::config::route_config(&edm_core::cli::Cli::new(&parsed, &env))
            .expect("route config")
    }

    #[test]
    fn official_enrichment_applies_pads_services_and_candidate_demand_only() {
        let document = edm_core::js::json::JsValue::parse(
            r#"{
          "starsystems":{"1":{"systemAddr":"1","name":"Official System","hasFleetCarriers":false,
          "techBroker":"none","materialTrader":"none","blackMarket":false,"facilitator":false,
          "voucherredemption":false,"carrierVendor":false,"modulepacks":false,"cacheuntil":1,
          "markets":{"1001":{"id":"1001","systemName":"Official System","name":"Official Port",
          "distFromSystem":12,"market_state":"","starsystem_id":"1","service_blackmarket":"0",
          "service_commodities":"1","commodities":{"128049165":{"type":"consumer","sellPrice":41832,
          "illegalJurisdictionQty":0}},"allowDumping":true,"simulatedAt":1,
          "smallPads":true,"mediumPads":true,"largePads":true,"surface":false}}}}}"#,
        )
        .expect("fixture JSON");
        let systems = edm_core::domain::marketdata::parse_marketdata(&document).systems;
        let mut candidate = station("Candidate", 1.0);
        candidate.market_id = 1001.0;
        let mut selection = select::Selection {
            keep: vec![candidate],
            exclusions: Vec::new(),
            considered: 1,
        };
        let mut prices = HashMap::new();
        let removed = apply_official_enrichment(
            &mut selection,
            &systems,
            &route_config(&["route", "Sol", "--verify-systems"]),
            &mut prices,
        );
        assert_eq!(removed, 0);
        assert_eq!(selection.keep[0].station_name, "Official Port");
        assert_eq!(selection.keep[0].distance_to_arrival, Some(12.0));
        assert_eq!(prices.get(&(1001, 128_049_165)), Some(&41_832));
    }

    #[test]
    fn special_opportunities_are_structured_in_json_coverage() {
        let opportunities = SpecialOpportunities {
            rescue_systems: 1,
            colonisation_markets: 2,
            stateful_markets: 3,
            commodity_override_markets: 4,
        };
        let edm_core::js::json::JsValue::Obj(coverage) = coverage_json(
            &RouteCoverage::default(),
            &ingest::Crossing::default(),
            opportunities,
            None,
        ) else {
            panic!("coverage object")
        };
        let Some(edm_core::js::json::JsValue::Obj(special)) = coverage.get("specialOpportunities")
        else {
            panic!("special opportunities object")
        };
        assert_eq!(
            special.get("rescueSystems"),
            Some(&edm_core::js::json::JsValue::Num(1.0))
        );
        assert_eq!(
            special.get("commodityOverrideMarkets"),
            Some(&edm_core::js::json::JsValue::Num(4.0))
        );
        assert!(coverage.get("quickLookup").is_none());
    }
}
