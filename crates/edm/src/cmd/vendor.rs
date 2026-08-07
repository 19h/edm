//! `vendor` — locate live Pioneer Supplies inventory by system, station, or market.

use futures_util::stream::{self, StreamExt as _};

use edm_core::ardent::{ArdentStation, Lookup, ReferenceSystem, is_carrier};
use edm_core::cli::Flag;
use edm_core::cli::config::LookupMode;
use edm_core::cli::vendor::{
    VendorTarget, minimum_level, search_radius, vendor_target_with_default,
};
use edm_core::consts::{DEFAULT_CONCURRENCY, MAX_CONCURRENCY, VENDOR_ITEMS};
use edm_core::domain::vendor::{VendorItem, read_outfitting_items, read_premium_items};
use edm_core::js;
use edm_core::js::json::JsValue;
use edm_core::render::{Block, Row, columns};

use crate::ardent::ArdentClient;
use crate::exchange::SendOptions;
use crate::game_api::{self, PreparedRequest};
use crate::net::HttpTransport;
use crate::out::EXIT_FAILURE;
use crate::ports::{Clock, Entropy, Fs};
use crate::route::discover::{self, DEFAULT_ANCHOR_BUDGET};

use super::{App, CmdResult, message, object, str_value};

const PIONEER_SUPPLIES: f64 = 1.0;
/// Ardent is unmetered discovery; match the route search's polite fan-out.
const ARDENT_CONCURRENCY: usize = 16;

#[derive(Clone, Debug)]
struct TargetMarket {
    index: usize,
    market_id: f64,
    station: String,
    system: String,
    /// Separation from the system that centres this search.
    distance_ly: Option<f64>,
    station_type: Option<String>,
}

#[derive(Debug)]
struct Visit {
    target: TargetMarket,
    /// Any decoded JSON, retained for documentary output even when its shape is wrong.
    payload: Option<JsValue>,
    succeeded: bool,
    items: Vec<VendorItem>,
}

/// Runs a Pioneer Supplies locator.  This command is Rust-only and therefore
/// uses documentary JSON rather than inheriting the ported commands' JSON leak.
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    current_system: Option<&str>,
) -> CmdResult {
    if app.session.json {
        app.out.stdout_is_a_document();
    }

    let target = vendor_target_with_default(&app.cli, current_system).map_err(message)?;
    let radius_ly = search_radius(&app.cli).map_err(message)?;
    let minimum_level = minimum_level(&app.cli).map_err(message)?;
    let max_requests = app
        .cli
        .optional_decimal(Flag::MaxRequests)
        .map_err(message)?
        .unwrap_or(edm_core::spend::DEFAULT_MAX_REQUESTS);
    let confirmed = app.cli.switch_value(Flag::Yes, false).map_err(message)?;
    let mut markets = resolve_markets(app, target, radius_ly).await?;
    if markets.is_empty() {
        return Err("Ardent found no markets to check for Pioneer Supplies stock".to_owned());
    }
    for (index, market) in markets.iter_mut().enumerate() {
        market.index = index;
    }
    let request_count = markets.len() as f64;
    if request_count > max_requests {
        return Err(format!(
            "the vendor request count ({}) is above the {} ceiling. Narrow --radius or raise it with --max-requests {}. Nothing has been sent.",
            js::format_integer(request_count),
            js::format_integer(max_requests),
            js::format_integer((request_count * 1.2).ceil()),
        ));
    }
    if request_count > edm_core::spend::CONFIRM_THRESHOLD && !confirmed {
        return Err(format!(
            "pass --yes to send {} requests to the game-internal API; nothing has been sent",
            js::format_integer(request_count),
        ));
    }

    let detail = app.cli.switch_value(Flag::Detail, false).map_err(message)?;
    let concurrency = app
        .cli
        .optional_number(Flag::Concurrency)
        .map_err(message)?
        .unwrap_or(f64::from(DEFAULT_CONCURRENCY))
        .clamp(1.0, f64::from(MAX_CONCURRENCY)) as usize;

    let mut prepared = Vec::with_capacity(markets.len());
    for target in markets {
        let stamp = app.stamp()?;
        let request = app.prepare(
            VENDOR_ITEMS,
            game_api::vendor_fields(
                &js::js_number(target.market_id),
                PIONEER_SUPPLIES,
                &app.credentials,
                stamp.frontier_time,
            ),
            stamp,
        );
        prepared.push((target, request));
    }

    if app.session.dry_run {
        emit_dry_run(app, &prepared, minimum_level, radius_ly).await;
        return Ok(());
    }

    if !app.session.json {
        app.out.emit(&[Block::Note(format!(
            "checking {} market{} for live Pioneer Supplies stock...",
            prepared.len(),
            if prepared.len() == 1 { "" } else { "s" },
        ))]);
    }

    let jobs = stream::iter(prepared.into_iter().map(|(target, request)| async move {
        visit(app, target, request, detail, minimum_level).await
    }));
    let mut visits: Vec<Visit> = jobs.buffer_unordered(concurrency).collect().await;
    visits.sort_by_key(|visit| visit.target.index);

    if app.session.json {
        app.out
            .document(&json_document(&visits, detail, minimum_level, radius_ly).stringify(2));
    } else {
        emit_table(app, &visits, detail, minimum_level);
    }
    Ok(())
}

