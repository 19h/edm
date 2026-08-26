//! Reading a region's live prices, paced.
//!
//! One authenticated request per market, through the shared [`Pacer`] and the
//! two-stage [`pool`]. The cache is consulted first, and what it answers is
//! never sent — a market that was read four minutes ago is not read again.
//!
//! The output is deliberately a *pair*: what was read, and what was not. A
//! market that could not be reached must never be indistinguishable from one
//! that ranked badly, so [`Acquired::unreached`] is carried all the way to the
//! coverage table rather than being logged and dropped.

use std::cell::RefCell;

use edm_core::ardent::ArdentStation;
use edm_core::consts::{MARKET_LIST, STARSYSTEM};
use edm_core::domain::MarketSnapshot;
use edm_core::domain::starsystem::read_market_points;
use edm_core::js;
use edm_core::js::json::JsValue;

use crate::exchange::SendOptions;
use crate::game_api::{self, Credentials, HeaderConfig, Stamp};
use crate::net::HttpTransport;
use crate::out::Out;
use crate::ports::{Clock, Entropy, Fs, Timer};
use crate::route::cache::{Cache, Hits};
use crate::route::pacer::Pacer;
use crate::route::pool::{self, Abandoned, Job, Outcome, Pool};
use crate::route::relay::{self, Relayed};
use crate::sweep::next_stamp;

/// `commodities not currently available at this market`.
///
/// Measured on a live radius-100 sweep, 2026-08-05: 40 of 5,089 markets answer
/// this. The station is real and was reached; it simply has nothing to trade.
const MARKET_GONE: u16 = 410;

/// One market's live listing.
#[derive(Clone, Debug)]
pub struct Listing {
    pub market_id: f64,
    pub station_name: String,
    pub system_name: String,
    /// The decrypted payload, kept whole so a snapshot can borrow from it.
    pub document: JsValue,
    /// When these prices were read. A cached listing carries the instant of the
    /// *original* read, not of the run that reused it — a price is as old as
    /// the price, whatever fetched it.
    pub read_at_ms: f64,
    /// Timestamp attached to the underlying market observation, when the
    /// payload supplies one. This is distinct from retrieval/cache time.
    pub observed_at_ms: Option<f64>,
    pub from_cache: bool,
}

impl Listing {
    /// The parsed listing, or `None` if the payload was not one.
    #[must_use]
    pub fn snapshot(&self) -> Option<MarketSnapshot<'_>> {
        edm_core::domain::parse_market_snapshot(&self.document)
    }
}

/// Everything the EDDN relay needs.
pub struct Eddn<'a> {
    pub options: &'a edm_core::domain::eddn::EddnOptions,
    pub url: &'a str,
    /// EDDN's own token bucket, **separate from the game-internal API's**.
    ///
    /// Relays ride inside the market poll, so before this the `--rps` meant for
    /// Frontier set the rate at which a shared community service was written
    /// to as well: a 565-market import at forty a second earned this host a
    /// `403 Forbidden` from the gateway's proxy.
    pub bucket: edm_core::pace::Bucket,
    pub tokens: &'a std::cell::RefCell<edm_core::pace::BucketState>,
    /// What this machine has already relayed, and when.
    pub relayed: &'a Relayed,
    /// The stations Ardent named, for the system and station names EDDN's
    /// schema requires and the market payload does not carry.
    pub stations: &'a [ArdentStation],
}

impl std::fmt::Debug for Eddn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Eddn")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// What a sweep reached, and what it did not.
#[derive(Debug, Default)]
pub struct Acquired {
    pub listings: Vec<Listing>,
    /// Markets in radius that were never read. Named in the coverage table.
    pub unreached: Vec<Abandoned>,
    pub cache: Hits,
    pub tally: pool::Tally,
    pub relayed: relay::Tally,
}

