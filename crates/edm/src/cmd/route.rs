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

/// Run the command.
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs, T: Timer>(
    app: &App<'_, H, C, E, F>,
    config: &RouteConfig,
    timer: &T,
) -> CmdResult {
    let out = app.out;
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);

    // Nothing below this point may run on a name that was never resolved: an
    // enumeration centred on the wrong system is a complete, confident answer
    // about the wrong region.
    let centre = resolve(&ardent, &config.reference).await?;

    let budget = if config.ardent_queries == 0 { DEFAULT_ANCHOR_BUDGET } else { config.ardent_queries };
    let enumeration = discover::enumerate(&ardent, &centre, config.radius_ly, budget)
        .await
        .map_err(|error| format!("enumerating systems around {}: {error}", centre.name))?;

    // One free `/markets` per system, then the filter. Both happen before the
    // plan is priced, so the plan's market count is measured rather than
    // extrapolated — `--fast-estimate` is the flag that trades this away.
    let (stations, systems_with_markets) = if config.fast_estimate {
        (Vec::new(), enumeration.systems.len())
    } else {
        gather(&ardent, &enumeration).await
    };

    let selection = select::select(stations, config, &centre.coordinates);
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
            cached_fresh: 0,
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
        cache: &cache_for(app, config),
        workers: config.workers as usize,
        quiet: config.json,
        verify_systems: config.verify_systems,
        language: &query.language,
    };
    // Only the systems that still hold a candidate market are worth an
    // authoritative read; the rest were emptied by the filter and a 500 KB
    // payload would confirm nothing.
    let systems: Vec<(String, f64)> = holding_systems(&enumeration, &selection);
    let acquired = acquire::sweep(&sweep_cx, &pacer, &selection.keep, &systems).await;

    let spent = pacer.spent();
    let coverage = RouteCoverage {
        systems_total: survey.counts.systems_to_read,
        systems_read: survey.counts.systems_to_read,
        systems_failed: 0,
        markets_found: selection.keep.len(),
        markets_polled: acquired.listings.len(),
        markets_priced: acquired.listings.iter().filter(|l| l.snapshot().is_some()).count(),
        markets_failed: acquired.unreached.len(),
        cache_hits: acquired.cache.fresh,
        requests_sent: spent.requests,
        throttled: spent.throttled,
        elapsed_seconds: (app.ports.clock.now_ms() - started_ms) / 1000.0,
        truncated_to_ly: enumeration.truncated.then_some(enumeration.complete_to_ly),
        breaker_tripped: pacer.tripped().is_some(),
    };
    out.emit(&views::route_coverage(&coverage));

    rank(out, config, &acquired, &selection.keep);

    // A market in radius that was never read is not a market that ranked
    // badly, and the exit code says so.
    if !acquired.unreached.is_empty() || coverage.breaker_tripped {
        out.set_exit(crate::out::EXIT_FAILURE);
    }
    Ok(())
}

/// Solve, and print what the search will actually claim.
fn rank(
    out: &crate::out::Out,
    config: &RouteConfig,
    acquired: &acquire::Acquired,
    stations: &[ardent::ArdentStation],
) {
    let (markets, commodities, crossing) =
        ingest::markets(&acquired.listings, stations, ingest::floors(config));
    if crossing.non_integral > 0 {
        // Never silently rounded: a fractional price is the wire reporting
        // something this program has no model for.
        out.line(&format!(
            "{} commodity rows carried a non-integral price or quantity and were skipped",
            edm_core::js::format_integer(f64::from(crossing.non_integral))
        ));
    }

    let solution = edm_route::solve(&markets, time_model(config), &ship(config), &limits(config));

    // Only the shape that was asked for. Printing all three would bury the
    // answer, and the two the user did not ask for carry different claims.
    let (kind, routes) = match config.shape {
        Shape::OneWay => (RouteKind::SingleHop, &solution.single),
        Shape::RoundTrip => (RouteKind::RoundTrip, &solution.round_trip),
        Shape::Loop | Shape::BoundedLoop(_) => (RouteKind::Loop { stops: 0 }, &solution.loops),
    };
    out.emit(&view::ranking(kind, routes, &markets));

    if config.detail {
        for route in routes {
            out.emit(&view::legs(route, &markets, &commodities));
        }
    }
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

/// One `/markets` per enumerated system.
///
/// Free and unmetered, so this is not paced and failures are not retried: a
/// system whose market list does not answer contributes nothing, and the plan
/// reports a smaller region rather than a wrong one. Returns the stations and
/// how many systems answered at all.
async fn gather<H: HttpTransport>(
    ardent: &ArdentClient<'_, H>,
    enumeration: &discover::Enumeration,
) -> (Vec<ardent::ArdentStation>, usize) {
    let mut stations = Vec::new();
    let mut answered = 0;
    for system in &enumeration.systems {
        // `system_markets` places the rows at these coordinates itself, which
        // is the only reason a `/markets` row has any position at all.
        let reference = ReferenceSystem {
            name: system.name.clone(),
            address: system.address,
            coordinates: system.coordinates,
        };
        let Ok(mut found) = ardent.system_markets(&reference).await else { continue };
        answered += 1;
        stations.append(&mut found);
    }
    (stations, answered)
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