async fn resolve_markets<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    target: VendorTarget,
    radius_ly: Option<f64>,
) -> Result<Vec<TargetMarket>, String> {
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);
    match target {
        VendorTarget::Market(market_id) => {
            let station = ardent.station_by_market_id(market_id).await;
            if let Some(radius_ly) = radius_ly {
                let station = station.ok_or_else(|| {
                    format!(
                        "Ardent does not know which system contains market {}; cannot centre --radius",
                        js::js_number(market_id),
                    )
                })?;
                let resolved = ardent
                    .resolve_location(&station.system_name, Lookup::System)
                    .await?;
                return markets_in_radius(app, &ardent, &resolved.system, radius_ly).await;
            }
            Ok(vec![TargetMarket {
                index: 0,
                market_id,
                station: station.as_ref().map_or_else(
                    || format!("market {}", js::js_number(market_id)),
                    |row| row.station_name.clone(),
                ),
                system: station.as_ref().map_or_else(
                    || "unknown system".to_owned(),
                    |row| row.system_name.clone(),
                ),
                distance_ly: station.as_ref().map(|_| 0.0),
                station_type: station.and_then(|row| row.station_type),
            }])
        }
        VendorTarget::Location { name, mode } => {
            if !app.session.json {
                app.out.emit(&[Block::Note(format!(
                    "resolving \"{name}\" through Ardent..."
                ))]);
            }
            let resolved = ardent.resolve_location(&name, lookup(mode)).await?;
            if let Some(radius_ly) = radius_ly {
                return markets_in_radius(app, &ardent, &resolved.system, radius_ly).await;
            }
            if let Some(market_id) = resolved.market_id {
                return Ok(vec![TargetMarket {
                    index: 0,
                    market_id,
                    station: resolved.station.unwrap_or_else(|| name.clone()),
                    system: resolved.system.name,
                    distance_ly: Some(0.0),
                    station_type: None,
                }]);
            }

            let include_carriers = app
                .cli
                .switch_value(Flag::Carriers, false)
                .map_err(message)?;
            let mut rows = ardent.system_markets(&resolved.system).await?;
            if !include_carriers {
                rows.retain(|row| !is_carrier(row.station_type.as_deref()));
            }
            if let Some(station) = resolved.station.as_deref() {
                rows.retain(|row| row.station_name.eq_ignore_ascii_case(station));
                if rows.is_empty() {
                    return Err(format!(
                        "Ardent resolved station {station} but did not provide its market id"
                    ));
                }
            }
            Ok(rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| target_market(index, row, Some(0.0)))
                .collect())
        }
    }
}

