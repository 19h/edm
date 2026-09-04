//! Re-pricing one pinned route \[C53\].
//!
//! The question a pin asks is narrow: does the route I chose still trade, at
//! what, and can I still dock. So a re-price reads exactly that route's
//! markets, live, rebuilds the route as a skeleton over what it read, and
//! hands the skeleton to `rescore`, which prices it with its commodity held
//! fixed or drops it. Nothing here can answer with a different route.

use std::collections::HashMap;

use edm_core::ardent::{self, ArdentStation};
use edm_core::cli::access::Cli;
use edm_core::domain::commander::{CarrierDoor, CommanderState};
use edm_core::domain::id64::Coordinates;
use edm_route::pin::PinKey;

use crate::cmd::App;
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs};
use crate::route::{access, acquire, ingest};
use crate::route::cache::Cache;

use super::cards::{MarketCard, RouteCard};
use super::{Event, Session};

/// Everything a re-price needs, owned.
#[derive(Clone, Debug)]
pub(crate) struct PinJob {
    pub key: PinKey,
    /// What the status bar calls it.
    pub label: String,
    /// The search that found the route: its flags decide the cache, the
    /// travel model, the ship and the carrier policy.
    pub argv: Vec<String>,
    /// The route's markets, in flying order.
    pub stations: Vec<ArdentStation>,
    /// The latest journal, for the door overlay and the approach distance.
    pub commander: Option<Box<CommanderState>>,
}

/// What a re-price found.
#[derive(Clone, Debug)]
pub(crate) struct PinState {
    pub refreshed_at_ms: f64,
    /// The route as priced now; `None` when a leg no longer trades.
    pub route: Option<RouteCard>,
    pub unpriced_reason: Option<String>,
    pub markets: Vec<MarketCard>,
    pub requests: usize,
}