/// Everything the sweep needs that it cannot decide for itself.
pub struct Cx<'a, H, C, E, F, T> {
    pub http: &'a H,
    pub clock: &'a C,
    /// Waiting, for EDDN's own pacing. The game-internal API's waiting is the
    /// pool's.
    pub timer: &'a T,
    pub entropy: &'a E,
    pub fs: &'a F,
    pub out: &'a Out,
    /// `EDM_ORIGIN_OVERRIDE`, or the game-internal API's own origin.
    ///
    /// An argument rather than a constant, because a sweep that read the
    /// constant would go to the live API while the harness believed it was
    /// talking to a mock — a mistake this codebase has already made once.
    pub origin: &'a str,
    pub credentials: &'a Credentials,
    pub headers: &'a HeaderConfig,
    pub method_override: Option<&'a str>,
    pub nonce_override: Option<edm_core::wire::Nonce>,
    pub frontier_time_override: Option<f64>,
    pub request_time_override: Option<u32>,
    pub cache: &'a Cache,
    /// Counters the relay writes as it goes.
    pub relayed: &'a RefCell<relay::Tally>,
    pub workers: usize,
    pub quiet: bool,
    /// One line per market as it lands. `None` under `--json`.
    pub report: Option<pool::Report<'a>>,
    /// `--verbose`: the pacing decisions behind them.
    pub trace: Option<pool::Trace<'a>>,
    /// How many jobs the report's `k/N` counts up to.
    pub total: usize,
    /// `--verify-systems`: read each system's `starsystem` payload and take
    /// *its* market list, instead of Ardent's.
    ///
    /// Off by default. Step 0 established that the game-internal API answers for a
    /// market the commander is not docked at, which is what makes Ardent's
    /// market ids usable directly — and a starsystem payload costs about
    /// twenty-five market reads. What the read buys is a market Ardent has
    /// never seen; the coverage block says so either way.
    pub verify_systems: bool,
    /// The language the `starsystem` read asks for, when it happens at all.
    pub language: &'a str,
    /// Set by `--eddn` / `--eddn-test`: relay every market polled live.
    pub eddn: Option<&'a Eddn<'a>>,
}

impl<H, C, E, F, T> std::fmt::Debug for Cx<'_, H, C, E, F, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cx")
            .field("origin", &self.origin)
            .field("workers", &self.workers)
            .finish_non_exhaustive()
    }
}

/// What the cache already answers, and what is left to buy.
///
/// Split out from [`sweep`] and run **before the spend gate**, because its
/// result changes what the sweep costs: a run that finds everything cached
/// sends no requests at all, and a plan that priced twenty-two requests and
/// then sent none is a plan nobody can check. It costs nothing but a few file
/// reads, so there is no reason to guess instead.
#[derive(Debug, Default)]
pub struct Prepared {
    /// Already known, and still fresh.
    pub cached: Vec<Listing>,
    /// One job per market that has to be read.
    pub to_poll: Vec<Job>,
    pub hits: Hits,
}

/// Consult the cache for every selected market.
pub fn prepare<F: Fs>(cache: &Cache, fs: &F, markets: &[ArdentStation], now_ms: f64) -> Prepared {
    let mut prepared = Prepared {
        cached: Vec::with_capacity(markets.len()),
        ..Prepared::default()
    };
    for market in markets {
        let lookup = cache.get(fs, market.market_id, now_ms);
        match lookup {
            crate::route::cache::Lookup::Fresh(entry)
                if edm_core::domain::parse_market_snapshot(&entry.payload).is_some() =>
            {
                prepared.hits.fresh += 1;
                prepared.cached.push(Listing {
                    market_id: market.market_id,
                    station_name: market.station_name.clone(),
                    system_name: market.system_name.clone(),
                    observed_at_ms: market_observed_at_ms(&entry.payload),
                    document: entry.payload,
                    read_at_ms: entry.read_at_ms,
                    from_cache: true,
                });
            }
            crate::route::cache::Lookup::Fresh(_) => {
                // A structurally valid envelope with an unusable payload is a
                // corrupt cache entry, not a hit. Counting it as fresh made
                // source notes claim more cached prices than were ranked.
                prepared.hits.corrupt += 1;
                prepared.to_poll.push(Job::Market {
                    market_id: market.market_id,
                    station: market.station_name.clone(),
                    system: market.system_name.clone(),
                });
            }
            other => {
                other.tally(&mut prepared.hits);
                prepared.to_poll.push(Job::Market {
                    market_id: market.market_id,
                    station: market.station_name.clone(),
                    system: market.system_name.clone(),
                });
            }
        }
    }
    prepared
}

