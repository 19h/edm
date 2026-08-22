//! `market` / `list` — one market by id, or a whole system swept.
//!
//! The two paths look similar and diverge in one place that matters: on the
//! single-market path `--json` is passed as the *quiet* argument (ts:1579), so
//! the request and response tables really are suppressed and the JSON document
//! is clean \[R77\]. On the sweep path the same flag only silences the progress
//! lines, and the sweep additionally returns before its failure tally, so a
//! `--json` sweep does not set exit 1 for markets that produced nothing
//! \[R78\].

use edm_core::ardent::Lookup;
use edm_core::cli::Flag;
use edm_core::cli::config::{self, LookupMode, MarketTarget};
use edm_core::consts::STARSYSTEM;
use edm_core::domain::eddn::{EddnOptions, EddnStation, build_message};
use edm_core::domain::starsystem::{MarketPoint, read_market_points};
use edm_core::domain::{MarketSnapshot, parse_market_snapshot};
use edm_core::js::json::JsValue;
use edm_core::js::{self, time};
use edm_core::render::{Block, views};

use crate::ardent::{ArdentClient, ResolvedSystem};
use crate::eddn::EddnResult;
use crate::exchange::SendOptions;
use crate::game_api;
use crate::net::HttpTransport;
use crate::out::EXIT_FAILURE;
use crate::ports::{Clock, Entropy, Fs};
use crate::sweep::{self, Cx, EddnPublish};

use super::{
    App, CmdResult, decrypted, field, message, num_or_null, object, str_value, timer_duration,
};

/// `runMarket` (ts:1685).
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
) -> CmdResult {
    match config::market_target(&app.cli).map_err(|error| error.message().to_owned())? {
        MarketTarget::Single(market_id) => single(app, market_id).await,
        MarketTarget::Sweep(name) => system(app, &name).await,
    }
}

// ---------------------------------------------------------------------------
// One market
// ---------------------------------------------------------------------------

/// What one poll produced. `MarketVisit` (ts:1347) for the single-market path,
/// which owns its document so the snapshot can borrow from it.
struct Visit {
    status: Option<u16>,
    /// `Some` exactly when the body parsed *and* was a market listing.
    document: Option<JsValue>,
    eddn: Option<EddnResult>,
}

/// `runMarketSingle` (ts:1557).
async fn single<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    market_id: f64,
) -> CmdResult {
    let eddn = load_eddn(app)?;
    let station = match eddn {
        None => None,
        Some(_) => Some(resolve_station(app, market_id).await?),
    };

    // The visit's `name` (ts:1579) is never read back on this path — the JSON
    // document does not carry it and the summary title is built from `station`
    // — so it is not computed.
    //
    // R77: `--json` reaches `visitMarket` as `quiet`, so both tables really are
    // suppressed here. That is not the bug R76 describes.
    let visit = poll(
        app,
        market_id,
        station.as_ref(),
        eddn.as_ref(),
        app.session.json,
    )
    .await?;

    if app.session.json {
        // ts:1582
        app.out.line(
            &object([
                ("marketId".to_owned(), JsValue::Num(market_id)),
                (
                    "station".to_owned(),
                    station.as_ref().map_or(JsValue::Null, station_json),
                ),
                (
                    "status".to_owned(),
                    num_or_null(visit.status.map(f64::from)),
                ),
                (
                    "eddn".to_owned(),
                    visit.eddn.as_ref().map_or(JsValue::Null, eddn_json),
                ),
                (
                    "payload".to_owned(),
                    visit
                        .document
                        .as_ref()
                        .and_then(parse_market_snapshot)
                        .map_or(JsValue::Null, |snapshot| {
                            JsValue::Obj(snapshot.payload.clone())
                        }),
                ),
            ])
            .stringify(2),
        );
        return Ok(());
    }

    // ts:1591 — nothing usable came back, and `send` has already reported why.
    let Some(document) = visit.document.as_ref() else {
        return Ok(());
    };
    let Some(snapshot) = parse_market_snapshot(document) else {
        return Ok(());
    };

    let title = format!(
        // ts:1594
        "MARKET SUMMARY  {}",
        station.as_ref().map_or_else(
            || format!("market {}", js::js_number(market_id)),
            |station| format!("{}, {}", station.station_name, station.system_name),
        )
    );
    app.out.emit(&views::market_snapshot(&snapshot, &title));

    if let Some(result) = &visit.eddn {
        // ts:1598
        app.out.emit(&[Block::Note(format!(
            "EDDN: {} ({} commodities{})",
            result.detail,
            js::format_integer(result.commodities as f64),
            if eddn.as_ref().is_some_and(|options| options.test) {
                ", test schema"
            } else {
                ""
            },
        ))]);
    }
    Ok(())
}

