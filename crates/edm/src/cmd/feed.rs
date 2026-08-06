//! `edm eddn market` — read named markets and relay them to EDDN \[C33\].
//!
//! `route --eddn` publishes what a route search happened to read. That is the
//! wrong shape for refreshing a region whose data has gone stale: it is bounded
//! by a radius, filtered to berths a big ship can use, and it spends a search on
//! markets it is only visiting in order to publish them.
//!
//! This names its targets instead. Every market under them is read live and
//! relayed — **live, never from the cache**, because a cached listing was read
//! at some earlier instant and republishing it would stamp that old reading with
//! the current time. Filling in stale data with staler data is worse than
//! leaving it alone.
//!
//! The pacing, the retry budget, the relay window and the message builder are
//! all the ones `route` uses, unchanged.

use edm_core::ardent::ArdentStation;
use edm_core::cli::feed::{Feed, FeedConfig, Target};
use edm_core::domain::id64::Coordinates;
use edm_core::js;
use edm_core::pace::{Bucket, Budget};

use crate::ardent::ArdentClient;
use crate::cmd::{App, CmdResult};
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs, Timer};
use crate::route::acquire::{self, Prepared};
use crate::route::cache::Cache;
use crate::route::pacer::{Pacer, Pacing};
use crate::route::pool::Job;
use crate::route::relay::Relayed;