/// Read every market the cache could not answer for.
///
/// Under `--verify-systems` the pool is seeded with **system** jobs instead,
/// and each one's markets are queued the instant that system's payload lands —
/// which is what the two-stage pool is for. Stage two is not a barrier behind
/// stage one, so the pool stays saturated rather than idling while the last few
/// systems trickle in.
pub async fn sweep<H, C, E, F, T>(
    cx: &Cx<'_, H, C, E, F, T>,
    pacer: &Pacer<'_, C, T, E>,
    prepared: Prepared,
    systems: &[(String, f64)],
) -> Acquired
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
    T: Timer,
{
    let Prepared {
        cached: mut listings,
        mut to_poll,
        hits,
    } = prepared;

    // A cached listing is never relayed — it was read at some earlier instant,
    // and republishing it would stamp that old reading with the current time.
    // Counted so the coverage line's numbers add up to the markets it saw.
    if cx.eddn.is_some() {
        cx.relayed.borrow_mut().cached += listings.len();
    }

    // The cached ones first, and *said* first: a run that resolves entirely
    // from disk should still show its work, or it looks like it did nothing.
    if let Some(report) = cx.report {
        for (index, listing) in listings.iter().enumerate() {
            report(
                &Job::Market {
                    market_id: listing.market_id,
                    station: listing.station_name.clone(),
                    system: listing.system_name.clone(),
                },
                &Outcome {
                    ok: true,
                    tradable: Some(tradable_rows(&listing.document)),
                    ..Outcome::default()
                },
                0,
                index + 1,
            );
        }
    }

    if cx.verify_systems {
        // Discovery comes from the game-internal API itself, so nothing is known
        // about which markets exist until the system reads land — and the cache
        // cannot be consulted for markets nobody has named yet.
        // Discovery comes from the game-internal API itself here, so the cache's
        // per-market answers cannot be used: nothing is known about which
        // markets exist until the system reads land.
        to_poll.clear();
        listings.clear();
        for (name, address) in systems {
            to_poll.push(Job::System {
                name: name.clone(),
                address: *address,
            });
        }
        let fresh = RefCell::new(Vec::<Listing>::new());
        let pool = Pool {
            pacer,
            out: cx.out,
            workers: cx.workers,
            quiet: cx.quiet,
            report: cx.report,
            trace: cx.trace,
        };
        let (tally, unreached) = pool::run(&pool, to_poll, |job| {
            let fresh = &fresh;
            async move {
                match job {
                    Job::System { name, address } => read_system(cx, &name, address).await,
                    Job::Market {
                        market_id,
                        station,
                        system,
                    } => poll(cx, market_id, &station, &system, fresh).await,
                }
            }
        })
        .await;
        let mut found = fresh.into_inner();
        found.sort_by(|a, b| a.market_id.total_cmp(&b.market_id));
        return Acquired {
            listings: found,
            unreached,
            cache: hits,
            tally,
            relayed: cx.relayed.borrow().clone(),
        };
    }

    let fresh = RefCell::new(Vec::<Listing>::new());
    let pool = Pool {
        pacer,
        out: cx.out,
        workers: cx.workers,
        quiet: cx.quiet,
        report: cx.report,
        trace: cx.trace,
    };
    let (tally, unreached) = pool::run(&pool, to_poll, |job| {
        let fresh = &fresh;
        async move {
            let Job::Market {
                market_id,
                station,
                system,
            } = job
            else {
                // The sweep seeds market jobs only; a system job here would be
                // a programming error, not a runtime condition.
                return Outcome::default();
            };
            poll(cx, market_id, &station, &system, fresh).await
        }
    })
    .await;

    listings.append(&mut fresh.into_inner());
    // Deterministic order regardless of which worker finished first, so two
    // runs over the same region rank identically.
    listings.sort_by(|a, b| a.market_id.total_cmp(&b.market_id));

    Acquired {
        listings,
        unreached,
        cache: hits,
        tally,
        relayed: cx.relayed.borrow().clone(),
    }
}