/// The two names EDDN insists on, from the flags or from Ardent (ts:1561).
async fn resolve_station<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    market_id: f64,
) -> Result<EddnStation, String> {
    let system_name = app.cli.optional_value(Flag::System, None);
    let station_name = app.cli.optional_value(Flag::Station, None);
    if let (Some(system_name), Some(station_name)) = (system_name, station_name) {
        return Ok(EddnStation {
            system_name: system_name.to_owned(),
            station_name: station_name.to_owned(),
            station_type: app
                .cli
                .optional_value(Flag::StationType, None)
                .map(str::to_owned),
            economies: None,
        });
    }

    if !app.session.json {
        app.note(format!(
            // ts:1568
            "resolving market {} through Ardent for the names EDDN requires...",
            js::js_number(market_id)
        ));
    }
    ArdentClient::new(app.http, &app.overrides.ardent_base)
        .station_by_market_id(market_id)
        .await
        .ok_or_else(|| {
            // ts:1573 — one string, so the sentence break carries a single space.
            format!(
                "EDDN needs a system and station name, and Ardent does not know market {}. \
                 Pass --system and --station, or sweep the whole system instead.",
                js::js_number(market_id)
            )
        })
}

/// `visitMarket` (ts:1390).
///
/// The sweep pool has its own copy in [`crate::sweep::visit_market`], which is
/// hard-wired to `quiet`. This one is not: on the single-market path the two
/// tables and the opaque-payload fallback all depend on `quiet` being
/// `session.json`.
async fn poll<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    market_id: f64,
    station: Option<&EddnStation>,
    eddn: Option<&EddnOptions>,
    quiet: bool,
) -> Result<Visit, String> {
    let exchange = app
        .fetch_market(
            &js::js_number(market_id),
            SendOptions {
                quiet,
                ignore_dry_run: false,
            },
        )
        .await?;
    let status = exchange.as_ref().map(|exchange| exchange.status);

    let mut document = None;
    if let Some(text) = decrypted(exchange.as_ref()) {
        document = JsValue::parse(text)
            .ok()
            .filter(|value| parse_market_snapshot(value).is_some());
        // ts:1399 — a body that decoded but is not a listing is printed whole
        // rather than silently dropped.
        if document.is_none() && !quiet {
            app.out.emit(&views::opaque_payload(text));
        }
    }

    let mut result = None;
    if let (Some(options), Some(station), Some(document)) = (eddn, station, document.as_ref())
        && let Some(snapshot) = parse_market_snapshot(document)
    {
        // The timestamp is the moment of publication, not of the poll.
        let timestamp = time::iso8601_from_ms(app.ports.clock.now_ms()).unwrap_or_default();
        let message = build_message(
            station,
            market_id,
            &snapshot.commodities,
            &timestamp,
            options,
        );
        result = Some(if app.session.dry_run {
            EddnResult {
                ok: true,
                status: None,
                // ts:1406
                detail: format!(
                    "dry-run: {} commodities ready",
                    js::js_number(message.count as f64)
                ),
                commodities: message.count,
            }
        } else {
            let body = message.payload.stringify_compact();
            crate::eddn::submit(
                app.http,
                &app.overrides.eddn_url,
                body.as_bytes(),
                message.count,
            )
            .await
        });
    }

    Ok(Visit {
        status,
        document,
        eddn: result,
    })
}

// ---------------------------------------------------------------------------
// A whole system
// ---------------------------------------------------------------------------

