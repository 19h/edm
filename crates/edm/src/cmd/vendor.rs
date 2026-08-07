//! `vendor` — locate live Pioneer Supplies inventory by system, station, or market.

use futures_util::stream::{self, StreamExt as _};

use edm_core::ardent::{ArdentStation, Lookup, is_carrier};
use edm_core::cli::Flag;
use edm_core::cli::config::LookupMode;
use edm_core::cli::vendor::{VendorTarget, minimum_level, vendor_target};
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

use super::{App, CmdResult, message, object, str_value};

const PIONEER_SUPPLIES: f64 = 1.0;

#[derive(Clone, Debug)]
struct TargetMarket {
    index: usize,
    market_id: f64,
    station: String,
    system: String,
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
) -> CmdResult {
    if app.session.json {
        app.out.stdout_is_a_document();
    }

    let target = vendor_target(&app.cli).map_err(message)?;
    let minimum_level = minimum_level(&app.cli).map_err(message)?;
    let mut markets = resolve_markets(app, target).await?;
    if markets.is_empty() {
        return Err("Ardent found no markets to check for Pioneer Supplies stock".to_owned());
    }
    for (index, market) in markets.iter_mut().enumerate() {
        market.index = index;
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
        emit_dry_run(app, &prepared, minimum_level).await;
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
            .document(&json_document(&visits, detail, minimum_level).stringify(2));
    } else {
        emit_table(app, &visits, detail, minimum_level);
    }
    Ok(())
}

async fn resolve_markets<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    target: VendorTarget,
) -> Result<Vec<TargetMarket>, String> {
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);
    match target {
        VendorTarget::Market(market_id) => {
            let station = ardent.station_by_market_id(market_id).await;
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
            if let Some(market_id) = resolved.market_id {
                return Ok(vec![TargetMarket {
                    index: 0,
                    market_id,
                    station: resolved.station.unwrap_or_else(|| name.clone()),
                    system: resolved.system.name,
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
            Ok(rows.into_iter().enumerate().map(target_market).collect())
        }
    }
}

fn target_market((index, row): (usize, ArdentStation)) -> TargetMarket {
    TargetMarket {
        index,
        market_id: row.market_id,
        station: row.station_name,
        system: row.system_name,
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
    Visit {
        target,
        payload,
        succeeded,
        items,
    }
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
    let mut rows = Vec::new();
    for visit in visits {
        for item in &visit.items {
            let name = if detail && item.name() != item.symbol {
                format!("{} [{}]", item.name(), item.symbol)
            } else {
                item.name().to_owned()
            };
            rows.push(Row::data([
                js::js_number(visit.target.market_id),
                visit.target.station.clone(),
                item.kind.as_str().to_owned(),
                name,
                js::format_integer(item.grade),
                item.quantity.map_or_else(String::new, js::format_integer),
                js::format_integer(item.price),
                item.mods.join(", "),
            ]));
        }
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

fn json_document(visits: &[Visit], detail: bool, minimum_level: f64) -> JsValue {
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