/// Run the command.
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence, and the order is the safeguard: resolve, count, \
              show the ceiling, then read"
)]
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs, T: Timer>(
    app: &App<'_, H, C, E, F>,
    config: &FeedConfig,
    timer: &T,
) -> CmdResult {
    let Feed::Market = config.feed;
    let out = app.out;
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);
    let note = |text: String| {
        if !config.quiet {
            out.line(&text);
        }
    };

    // Ardent resolves every target to the stations under it, and supplies the
    // system and station names EDDN's schema requires and the market payload
    // does not carry.
    let cache = Cache::new(
        Cache::locate(
            app.cli.env("XDG_CACHE_HOME"),
            app.cli.env("HOME"),
            config.cache_dir.as_deref(),
        ),
        0.0,
        // The price cache is *written* — a refreshed listing is worth keeping
        // for a later `route` — but never *read*, because a cached listing is
        // never relayed and reading one would only skip the poll this command
        // exists to make.
        true,
        true,
    );
    let atlas = crate::route::atlas::Atlas::new(cache.root(), true, false);
    let now_ms = app.ports.clock.now_ms();

    note(format!(
        "resolving {} {} through Ardent ({} at a time)...",
        js::format_integer(config.targets.len() as f64),
        if config.targets.len() == 1 { "target" } else { "targets" },
        js::format_integer(ARDENT_CONCURRENCY as f64),
    ));
    // A counter rewritten in place: 629 lines of "resolved X" is not a progress
    // report, it is the output.
    let resolve_report = (!config.quiet).then_some(
        move |done: usize, total: usize, found: usize| {
            if done.is_multiple_of(32) || done == total {
                out.progress(&format!(
                    "  {} / {} targets resolved, {} markets found",
                    js::format_integer(done as f64),
                    js::format_integer(total as f64),
                    js::format_integer(found as f64),
                ));
            }
        },
    );
    let (stations, skipped) = resolve(
        &ardent,
        &atlas,
        &app.ports.fs,
        now_ms,
        &config.targets,
        resolve_report.as_ref().map(|f| f as &dyn Fn(_, _, _)),
    )
    .await;

    // Named, not counted: in a hand-written list the line is what needs
    // correcting, and *which* of these three things happened decides whether
    // anything needs correcting at all.
    let mut failed = 0;
    for skip in &skipped {
        match &skip.why {
            // Not an error. Ardent knows the system perfectly well; there is
            // simply nothing there to publish, which is a fact about the
            // galaxy and not about the list.
            Skip::NoMarkets => note(format!("{} has no markets", skip.target)),
            Skip::Unknown => {
                failed += 1;
                out.error(&format!("Ardent has no system or market called \"{}\"", skip.target));
            }
            Skip::Failed(why) => {
                failed += 1;
                out.error(&format!("{}: {why}", skip.target));
            }
        }
    }

    if stations.is_empty() {
        if failed > 0 {
            return Err("nothing to import: no target resolved to a market".to_owned());
        }
        // Every target was real and none of them trades. Nothing to do, and
        // nothing wrong.
        note("nothing to import: none of these systems has a market".to_owned());
        return Ok(());
    }

    note(format!(
        "{} markets to read",
        js::format_integer(stations.len() as f64)
    ));
    if stations.len() as f64 > config.max_requests {
        out.error_paragraph(&format!(
            "{} markets is above the {} ceiling. Narrow the list or raise it with\n\
             --max-requests {}. Nothing has been sent.",
            js::format_integer(stations.len() as f64),
            js::format_integer(config.max_requests),
            js::format_integer((stations.len() as f64 * 1.2).ceil()),
        ));
        out.set_exit(crate::out::EXIT_FAILURE);
        return Ok(());
    }
    if config.dry_run {
        return Ok(());
    }

    let relayed = Relayed::new(cache.root(), config.eddn_max_age_minutes);
    let options = edm_core::cli::config::eddn_config(&app.cli, &app.session.credentials)
        .map_err(|error| error.message().to_owned())?;
    let options = edm_core::domain::eddn::EddnOptions { test: config.test, ..options };

    let stamp_overrides = app.stamp_overrides()?;
    let query = edm_core::cli::config::starsystem_query(
        &app.cli,
        edm_core::cli::config::CachedTimestamp::SweepZero,
    )
    .map_err(|error| error.message().to_owned())?;

    let entropy = crate::ports::PinnedJitter {
        inner: &app.ports.entropy,
        unit: app.overrides.jitter.unwrap_or(f64::NAN),
    };
    let pacer = Pacer::new(pacing(config), &app.ports.clock, timer, &entropy);
    let relay_tally = std::cell::RefCell::new(crate::route::relay::Tally::default());

    let total = stations.len();
    let report = |job: &crate::route::pool::Job,
                  outcome: &crate::route::pool::Outcome,
                  attempts: u32,
                  completed: usize| {
        let system = stations
            .iter()
            .find(|s| matches!(job, Job::Market { market_id, .. } if *market_id == s.market_id))
            .map_or("", |s| s.system_name.as_str());
        out.line(&edm_core::render::views::sweep_line(
            &edm_core::render::views::SweepLine {
                completed,
                total,
                station: job.label(),
                system,
                status: outcome.status,
                tradable: outcome.tradable,
                from_cache: false,
                attempts,
            },
        ));
    };
    let trace = |event: &edm_core::render::views::PaceEvent<'_>| {
        out.line(&edm_core::render::views::pace_line(event));
    };

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
    let eddn = acquire::Eddn {
        options: &options,
        url: &app.overrides.eddn_url,
        relayed: &relayed,
        stations: &stations,
        bucket: eddn_bucket,
        tokens: &eddn_tokens,
    };
    let cx = acquire::Cx {
        http: app.http,
        clock: &app.ports.clock,
        timer,
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
        eddn: Some(&eddn),
        workers: config.workers as usize,
        quiet: config.quiet,
        verify_systems: false,
        language: &query.language,
        report: (!config.quiet).then_some(&report as crate::route::pool::Report<'_>),
        trace: (config.verbose && !config.quiet)
            .then_some(&trace as crate::route::pool::Trace<'_>),
        total,
    };

    // Every market goes in `to_poll`: this command exists to read them live.
    let prepared = Prepared {
        cached: Vec::new(),
        to_poll: stations
            .iter()
            .map(|s| Job::Market {
                market_id: s.market_id,
                station: s.station_name.clone(),
                system: s.system_name.clone(),
            })
            .collect(),
        hits: crate::route::cache::Hits::default(),
    };
    let acquired = acquire::sweep(&cx, &pacer, prepared, &[]).await;

    out.emit(&summary(&acquired, &pacer.spent(), config.test));
    if !acquired.unreached.is_empty() || acquired.relayed.failed > 0 {
        out.set_exit(crate::out::EXIT_FAILURE);
    }
    Ok(())
}

/// What the run did, in the shape the route coverage block uses.
fn summary(
    acquired: &acquire::Acquired,
    spent: &crate::route::pacer::Spent,
    test: bool,
) -> Vec<edm_core::render::Block<'static>> {
    use edm_core::render::views::{EddnCoverage, RouteCoverage};

    let mut blocks = edm_core::render::views::coverage_titled("EDDN IMPORT", &RouteCoverage {
        ranked: false,
        eddn_refusal: acquired.relayed.first_refusal.clone(),
        markets_found: acquired.listings.len() + acquired.unreached.len() + acquired.tally.markets_absent,
        markets_polled: acquired.listings.len(),
        markets_priced: acquired.listings.len(),
        markets_failed: acquired.unreached.len(),
        markets_absent: acquired.tally.markets_absent,
        eddn: Some(EddnCoverage {
            sent: acquired.relayed.sent,
            failed: acquired.relayed.failed,
            recent: acquired.relayed.recent,
            cached: acquired.relayed.cached,
            unnamed: acquired.relayed.unnamed,
            abandoned: acquired.relayed.abandoned,
        }),
        requests_sent: spent.requests,
        throttled: spent.throttled,
        ..RouteCoverage::default()
    });
    if test {
        blocks.push(edm_core::render::Block::Note(
            "sent to the test schema: the gateway accepts these and does not relay them onward"
                .to_owned(),
        ));
    }
    blocks
}