/// Expands a resolved system with the same cap-aware Ardent enumeration used
/// by `edm route`, then reads each system's markets before any Frontier request
/// is prepared or sent.
async fn markets_in_radius<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    ardent: &ArdentClient<'_, H>,
    centre: &ReferenceSystem,
    radius_ly: f64,
) -> Result<Vec<TargetMarket>, String> {
    let include_carriers = app
        .cli
        .switch_value(Flag::Carriers, false)
        .map_err(message)?;
    let cache_enabled = app.cli.switch_value(Flag::Cache, true).map_err(message)?;
    let refresh = app
        .cli
        .switch_value(Flag::Refresh, false)
        .map_err(message)?;
    let cache_root = crate::route::cache::Cache::locate(
        app.cli.env("XDG_CACHE_HOME"),
        app.cli.env("HOME"),
        app.cli.optional_value(Flag::CacheDir, None),
    );
    if !app.session.json {
        app.out.emit(&[Block::Note(format!(
            "enumerating systems within {} Ly of {}...",
            js::js_number(radius_ly),
            centre.name,
        ))]);
    }

    // Nearby systems and station lists age slowly, so a regional vendor
    // search shares route's Ardent atlas without ever caching vendor stock.
    let atlas = crate::route::atlas::Atlas::new(&cache_root, cache_enabled, refresh);
    let now_ms = app.ports.clock.now_ms();
    let enumeration = discover::enumerate(
        ardent,
        &atlas,
        &app.ports.fs,
        now_ms,
        centre,
        radius_ly,
        DEFAULT_ANCHOR_BUDGET,
        None,
    )
    .await
    .map_err(|error| format!("enumerating systems around {}: {error}", centre.name))?;
    if enumeration.truncated {
        return Err(format!(
            "Ardent's system enumeration within {} Ly of {} was incomplete after {} queries (complete only to {} Ly); use a smaller --radius",
            js::js_number(radius_ly),
            centre.name,
            enumeration.ardent_requests,
            js::js_number(enumeration.complete_to_ly),
        ));
    }

    if !app.session.json {
        app.out.emit(&[Block::Note(format!(
            "{} system{} in range; reading their market lists...",
            enumeration.systems.len(),
            if enumeration.systems.len() == 1 {
                ""
            } else {
                "s"
            },
        ))]);
    }
    let atlas = &atlas;
    let fs = &app.ports.fs;
    let reads = stream::iter(enumeration.systems.into_iter().map(|system| async move {
        let distance_ly = system.distance;
        let reference = ReferenceSystem {
            name: system.name,
            address: system.address,
            coordinates: system.coordinates,
        };
        ardent
            .system_markets_cached(atlas, fs, now_ms, &reference)
            .await
            .map(|rows| (distance_ly, rows))
            .map_err(|error| format!("reading Ardent markets for {}: {error}", reference.name))
    }))
    .buffered(ARDENT_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let mut rows = Vec::new();
    for result in reads {
        let (distance_ly, batch) = result?;
        rows.extend(batch.into_iter().map(|row| (distance_ly, row)));
    }
    let mut seen = std::collections::HashSet::new();
    rows.retain(|(_, row)| seen.insert(row.market_id.to_bits()));
    if !include_carriers {
        rows.retain(|(_, row)| !is_carrier(row.station_type.as_deref()));
    }
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(index, (distance_ly, row))| target_market(index, row, Some(distance_ly)))
        .collect())
}

fn target_market(index: usize, row: ArdentStation, distance_ly: Option<f64>) -> TargetMarket {
    TargetMarket {
        index,
        market_id: row.market_id,
        station: row.station_name,
        system: row.system_name,
        distance_ly,
        station_type: row.station_type,
    }
}

const fn lookup(mode: LookupMode) -> Lookup {
    match mode {
        LookupMode::Station => Lookup::Station,
        LookupMode::System => Lookup::System,
        LookupMode::Auto => Lookup::Auto,
    }
}

async fn visit<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    target: TargetMarket,
    request: PreparedRequest,
    detail: bool,
    minimum_level: f64,
) -> Visit {
    let exchange = app
        .send(
            &request,
            SendOptions {
                quiet: true,
                ignore_dry_run: false,
            },
        )
        .await;
    let mut succeeded = false;
    let payload = exchange
        .and_then(|exchange| exchange.decrypted)
        .and_then(|text| match JsValue::parse(&text) {
            Ok(payload) if is_vendor_payload(&payload) => {
                succeeded = true;
                Some(payload)
            }
            Ok(payload) => {
                app.out.error(&format!(
                    "market {} returned JSON without vendor inventory data",
                    js::js_number(target.market_id)
                ));
                app.out.set_exit(EXIT_FAILURE);
                Some(payload)
            }
            Err(error) => {
                app.out.error(&format!(
                    "market {} returned invalid vendor JSON: {error}",
                    js::js_number(target.market_id)
                ));
                app.out.set_exit(EXIT_FAILURE);
                None
            }
        });

    let mut items = Vec::new();
    if let Some(payload) = &payload {
        items.extend(
            read_premium_items(payload)
                .into_iter()
                .filter(|item| item.grade >= minimum_level)
                .filter(|item| detail || item.available()),
        );
        items.extend(
            read_outfitting_items(payload)
                .into_iter()
                .filter(|item| item.grade >= minimum_level),
        );
    }
    items.sort_by(item_order);
    Visit {
        target,
        payload,
        succeeded,
        items,
    }
}