/// One `starsystem` read, whose market list becomes the pool's next stage.
async fn read_system<H, C, E, F, T>(cx: &Cx<'_, H, C, E, F, T>, name: &str, address: f64) -> Outcome
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
    T: Timer,
{
    let stamp: Stamp = next_stamp(
        cx.clock,
        cx.entropy,
        cx.nonce_override,
        cx.frontier_time_override,
        cx.request_time_override,
    );
    let request = game_api::prepare(
        cx.origin,
        STARSYSTEM,
        cx.method_override,
        game_api::starsystem_fields(
            address,
            cx.language,
            0.0,
            cx.credentials,
            stamp.frontier_time,
        ),
        stamp,
        cx.headers,
    );
    let exchange = crate::exchange::send(
        cx.http,
        cx.out,
        &request,
        false,
        SendOptions {
            quiet: true,
            ignore_dry_run: false,
            quiet_failure: false,
        },
        |_| {},
        |exchange| crate::cmd::emit_response(cx.out, exchange),
    )
    .await;

    let Some(exchange) = exchange else {
        return Outcome {
            status: None,
            ok: false,
            ..Outcome::default()
        };
    };
    let retry_after = exchange.headers.get("retry-after");
    if !(200..300).contains(&exchange.status) {
        return Outcome {
            status: Some(exchange.status),
            retry_after,
            ok: false,
            ..Outcome::default()
        };
    }
    let document = exchange
        .decrypted
        .as_deref()
        .and_then(|text| JsValue::parse(text).ok());
    let Some(JsValue::Obj(payload)) = document else {
        return Outcome {
            status: Some(exchange.status),
            retry_after,
            ok: false,
            ..Outcome::default()
        };
    };

    let follow_on = read_market_points(&payload)
        .into_iter()
        .map(|point| Job::Market {
            market_id: point.market_id,
            station: point.name.into_owned(),
            system: name.to_owned(),
        })
        .collect();
    Outcome {
        status: Some(exchange.status),
        retry_after,
        ok: true,
        absent: false,
        tradable: None,
        follow_on,
    }
}

/// One market poll.
async fn poll<H, C, E, F, T>(
    cx: &Cx<'_, H, C, E, F, T>,
    market_id: f64,
    station: &str,
    system: &str,
    fresh: &RefCell<Vec<Listing>>,
) -> Outcome
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
    T: Timer,
{
    let stamp: Stamp = next_stamp(
        cx.clock,
        cx.entropy,
        cx.nonce_override,
        cx.frontier_time_override,
        cx.request_time_override,
    );
    let request = game_api::prepare(
        cx.origin,
        MARKET_LIST,
        cx.method_override,
        game_api::list_fields(
            &js::js_number(market_id),
            cx.credentials,
            stamp.frontier_time,
        ),
        stamp,
        cx.headers,
    );

    // Quiet, but not silent: `send` still prints the RESPONSE table when a
    // quiet poll fails, because the headers are where the diagnosis is \[R74\].
    // Passing a no-op for both would drop that block for every 429 in a sweep,
    // which is precisely the failure a wide run needs to see.
    let exchange = crate::exchange::send(
        cx.http,
        cx.out,
        &request,
        false,
        SendOptions {
            quiet: true,
            ignore_dry_run: false,
            quiet_failure: false,
        },
        |_| {},
        |exchange| crate::cmd::emit_response(cx.out, exchange),
    )
    .await;

    let Some(exchange) = exchange else {
        // No status at all: a transport failure or a timeout, which
        // `is_transient_status` treats as worth retrying.
        return Outcome {
            status: None,
            ok: false,
            ..Outcome::default()
        };
    };

    let retry_after = exchange.headers.get("retry-after");
    let document = exchange
        .decrypted
        .as_deref()
        .and_then(|text| JsValue::parse(text).ok())
        .filter(|doc| edm_core::domain::parse_market_snapshot(doc).is_some());

    // 410 is the game-internal API saying, correctly and permanently, that this
    // station has no commodity market. Retrying it repeats a question that has
    // been answered; counting it as unreached tells the user a market was
    // missed when it was not.
    if exchange.status == MARKET_GONE {
        return Outcome {
            status: Some(exchange.status),
            retry_after,
            absent: true,
            ..Outcome::default()
        };
    }
    if !(200..300).contains(&exchange.status) {
        return Outcome {
            status: Some(exchange.status),
            retry_after,
            ok: false,
            ..Outcome::default()
        };
    }

    let Some(document) = document else {
        // A 200 that does not decrypt to a market listing is not a success.
        // Treating it as one would put an empty market in the graph and rank a
        // route through it as unprofitable rather than as unread.
        return Outcome {
            status: Some(exchange.status),
            retry_after,
            ok: false,
            ..Outcome::default()
        };
    };
    let tradable = Some(tradable_rows(&document));

    let now = cx.clock.now_ms();
    cx.cache.put(cx.fs, market_id, &document, now);
    if let Some(eddn) = cx.eddn {
        publish(cx, eddn, market_id, &document, now).await;
    }
    fresh.borrow_mut().push(Listing {
        market_id,
        station_name: station.to_owned(),
        system_name: system.to_owned(),
        observed_at_ms: market_observed_at_ms(&document),
        document,
        read_at_ms: now,
        from_cache: false,
    });

    Outcome {
        status: Some(exchange.status),
        retry_after,
        ok: true,
        absent: false,
        tradable,
        follow_on: Vec::new(),
    }
}

/// Relay one freshly-read listing to EDDN.
///
/// Only ever called from the live-poll path. A listing served from the price
/// cache is **never** relayed: it was read at some earlier instant, and
/// republishing it would stamp that old reading with the current time, which is
/// worse than the duplicate the suppression window exists to prevent.
async fn publish<H, C, E, F, T>(
    cx: &Cx<'_, H, C, E, F, T>,
    eddn: &Eddn<'_>,
    market_id: f64,
    document: &JsValue,
    now_ms: f64,
) where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
    T: Timer,
{
    // Every decision that reads or writes the tally happens inside this block,
    // so no borrow can span the awaits below. `drop()` before an await is not
    // enough to say that clearly — and with sixteen workers sharing one cell,
    // a borrow that outlived its scope would not be a lint, it would be a
    // panic mid-sweep.
    let message = {
        let mut tally = cx.relayed.borrow_mut();

        // Stop sending once the gateway has refused enough times in a row that
        // the refusals are clearly about us rather than about one message.
        if tally.failed >= relay::GIVE_UP_AFTER && tally.sent == 0 {
            tally.abandoned += 1;
            return;
        }

        if !eddn.relayed.may_relay(cx.fs, market_id, now_ms) {
            tally.recent += 1;
            return;
        }
        // EDDN's schema requires a system and a station name, and the market
        // payload carries neither — they come from the Ardent row that put this
        // market in the sweep. Without one there is nothing well-formed to send.
        let Some(station) = eddn.stations.iter().find(|s| s.market_id == market_id) else {
            tally.unnamed += 1;
            return;
        };
        let Some(snapshot) = edm_core::domain::parse_market_snapshot(document) else {
            tally.unnamed += 1;
            return;
        };

        let target = edm_core::domain::eddn::EddnStation {
            system_name: station.system_name.clone(),
            station_name: station.station_name.clone(),
            station_type: station.station_type.clone(),
            economies: None,
        };
        // The instant of *publication*, as the ported sweep does it.
        let timestamp = edm_core::js::time::iso8601_from_ms(now_ms).unwrap_or_default();
        edm_core::domain::eddn::build_message(
            &target,
            market_id,
            &snapshot.commodities,
            &timestamp,
            eddn.options,
        )
    };

    // Wait for EDDN's own bucket, reserved then released before the await —
    // the same discipline `Pacer::acquire` uses and for the same reason.
    let at_ms = {
        let mut tokens = eddn.tokens.borrow_mut();
        eddn.bucket.reserve(&mut tokens, cx.clock.now_ms())
    };
    let wait = edm_core::js::js_max(at_ms - cx.clock.now_ms(), 0.0);
    if wait > 0.0 {
        cx.timer.sleep_ms(wait).await;
    }

    let body = message.payload.stringify_compact();
    let result = crate::eddn::submit(cx.http, eddn.url, body.as_bytes(), message.count).await;

    let mut tally = cx.relayed.borrow_mut();
    if result.ok {
        tally.sent += 1;
        // Recorded only on success, so a rejected message is retried next run
        // rather than suppressed for half an hour.
        eddn.relayed.record(cx.fs, market_id, now_ms);
    } else {
        tally.failed += 1;
        if tally.first_refusal.is_none() {
            tally.first_refusal = Some(result.detail.clone());
        }
    }
}

/// The market simulation/update timestamp carried by a listing.
///
/// Cache retrieval never rewrites it: a cached payload remains as old as the
/// observation it contains. Unknown, non-finite and negative timestamps stay
/// explicit rather than being relabelled "now".
fn market_observed_at_ms(document: &JsValue) -> Option<f64> {
    let root = document.as_record()?;
    let modified = root.get("lastModified")?.as_record()?;
    let seconds = modified.get("sec")?.as_f64()?;
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds * 1_000.0)
}