/// `runMarketSweep` (ts:1601).
#[expect(
    clippy::too_many_lines,
    reason = "R50: this is one ordered sequence of reads, network calls and emissions, and the \
              order is the specification; hiding half of it behind a helper would put the thing \
              under review behind a call graph"
)]
async fn system<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    name: &str,
) -> CmdResult {
    let eddn = load_eddn(app)?;

    if !app.session.json {
        // ts:1605
        app.note(format!("resolving \"{name}\" through Ardent..."));
    }
    // Two-valued, not three: the sweep asks only whether `--station` was given
    // \[R52\].
    let lookup = match config::sweep_lookup_mode(&app.cli) {
        LookupMode::Station => Lookup::Station,
        _ => Lookup::Auto,
    };
    let resolved = ArdentClient::new(app.http, &app.overrides.ardent_base)
        .resolve_location(name, lookup)
        .await?;

    if !app.session.json {
        // ts:1608
        app.note(format!(
            "reading {} for {} to find its markets...",
            STARSYSTEM.path, resolved.system.name
        ));
    }
    let stamp = app.stamp()?;
    // The sweep passes a literal 0 for `cachedTimeStamp` and never reads the
    // flag \[R51\].
    let query = config::starsystem_query(&app.cli, config::CachedTimestamp::SweepZero)
        .map_err(|error| error.message().to_owned())?;
    let request = app.prepare(
        STARSYSTEM,
        game_api::starsystem_fields(
            resolved.system.address,
            &query.language,
            query.cached_timestamp,
            &app.credentials,
            stamp.frontier_time,
        ),
        stamp,
    );
    // Read-only, so it runs even under `--dry-run` \[R74\].
    let exchange = app
        .send(
            &request,
            SendOptions {
                quiet: true,
                ignore_dry_run: true,
            },
        )
        .await;
    let Some(text) = decrypted(exchange.as_ref()) else {
        // ts:1619
        return Err(
            "Could not read the star system; try `markets` first to see what is there".to_owned(),
        );
    };

    // ts:1621 — an unparseable payload throws here rather than degrading; the
    // message is the JSON lexer's, and ours is not JavaScriptCore's \[C15\].
    let payload = JsValue::parse(text).map_err(|error| error.to_string())?;
    let all: Vec<MarketPoint<'_>> = payload
        .as_record()
        .map_or_else(Vec::new, read_market_points);
    if all.is_empty() {
        // ts:1625
        return Err(
            "No markets found in that system; run `markets --dump <file>` to inspect the payload"
                .to_owned(),
        );
    }

    let include_carriers = app
        .cli
        .switch_value(Flag::Carriers, false)
        .map_err(message)?;
    let include_idle = app
        .cli
        .switch_value(Flag::AllMarkets, false)
        .map_err(message)?;

    let skipped_carriers = if include_carriers {
        0
    } else {
        all.iter().filter(|point| point.is_carrier()).count()
    };
    let survivors: Vec<&MarketPoint<'_>> = all
        .iter()
        .filter(|point| include_carriers || !point.is_carrier())
        .collect();
    // Counted *after* the carrier filter, exactly as the TypeScript's second
    // `targets.filter` is.
    let skipped_idle = if include_idle {
        0
    } else {
        survivors.iter().filter(|point| !point.trades()).count()
    };
    let targets: Vec<MarketPoint<'_>> = survivors
        .into_iter()
        .filter(|point| include_idle || point.trades())
        .cloned()
        .collect();

    if targets.is_empty() {
        // ts:1629
        return Err("Every market was filtered out; add --all-markets or --carriers".to_owned());
    }

    // Read only now, after two network calls have already happened \[R50\].
    let settings = config::sweep_settings(&app.cli, app.session.json).map_err(message)?;

    if !app.session.json {
        let mut rows = vec![
            // ts:1642
            field(
                "system",
                format!(
                    "{} ({})",
                    resolved.system.name,
                    js::js_number(resolved.system.address)
                ),
            ),
            field("markets", format!("{} of {}", targets.len(), all.len())),
            field(
                "workers",
                format!("{} pulling from one queue", settings.workers),
            ),
            field(
                "timeout",
                format!(
                    // `${ms / 1000}s` is `Number::toString`, never `{:.1}` \[R92\].
                    "{}s per attempt, up to {} requeues",
                    js::js_number(settings.timeout_ms / 1_000.0),
                    js::js_number(settings.requeues),
                ),
            ),
            field(
                "eddn",
                match &eddn {
                    None => "off".to_owned(),
                    Some(options) if options.test => "test schema".to_owned(),
                    Some(_) => "live".to_owned(),
                },
            ),
        ];
        // The two skipped rows appear only when they are non-zero.
        if skipped_carriers != 0 {
            rows.push(field("carriers skipped", skipped_carriers.to_string()));
        }
        if skipped_idle != 0 {
            rows.push(field("no-market skipped", skipped_idle.to_string()));
        }
        app.out.emit(&[Block::Table {
            title: "SWEEP".to_owned(),
            columns: edm_core::render::columns::FIELD_COLUMNS,
            rows,
        }]);
    }

    let stamp_overrides = app.stamp_overrides()?;
    let publish = eddn.as_ref().map(|options| EddnPublish {
        options,
        url: &app.overrides.eddn_url,
        system_name: &resolved.system.name,
    });
    // `--detail` renders from inside the worker, right after that market's
    // progress line, which is where the original prints it (ts:1546).
    let detail = |visit: &sweep::MarketVisit| {
        if let Some(snapshot) = visit.snapshot() {
            app.out.emit(&views::market_snapshot(
                &snapshot,
                &format!(
                    "MARKET  {} ({})",
                    visit.name,
                    js::js_number(visit.market_id)
                ),
            ));
        }
    };
    let cx = Cx {
        detail: Some(&detail),
        origin: &app.overrides.origin,
        http: app.http,
        clock: &app.ports.clock,
        entropy: &app.ports.entropy,
        out: app.out,
        credentials: &app.credentials,
        headers: &app.headers,
        method_override: app.session.method_override.as_deref(),
        dry_run: app.session.dry_run,
        nonce_override: stamp_overrides.nonce,
        frontier_time_override: stamp_overrides.frontier_time,
        request_time_override: stamp_overrides.request_time,
        eddn: publish.as_ref(),
    };
    let pool = sweep::SweepSettings {
        workers: settings.workers as usize,
        timeout: timer_duration(settings.timeout_ms),
        requeues: settings.requeues,
        quiet: settings.quiet,
        detail: settings.detail,
    };
    let visits = sweep::sweep(&cx, &targets, &pool).await;

    // Materialised once: `Visit` borrows its snapshot, and re-parsing per row
    // would hand out references to temporaries.
    let snapshots: Vec<Option<MarketSnapshot<'_>>> =
        visits.iter().map(sweep::MarketVisit::snapshot).collect();

    if app.session.json {
        // ts:1658
        app.out.line(
            &object([
                ("system".to_owned(), resolved_json(&resolved)),
                (
                    "markets".to_owned(),
                    JsValue::Arr(
                        visits
                            .iter()
                            .zip(&snapshots)
                            .map(|(visit, snapshot)| {
                                object([
                                    ("marketId".to_owned(), JsValue::Num(visit.market_id)),
                                    ("name".to_owned(), str_value(&visit.name)),
                                    (
                                        "status".to_owned(),
                                        num_or_null(visit.status.map(f64::from)),
                                    ),
                                    (
                                        "eddn".to_owned(),
                                        visit.eddn.as_ref().map_or(JsValue::Null, eddn_json),
                                    ),
                                    (
                                        "payload".to_owned(),
                                        snapshot.as_ref().map_or(JsValue::Null, |snapshot| {
                                            JsValue::Obj(snapshot.payload.clone())
                                        }),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ])
            .stringify(2),
        );
        // R78: a `--json` sweep returns *before* the failure tally, so it never
        // sets exit 1 for markets with no usable data.
        return Ok(());
    }

    let rows: Vec<views::Visit<'_>> = visits
        .iter()
        .zip(&snapshots)
        .map(|(visit, snapshot)| views::Visit {
            market_id: visit.market_id,
            name: &visit.name,
            status: visit.status.map(f64::from),
            snapshot: snapshot.as_ref(),
            eddn: visit.eddn.as_ref().map(|result| views::EddnOutcome {
                ok: result.ok,
                detail: &result.detail,
            }),
            attempts: Some(f64::from(visit.attempts)),
        })
        .collect();
    app.out.emit(&views::sweep_summary(
        &rows,
        // ts:1670 — the dash is U+2014.
        &format!(
            "SWEEP RESULTS  {} \u{2014} {} markets",
            resolved.system.name,
            visits.len()
        ),
        app.out.metric(),
    ));

    let failed = snapshots
        .iter()
        .filter(|snapshot| snapshot.is_none())
        .count();
    if failed > 0 {
        app.out.set_exit(EXIT_FAILURE);
        // ts:1674
        app.note(format!("{failed} markets returned no usable data"));
    }
    if let Some(options) = &eddn {
        let sent = visits
            .iter()
            .filter(|visit| visit.eddn.as_ref().is_some_and(|e| e.ok))
            .count();
        let rejected: Vec<&sweep::MarketVisit> = visits
            .iter()
            .filter(|visit| visit.eddn.as_ref().is_some_and(|result| !result.ok))
            .collect();
        // ts:1678
        app.note(format!(
            "EDDN: {sent} sent, {} rejected{}",
            rejected.len(),
            if options.test {
                " (test schema — not relayed to consumers)"
            } else {
                ""
            },
        ));
        for visit in rejected.iter().take(5) {
            // ts:1680 — two leading spaces, inside a note that indents by three.
            app.note(format!(
                "  {}: {}",
                visit.name,
                visit
                    .eddn
                    .as_ref()
                    .map_or("", |result| result.detail.as_str())
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// `wantsEddn ? loadEddnOptions(...) : null` (ts:1558, ts:1603).
///
/// The `||` in `wantsEddn` short-circuits, so with `--eddn` set a poisoned
/// `--eddn-test` is never read and never throws \[R47\].
fn load_eddn<H, C, E, F>(app: &App<'_, H, C, E, F>) -> Result<Option<EddnOptions>, String> {
    if !config::wants_eddn(&app.cli).map_err(message)? {
        return Ok(None);
    }
    config::eddn_config(&app.cli, &app.session.credentials)
        .map(Some)
        .map_err(message)
}

/// `EddnStation` (ts:2848) as JSON.
fn station_json(station: &EddnStation) -> JsValue {
    object([
        ("systemName".to_owned(), str_value(&station.system_name)),
        ("stationName".to_owned(), str_value(&station.station_name)),
        (
            "stationType".to_owned(),
            station
                .station_type
                .as_deref()
                .map_or(JsValue::Null, str_value),
        ),
        (
            "economies".to_owned(),
            station
                .economies
                .as_ref()
                .map_or(JsValue::Null, |economies| {
                    JsValue::Arr(
                        economies
                            .iter()
                            .map(|(name, proportion)| {
                                object([
                                    ("name".to_owned(), str_value(name)),
                                    ("proportion".to_owned(), JsValue::Num(*proportion)),
                                ])
                            })
                            .collect(),
                    )
                }),
        ),
    ])
}

/// `EddnResult` (ts:2941) as JSON.
fn eddn_json(result: &EddnResult) -> JsValue {
    object([
        ("ok".to_owned(), JsValue::Bool(result.ok)),
        (
            "status".to_owned(),
            num_or_null(result.status.map(f64::from)),
        ),
        ("detail".to_owned(), str_value(&result.detail)),
        (
            "commodities".to_owned(),
            JsValue::Num(result.commodities as f64),
        ),
    ])
}

/// `ResolvedSystem` (ts:2451) as JSON.
pub(crate) fn resolved_json(resolved: &ResolvedSystem) -> JsValue {
    object([
        ("name".to_owned(), str_value(&resolved.system.name)),
        ("address".to_owned(), JsValue::Num(resolved.system.address)),
        (
            "coordinates".to_owned(),
            object([
                ("x".to_owned(), JsValue::Num(resolved.system.coordinates.x)),
                ("y".to_owned(), JsValue::Num(resolved.system.coordinates.y)),
                ("z".to_owned(), JsValue::Num(resolved.system.coordinates.z)),
            ]),
        ),
        ("via".to_owned(), str_value(&resolved.via)),
        (
            "station".to_owned(),
            resolved.station.as_deref().map_or(JsValue::Null, str_value),
        ),
    ])
}