fn item_order(left: &VendorItem, right: &VendorItem) -> std::cmp::Ordering {
    left.name()
        .cmp(right.name())
        .then_with(|| right.grade.total_cmp(&left.grade))
        .then_with(|| left.kind.as_str().cmp(right.kind.as_str()))
        .then_with(|| left.symbol.cmp(&right.symbol))
        .then_with(|| left.id.cmp(&right.id))
}

fn is_vendor_payload(value: &JsValue) -> bool {
    value.as_record().is_some_and(|root| {
        ["premiumstock", "outfitting", "microresources"]
            .iter()
            .any(|key| {
                root.get(key)
                    .is_some_and(|section| section.is_null() || section.as_record().is_some())
            })
    })
}

async fn emit_dry_run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    prepared: &[(TargetMarket, PreparedRequest)],
    minimum_level: f64,
    radius_ly: Option<f64>,
) {
    if app.session.json {
        let markets = prepared
            .iter()
            .map(|(target, _)| target_json(target))
            .collect();
        app.out.document(
            &object([
                ("dryRun".to_owned(), JsValue::Bool(true)),
                ("vendorType".to_owned(), JsValue::Num(PIONEER_SUPPLIES)),
                ("minimumLevel".to_owned(), JsValue::Num(minimum_level)),
                (
                    "radiusLy".to_owned(),
                    radius_ly.map_or(JsValue::Null, JsValue::Num),
                ),
                ("markets".to_owned(), JsValue::Arr(markets)),
            ])
            .stringify(2),
        );
        return;
    }

    if prepared.len() == 1 {
        let _ = app
            .send(
                &prepared[0].1,
                SendOptions {
                    quiet: false,
                    ignore_dry_run: false,
                },
            )
            .await;
        return;
    }

    let rows = prepared
        .iter()
        .map(|(target, _)| {
            Row::data([
                js::js_number(target.market_id),
                target.station.clone(),
                target.system.clone(),
                target.distance_ly.map_or_else(String::new, js::to_fixed_1),
                target.station_type.clone().unwrap_or_default(),
            ])
        })
        .collect();
    app.out.emit(&[
        Block::Table {
            title: format!("VENDOR SEARCH PLAN  {} markets", prepared.len()),
            columns: columns::VENDOR_MARKET_COLUMNS,
            rows,
        },
        Block::Note("dry-run: no Frontier vendor requests sent".to_owned()),
    ]);
}

fn emit_table<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    visits: &[Visit],
    detail: bool,
    minimum_level: f64,
) {
    let mut offers: Vec<(&Visit, &VendorItem)> = visits
        .iter()
        .flat_map(|visit| visit.items.iter().map(move |item| (visit, item)))
        .collect();
    offers.sort_by(|(left_visit, left_item), (right_visit, right_item)| {
        left_item
            .name()
            .cmp(right_item.name())
            .then_with(|| {
                left_visit
                    .target
                    .distance_ly
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&right_visit.target.distance_ly.unwrap_or(f64::INFINITY))
            })
            .then_with(|| left_visit.target.system.cmp(&right_visit.target.system))
            .then_with(|| left_visit.target.station.cmp(&right_visit.target.station))
            .then_with(|| right_item.grade.total_cmp(&left_item.grade))
            .then_with(|| left_item.kind.as_str().cmp(right_item.kind.as_str()))
            .then_with(|| left_item.symbol.cmp(&right_item.symbol))
            .then_with(|| left_item.id.cmp(&right_item.id))
            .then_with(|| {
                left_visit
                    .target
                    .market_id
                    .total_cmp(&right_visit.target.market_id)
            })
    });

    let mut rows = Vec::with_capacity(offers.len());
    for (visit, item) in offers {
        let name = if detail && item.name() != item.symbol {
            format!("{} [{}]", item.name(), item.symbol)
        } else {
            item.name().to_owned()
        };
        rows.push(Row::data([
            js::js_number(visit.target.market_id),
            visit.target.station.clone(),
            visit.target.system.clone(),
            visit
                .target
                .distance_ly
                .map_or_else(String::new, js::to_fixed_1),
            item.kind.as_str().to_owned(),
            name,
            js::format_integer(item.grade),
            item.quantity.map_or_else(String::new, js::format_integer),
            js::format_integer(item.price),
            item.mods.join(", "),
        ]));
    }

    let succeeded = visits.iter().filter(|visit| visit.succeeded).count();
    let level_label = if minimum_level > 1.0 {
        format!(" G{}+", js::format_integer(minimum_level))
    } else {
        String::new()
    };
    let title = format!(
        "VENDOR ITEMS{level_label}  {} offer{} across {} market{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        visits.len(),
        if visits.len() == 1 { "" } else { "s" },
    );
    if rows.is_empty() {
        app.out.emit(&[
            Block::Heading(title),
            Block::Note(format!(
                "no Pioneer Supplies items of grade {} or higher were available",
                js::format_integer(minimum_level)
            )),
        ]);
    } else {
        app.out.emit(&[Block::Table {
            title,
            columns: columns::VENDOR_COLUMNS,
            rows,
        }]);
    }
    if succeeded < visits.len() {
        app.out.emit(&[Block::Note(format!(
            "coverage incomplete: {succeeded} of {} markets returned decoded vendor data",
            visits.len()
        ))]);
    }
}