/// Why a target contributed no markets.
///
/// **Three different facts, and they were one message.** A system Ardent has
/// never heard of is a typo in the list; a system it knows that has no markets
/// is a fact about the galaxy; a request that failed is neither. Reporting all
/// three as "Ardent does not know" told a user to go and fix a line that was
/// correct — `Col 285 Sector HB-V c3-0` is a real system, and Ardent answers
/// HTTP 200 with a full record for it.
#[derive(Clone, Debug)]
enum Skip {
    /// Ardent answered 404: no such system or market.
    Unknown,
    /// Ardent answered 200 with an empty list. Nothing to publish, and nothing
    /// wrong.
    NoMarkets,
    /// The request itself failed, and the message says how.
    Failed(String),
}

/// A target that yielded nothing, and why.
#[derive(Clone, Debug)]
struct Skipped {
    target: String,
    why: Skip,
}

/// Every station under every target, and the targets that yielded none.
///
/// A market id resolves to itself; a system name resolves to every market
/// Ardent lists in it — **all of them**, with no station filter. `route`'s
/// filter exists because a 1,232-tonne ship cannot berth at a settlement; this
/// command is not flying anywhere, and a settlement's prices are as worth
/// publishing as a Coriolis's.
///
/// The two empty cases are told apart by the status Ardent already sends:
/// an unknown system is a 404 and so arrives as an `Err`, while a known system
/// with nothing in it is a 200 carrying `[]`. No extra request is needed.
///
/// **Concurrent, cached and reported.** This was a serial `for` loop that
/// printed nothing: at ~330 ms a request, a 629-line list is three and a half
/// minutes of silence before the first market is even read — the same defect
/// `route`'s gather had, in a command written after it was fixed there. The
/// answers go through the atlas, so a second run over the same list resolves
/// from disk.
async fn resolve<H: HttpTransport, F: Fs>(
    ardent: &ArdentClient<'_, H>,
    atlas: &crate::route::atlas::Atlas,
    fs: &F,
    now_ms: f64,
    targets: &[Target],
    report: Option<&dyn Fn(usize, usize, usize)>,
) -> (Vec<ArdentStation>, Vec<Skipped>) {
    use futures_util::StreamExt as _;

    let total = targets.len();
    let done = std::cell::Cell::new(0usize);
    let found = std::cell::Cell::new(0usize);

    let answers = futures_util::stream::iter(targets.iter().map(|target| {
        let (done, found) = (&done, &found);
        async move {
            let answer = resolve_one(ardent, atlas, fs, now_ms, target).await;
            done.set(done.get() + 1);
            if let Ok(stations) = &answer {
                found.set(found.get() + stations.len());
            }
            if let Some(report) = report {
                report(done.get(), total, found.get());
            }
            answer
        }
    }))
    // `buffered`, not `buffer_unordered`: both run sixteen at once, but only
    // this one yields in input order, and the order of a hand-written list is
    // the order its writer meant.
    .buffered(ARDENT_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut stations: Vec<ArdentStation> = Vec::new();
    let mut skipped = Vec::new();
    for (target, answer) in targets.iter().zip(answers) {
        match answer {
            Ok(mut found) => stations.append(&mut found),
            Err(why) => skipped.push(Skipped { target: label(target), why }),
        }
    }

    // A market named twice — directly and through its system — is read once.
    stations.sort_by(|a, b| a.market_id.total_cmp(&b.market_id));
    stations.dedup_by(|a, b| a.market_id == b.market_id);
    (stations, skipped)
}

/// How a target is named back to the user: the line they wrote.
fn label(target: &Target) -> String {
    match target {
        Target::Market(market_id) => js::js_number(*market_id),
        Target::System(name) => name.clone(),
    }
}

/// One target's stations, or why it has none.
async fn resolve_one<H: HttpTransport, F: Fs>(
    ardent: &ArdentClient<'_, H>,
    atlas: &crate::route::atlas::Atlas,
    fs: &F,
    now_ms: f64,
    target: &Target,
) -> Result<Vec<ArdentStation>, Skip> {
    match target {
        Target::Market(market_id) => match ardent.station_by_market_id(*market_id).await {
            Some(station) => Ok(vec![ArdentStation {
                market_id: *market_id,
                station_name: station.station_name,
                system_name: station.system_name,
                station_type: station.station_type,
                max_landing_pad_size: None,
                distance_to_arrival: None,
                // Never read: nothing here filters by distance.
                coordinates: Coordinates { x: f64::NAN, y: f64::NAN, z: f64::NAN },
            }]),
            // `station_by_market_id` swallows every failure alike \[R81\], so
            // this is the one place the three cases cannot be told apart — and
            // the message says only what is certain.
            None => Err(Skip::Unknown),
        },
        Target::System(name) => {
            let reference = edm_core::ardent::ReferenceSystem {
                name: name.clone(),
                address: 0.0,
                coordinates: Coordinates { x: f64::NAN, y: f64::NAN, z: f64::NAN },
            };
            match ardent.system_markets_cached_status(atlas, fs, now_ms, &reference).await {
                Ok(found) if !found.is_empty() => Ok(found),
                Ok(_) => Err(Skip::NoMarkets),
                // The status itself, not a substring of the message: 404 is
                // Ardent saying it has no such system, and anything else is a
                // failure that must be reported as one.
                Err(refusal) if refusal.status == Some(404) => Err(Skip::Unknown),
                Err(refusal) => Err(Skip::Failed(refusal.message)),
            }
        }
    }
}

/// How many Ardent lookups run at once. See `route`'s constant of the same
/// name: sixteen concurrent requests were measured returning 200 in 0.6 s
/// against 330 ms each serially.
const ARDENT_CONCURRENCY: usize = 16;

fn pacing(config: &FeedConfig) -> Pacing {
    Pacing {
        bucket: Bucket {
            rate: config.rate_per_second,
            burst: 1.0,
            min_rate: js::js_min(config.rate_per_second, 0.5),
        },
        budget: Budget {
            per_job_ms: js::js_min(Budget::default().per_job_ms, config.deadline_seconds * 1000.0),
            run_deadline_ms: config.deadline_seconds * 1000.0,
            ..Budget::default()
        },
        ..Pacing::default()
    }
}