/// Rows this market will actually sell or buy.
///
/// Every game-internal API market returns the same 391-entry commodity map — most
/// of it priced but with zero stock and zero demand — so a commodity count is
/// the same number everywhere and says nothing about whether a station is worth
/// visiting. Measured across four real markets, 2026-08-05.
fn tradable_rows(document: &JsValue) -> usize {
    edm_core::domain::parse_market_snapshot(document).map_or(0, |snapshot| {
        snapshot
            .commodities
            .iter()
            .filter(|row| {
                (row.stock > 0.0 && row.buy_price > 0.0)
                    || (row.demand > 0.0 && row.sell_price > 0.0)
            })
            .count()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::RecordingFs;
    use edm_core::domain::id64::Coordinates;
    use std::path::PathBuf;

    #[test]
    fn an_unusable_cached_payload_is_repolled_not_counted_fresh() {
        let fs = RecordingFs::default();
        let cache = Cache::new(PathBuf::from("/cache"), 30.0, true, false);
        let invalid = JsValue::Obj(edm_core::js::json::JsObject::from_document_order(Vec::new()));
        cache.put(&fs, 42.0, &invalid, 1_000.0);
        let station = ArdentStation {
            market_id: 42.0,
            station_name: "Galileo".to_owned(),
            system_name: "Sol".to_owned(),
            system_address: 10_477_373_803.0,
            station_type: Some("Orbis".to_owned()),
            max_landing_pad_size: Some(3.0),
            distance_to_arrival: Some(500.0),
            coordinates: Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        };

        let prepared = prepare(&cache, &fs, &[station], 2_000.0);
        assert!(prepared.cached.is_empty());
        assert_eq!(prepared.to_poll.len(), 1);
        assert_eq!(prepared.hits.fresh, 0);
        assert_eq!(prepared.hits.corrupt, 1);
    }
}