fn json_document(
    visits: &[Visit],
    detail: bool,
    minimum_level: f64,
    radius_ly: Option<f64>,
) -> JsValue {
    let markets = visits
        .iter()
        .map(|visit| {
            let items = visit.items.iter().map(item_json).collect();
            object([
                ("market".to_owned(), target_json(&visit.target)),
                ("items".to_owned(), JsValue::Arr(items)),
                (
                    "payload".to_owned(),
                    visit.payload.clone().unwrap_or(JsValue::Null),
                ),
            ])
        })
        .collect();
    let succeeded = visits.iter().filter(|visit| visit.succeeded).count();
    let item_count = visits.iter().map(|visit| visit.items.len()).sum::<usize>();
    object([
        ("vendorType".to_owned(), JsValue::Num(PIONEER_SUPPLIES)),
        ("minimumLevel".to_owned(), JsValue::Num(minimum_level)),
        (
            "radiusLy".to_owned(),
            radius_ly.map_or(JsValue::Null, JsValue::Num),
        ),
        ("detail".to_owned(), JsValue::Bool(detail)),
        ("markets".to_owned(), JsValue::Arr(markets)),
        (
            "summary".to_owned(),
            object([
                ("markets".to_owned(), JsValue::Num(visits.len() as f64)),
                ("succeeded".to_owned(), JsValue::Num(succeeded as f64)),
                (
                    "failed".to_owned(),
                    JsValue::Num((visits.len() - succeeded) as f64),
                ),
                ("items".to_owned(), JsValue::Num(item_count as f64)),
            ]),
        ),
    ])
}

fn target_json(target: &TargetMarket) -> JsValue {
    object([
        ("marketId".to_owned(), JsValue::Num(target.market_id)),
        ("station".to_owned(), str_value(&target.station)),
        ("system".to_owned(), str_value(&target.system)),
        (
            "distanceLy".to_owned(),
            target.distance_ly.map_or(JsValue::Null, JsValue::Num),
        ),
        (
            "stationType".to_owned(),
            target
                .station_type
                .as_deref()
                .map_or(JsValue::Null, str_value),
        ),
    ])
}

fn item_json(item: &VendorItem) -> JsValue {
    object([
        ("kind".to_owned(), str_value(item.kind.as_str())),
        ("name".to_owned(), str_value(item.name())),
        ("symbol".to_owned(), str_value(&item.symbol)),
        ("id".to_owned(), str_value(&item.id)),
        ("grade".to_owned(), JsValue::Num(item.grade)),
        ("premium".to_owned(), JsValue::Bool(item.premium())),
        (
            "quantity".to_owned(),
            item.quantity.map_or(JsValue::Null, JsValue::Num),
        ),
        ("price".to_owned(), JsValue::Num(item.price)),
        (
            "mods".to_owned(),
            JsValue::Arr(
                item.mods
                    .iter()
                    .map(|modifier| str_value(modifier))
                    .collect(),
            ),
        ),
    ])
}