/// Re-price `job`, reporting [`Event::Repriced`].
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence: read the markets, check the doors, rebuild the route, describe each market"
)]
pub(crate) async fn reprice<H, C, E, F>(
    session: &Session<'_, H, C, E, F>,
    job: PinJob,
) -> Result<bool, String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
{
    let parsed = super::search::parse_argv(&job.argv)?;
    let cli = Cli::new(&parsed, session.env);
    let mut config = edm_core::cli::config::route_config_with_reference(&cli, Some("unused"))
        .map_err(|error| error.message().to_owned())?;
    if let Some(state) = job.commander.as_deref() {
        crate::cmd::apply_commander_defaults(&cli, state, &mut config);
    }
    let app = App::open(cli, session.http, session.ports, session.out, session.overrides)?;
    let out = app.out;
    let before = session.pacer.spent();
    session.pacer.begin_round();

    // Every market of the route, live: the write-side cache is refresh-mode,
    // so the entry the previous round wrote cannot answer for this one.
    let read_cache = crate::cmd::route::cache_for(&app, &config);
    let write_cache = Cache::new(
        read_cache.root().to_path_buf(),
        config.max_age_minutes,
        config.cache,
        true,
    );
    let stamp_overrides = app.stamp_overrides()?;
    let query = edm_core::cli::config::starsystem_query(
        &app.cli,
        edm_core::cli::config::CachedTimestamp::SweepZero,
    )
    .map_err(|error| error.message().to_owned())?;
    let relay_tally = std::cell::RefCell::new(crate::route::relay::Tally::default());
    let cx = acquire::Cx {
        http: app.http,
        clock: &app.ports.clock,
        timer: session.timer,
        entropy: session.entropy,
        fs: &app.ports.fs,
        out,
        origin: &app.overrides.origin,
        credentials: &app.credentials,
        headers: &app.headers,
        method_override: app.session.method_override.as_deref(),
        nonce_override: stamp_overrides.nonce,
        frontier_time_override: stamp_overrides.frontier_time,
        request_time_override: stamp_overrides.request_time,
        cache: &write_cache,
        relayed: &relay_tally,
        eddn: None,
        workers: config.workers as usize,
        quiet: true,
        verify_systems: false,
        language: &query.language,
        report: None,
        trace: None,
        total: job.stations.len(),
    };
    let now_ms = app.ports.clock.now_ms();
    let prepared = acquire::prepare(&write_cache, &app.ports.fs, &job.stations, now_ms);
    let acquired = acquire::sweep(&cx, session.pacer, prepared, &[]).await;

    // Docking access for any carrier on the route, from the cache when it is
    // fresh and live otherwise, then the journal's own word overlaid.
    let carrier_ids: Vec<f64> = job
        .stations
        .iter()
        .filter(|station| ardent::is_carrier(station.station_type.as_deref()))
        .map(|station| station.market_id)
        .collect();
    let notoriety = job.commander.as_deref().map_or(0.0, |state| state.notoriety);
    let mut verdicts = access::AccessIndex::default();
    if !carrier_ids.is_empty() {
        let policy = access::CachePolicy {
            enabled: config.cache,
            refresh: config.refresh,
            max_age_minutes: Some(config.max_age_minutes),
        };
        let mut prepared = access::prepare(
            &app.ports.fs,
            read_cache.root(),
            &carrier_ids,
            now_ms,
            policy,
            notoriety,
        );
        if !prepared.cold.is_empty() {
            let probe_cx = access::ProbeCx {
                http: app.http,
                out,
                origin: &app.overrides.origin,
                clock: &app.ports.clock,
                entropy: &app.ports.entropy,
                credentials: &app.credentials,
                headers: &app.headers,
                language: &query.language,
                method_override: app.session.method_override.as_deref(),
                dry_run: false,
                nonce_override: stamp_overrides.nonce,
                frontier_time_override: stamp_overrides.frontier_time,
                request_time_override: stamp_overrides.request_time,
            };
            let cold = std::mem::take(&mut prepared.cold);
            access::probe(
                &probe_cx,
                session.pacer,
                &app.ports.fs,
                read_cache.root(),
                &cold,
                now_ms,
                policy,
                notoriety,
                &mut prepared.index,
                &mut prepared.cost,
                None,
            )
            .await?;
        }
        access::finish(
            &mut prepared.index,
            &carrier_ids,
            job.commander.as_deref(),
            &mut prepared.cost,
        );
        verdicts = prepared.index;
    }

    // The route, over what was just read.
    let floors = ingest::floors(&config);
    let (markets, commodities, _) =
        ingest::markets_from_listings(&acquired.listings, &job.stations, &floors, &HashMap::new());
    let time = crate::cmd::route::time_model(&config);
    let ship = crate::cmd::route::ship(&config);
    let limits = edm_route::model::Limits {
        top_n: 1,
        ..crate::cmd::route::limits(&config)
    };
    let origin: Option<Coordinates> = job
        .commander
        .as_deref()
        .and_then(|state| state.current_system.as_ref())
        .and_then(|seen| seen.value.coordinates)
        .map(|xyz| Coordinates {
            x: xyz[0],
            y: xyz[1],
            z: xyz[2],
        });
    let (route, unpriced_reason) = match job.key.skeleton(&markets, &commodities, time) {
        Some(skeleton) => {
            let priced = edm_route::rescore::rescore(&markets, time, &ship, &limits, vec![skeleton]);
            match priced.into_iter().next() {
                Some(route) => (
                    Some(RouteCard::of(
                        &route,
                        &markets,
                        &commodities,
                        origin,
                        config.cargo.map(|tons| tons as i64),
                    )),
                    None,
                ),
                None => (
                    None,
                    Some("a leg no longer trades: the seller is out of stock, the buyer's order is filled, or the spread has gone".to_owned()),
                ),
            }
        }
        None => (
            None,
            Some(if markets.len() < job.stations.len() {
                "a market on the route did not answer with a usable listing".to_owned()
            } else {
                "a commodity on the route is no longer listed at its markets".to_owned()
            }),
        ),
    };

    let markets_cards = job
        .stations
        .iter()
        .map(|station| {
            let listing = acquired
                .listings
                .iter()
                .find(|listing| listing.market_id == station.market_id);
            let unreached = acquired
                .unreached
                .iter()
                .find(|gone| matches!(&gone.job, crate::route::pool::Job::Market { market_id, .. } if *market_id == station.market_id))
                .map(|gone| format!("{:?}", gone.reason));
            let mut card = MarketCard::of(station, listing, unreached.as_deref(), &job.key.commodities);
            if ardent::is_carrier(station.station_type.as_deref()) {
                card.access = Some(match verdicts.get(station.market_id) {
                    edm_core::carrier::Access::Open => "open to you".to_owned(),
                    edm_core::carrier::Access::Restricted => "restricted: you cannot dock".to_owned(),
                    edm_core::carrier::Access::Unknown => "unknown: no verdict".to_owned(),
                });
                card.door = job
                    .commander
                    .as_deref()
                    .and_then(|state| {
                        state
                            .carrier_doors
                            .iter()
                            .find(|(id, _)| *id == station.market_id as u64)
                    })
                    .map(|(_, seen)| {
                        format!(
                            "{}{}",
                            match seen.door {
                                CarrierDoor::Admitted => "your ship was admitted",
                                CarrierDoor::Refused => "your ship was refused",
                            },
                            seen.observed_at
                                .as_deref()
                                .map_or_else(String::new, |at| format!(" ({at})")),
                        )
                    });
            }
            card
        })
        .collect();

    let spent = session.pacer.spent();
    session
        .send(Event::Repriced {
            key: job.key,
            state: Box::new(PinState {
                refreshed_at_ms: app.ports.clock.now_ms(),
                route,
                unpriced_reason,
                markets: markets_cards,
                requests: spent.requests - before.requests,
            }),
        })
        .await;
    Ok(true)
}
