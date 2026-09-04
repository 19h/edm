//! `cz` — list combat zones in a system, or every system in a radius.

use futures_util::stream::{self, StreamExt as _};

use edm_core::ardent::{Lookup, NearbySystem, ReferenceSystem};
use edm_core::cli::Flag;
use edm_core::cli::config::{CachedTimestamp, LookupMode};
use edm_core::cli::cz::{CzTarget, cz_target_with_default, search_radius};
use edm_core::consts::{DEFAULT_CONCURRENCY, MAX_CONCURRENCY, STARSYSTEM};
use edm_core::domain::cz::{CombatZone, read_combat_zones};
use edm_core::domain::read::Read;
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

#[derive(Clone, Debug)]
struct TargetSystem {
    index: usize,
    name: String,
    address: f64,
    distance_ly: f64,
}

#[derive(Debug)]
struct Visit {
    target: TargetSystem,
    payload: Option<JsValue>,
    succeeded: bool,
    zones: Vec<CombatZone>,
}

/// Runs a combat-zone locator. Rust-only, so JSON is one documentary document.
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    current_system: Option<&str>,
) -> CmdResult {
    if app.session.json {
        app.out.stdout_is_a_document();
    }

    let target = cz_target_with_default(&app.cli, current_system).map_err(message)?;
    let radius_ly = search_radius(&app.cli).map_err(message)?;
    let include_settlements = app
        .cli
        .switch_value(Flag::Settlements, false)
        .map_err(message)?;
    let detail = app.cli.switch_value(Flag::Detail, false).map_err(message)?;
    let max_requests = app
        .cli
        .optional_decimal(Flag::MaxRequests)
        .map_err(message)?
        .unwrap_or(edm_core::spend::DEFAULT_MAX_REQUESTS);
    let confirmed = app.cli.switch_value(Flag::Yes, false).map_err(message)?;
    let query =
        edm_core::cli::config::starsystem_query(&app.cli, CachedTimestamp::Flag).map_err(message)?;

    let mut systems = resolve_systems(app, target, radius_ly).await?;
    if systems.is_empty() {
        return Err("Ardent found no systems to check for combat zones".to_owned());
    }
    for (index, system) in systems.iter_mut().enumerate() {
        system.index = index;
    }
    let request_count = systems.len() as f64;
    if request_count > max_requests {
        return Err(format!(
            "the combat-zone request count ({}) is above the {} ceiling. Narrow --radius or raise it with --max-requests {}. Nothing has been sent.",
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

    let concurrency = app
        .cli
        .optional_number(Flag::Concurrency)
        .map_err(message)?
        .unwrap_or(f64::from(DEFAULT_CONCURRENCY))
        .clamp(1.0, f64::from(MAX_CONCURRENCY)) as usize;

    let mut prepared = Vec::with_capacity(systems.len());
    for target in systems {
        let stamp = app.stamp()?;
        let request = app.prepare(
            STARSYSTEM,
            game_api::starsystem_fields(
                target.address,
                &query.language,
                query.cached_timestamp,
                &app.credentials,
                stamp.frontier_time,
            ),
            stamp,
        );
        prepared.push((target, request));
    }

    if app.session.dry_run {
        emit_dry_run(app, &prepared, radius_ly, include_settlements).await;
        return Ok(());
    }

    if !app.session.json {
        app.out.emit(&[Block::Note(format!(
            "reading {} system{} for combat zones...",
            prepared.len(),
            if prepared.len() == 1 { "" } else { "s" },
        ))]);
    }

    let jobs = stream::iter(prepared.into_iter().map(|(target, request)| async move {
        visit(app, target, request, include_settlements).await
    }));
    let mut visits: Vec<Visit> = jobs.buffer_unordered(concurrency).collect().await;
    visits.sort_by_key(|visit| visit.target.index);

    if app.session.json {
        app.out.document(
            &json_document(&visits, detail, include_settlements, radius_ly).stringify(2),
        );
    } else {
        emit_table(app, &visits, detail, include_settlements);
    }
    Ok(())
}

async fn resolve_systems<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    target: CzTarget,
    radius_ly: Option<f64>,
) -> Result<Vec<TargetSystem>, String> {
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);
    let CzTarget::Location { name, mode } = target;
    if !app.session.json {
        app.out.emit(&[Block::Note(format!(
            "resolving \"{name}\" through Ardent..."
        ))]);
    }
    let resolved = ardent.resolve_location(&name, lookup(mode)).await?;
    if let Some(radius_ly) = radius_ly {
        return systems_in_radius(app, &ardent, &resolved.system, radius_ly).await;
    }
    Ok(vec![TargetSystem {
        index: 0,
        name: resolved.system.name,
        address: resolved.system.address,
        distance_ly: 0.0,
    }])
}

async fn systems_in_radius<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    ardent: &ArdentClient<'_, H>,
    centre: &ReferenceSystem,
    radius_ly: f64,
) -> Result<Vec<TargetSystem>, String> {
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

    Ok(enumeration
        .systems
        .into_iter()
        .enumerate()
        .map(|(index, system)| target_system(index, system))
        .collect())
}

fn target_system(index: usize, system: NearbySystem) -> TargetSystem {
    TargetSystem {
        index,
        name: system.name,
        address: system.address,
        distance_ly: system.distance,
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
    target: TargetSystem,
    request: PreparedRequest,
    include_settlements: bool,
) -> Visit {
    let exchange = app
        .send(
            &request,
            SendOptions {
                quiet: true,
                ignore_dry_run: false,
                quiet_failure: false,
            },
        )
        .await;
    let mut succeeded = false;
    let payload = exchange
        .and_then(|exchange| exchange.decrypted)
        .and_then(|text| match JsValue::parse(&text) {
            Ok(payload) if is_starsystem_payload(&payload) => {
                succeeded = true;
                Some(payload)
            }
            Ok(payload) => {
                app.out.error(&format!(
                    "system {} returned JSON without starsystem data",
                    target.name
                ));
                app.out.set_exit(EXIT_FAILURE);
                Some(payload)
            }
            Err(error) => {
                app.out.error(&format!(
                    "system {} returned invalid starsystem JSON: {error}",
                    target.name
                ));
                app.out.set_exit(EXIT_FAILURE);
                None
            }
        });

    let zones = payload
        .as_ref()
        .map(|payload| read_combat_zones(payload, include_settlements))
        .unwrap_or_default();
    Visit {
        target,
        payload,
        succeeded,
        zones,
    }
}

fn is_starsystem_payload(value: &JsValue) -> bool {
    value
        .as_record()
        .and_then(|root| root.record("starsystem"))
        .is_some()
}

async fn emit_dry_run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    prepared: &[(TargetSystem, PreparedRequest)],
    radius_ly: Option<f64>,
    include_settlements: bool,
) {
    if app.session.json {
        let systems = prepared
            .iter()
            .map(|(target, _)| target_json(target))
            .collect();
        app.out.document(
            &object([
                ("dryRun".to_owned(), JsValue::Bool(true)),
                (
                    "radiusLy".to_owned(),
                    radius_ly.map_or(JsValue::Null, JsValue::Num),
                ),
                (
                    "settlements".to_owned(),
                    JsValue::Bool(include_settlements),
                ),
                ("systems".to_owned(), JsValue::Arr(systems)),
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
                    quiet_failure: false,
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
                target.name.clone(),
                js::to_fixed_1(target.distance_ly),
                js::js_number(target.address),
            ])
        })
        .collect();
    app.out.emit(&[
        Block::Table {
            title: format!("COMBAT ZONE SEARCH PLAN  {} systems", prepared.len()),
            columns: columns::CZ_PLAN_COLUMNS,
            rows,
        },
        Block::Note("dry-run: no Frontier starsystem requests sent".to_owned()),
    ]);
}

fn emit_table<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    visits: &[Visit],
    detail: bool,
    include_settlements: bool,
) {
    let mut rows_src: Vec<(&Visit, &CombatZone)> = visits
        .iter()
        .flat_map(|visit| visit.zones.iter().map(move |zone| (visit, zone)))
        .collect();
    rows_src.sort_by(|(left_visit, left_zone), (right_visit, right_zone)| {
        left_visit
            .target
            .distance_ly
            .total_cmp(&right_visit.target.distance_ly)
            .then_with(|| left_zone.intensity.rank().cmp(&right_zone.intensity.rank()))
            .then_with(|| left_visit.target.name.cmp(&right_visit.target.name))
            .then_with(|| left_zone.location().cmp(right_zone.location()))
            .then_with(|| {
                left_zone
                    .dist_ls
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&right_zone.dist_ls.unwrap_or(f64::INFINITY))
            })
            .then_with(|| left_zone.site_id.total_cmp(&right_zone.site_id))
    });

    let rows: Vec<Row<'_>> = rows_src
        .iter()
        .map(|(visit, zone)| zone_row(visit, zone, detail, include_settlements))
        .collect();

    let succeeded = visits.iter().filter(|visit| visit.succeeded).count();
    let title = format!(
        "COMBAT ZONES  {} zone{} across {} system{}",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        visits.len(),
        if visits.len() == 1 { "" } else { "s" },
    );
    let columns = if detail {
        columns::CZ_DETAIL_COLUMNS
    } else if include_settlements {
        columns::CZ_SETTLEMENT_COLUMNS
    } else {
        columns::CZ_COLUMNS
    };
    if rows.is_empty() {
        app.out.emit(&[
            Block::Heading(title),
            Block::Note("no combat zones were listed in the systems that replied".to_owned()),
        ]);
    } else {
        app.out.emit(&[Block::Table {
            title,
            columns,
            rows,
        }]);
    }
    if succeeded < visits.len() {
        app.out.emit(&[Block::Note(format!(
            "coverage incomplete: {succeeded} of {} systems returned decoded starsystem data",
            visits.len()
        ))]);
    }
}

fn zone_row<'a>(
    visit: &'a Visit,
    zone: &'a CombatZone,
    detail: bool,
    include_settlements: bool,
) -> Row<'a> {
    let ly = js::to_fixed_1(visit.target.distance_ly);
    let ls = zone
        .dist_ls
        .map_or_else(String::new, js::format_integer);
    let conflict = zone
        .conflict
        .as_deref()
        .map(display_conflict)
        .unwrap_or_default();
    if detail {
        Row::data([
            visit.target.name.clone(),
            zone.location().to_owned(),
            ly,
            zone.kind.as_str().to_owned(),
            zone.intensity.as_str().to_owned(),
            zone.difficulty.clone().unwrap_or_default(),
            conflict,
            zone.sides(),
            ls,
            js::js_number(zone.site_id),
            zone.gameplay.clone(),
        ])
    } else if include_settlements {
        Row::data([
            visit.target.name.clone(),
            zone.location().to_owned(),
            ly,
            zone.kind.as_str().to_owned(),
            zone.intensity.as_str().to_owned(),
            zone.difficulty.clone().unwrap_or_default(),
            conflict,
            zone.sides(),
            ls,
        ])
    } else {
        Row::data([
            visit.target.name.clone(),
            ly,
            zone.intensity.as_str().to_owned(),
            conflict,
            zone.sides(),
            ls,
        ])
    }
}

fn display_conflict(raw: &str) -> String {
    match raw {
        "war" => "War".to_owned(),
        "civilwar" => "Civil war".to_owned(),
        other => other.to_owned(),
    }
}

fn json_document(
    visits: &[Visit],
    detail: bool,
    include_settlements: bool,
    radius_ly: Option<f64>,
) -> JsValue {
    let systems = visits
        .iter()
        .map(|visit| {
            let zones = visit.zones.iter().map(zone_json).collect();
            object([
                ("system".to_owned(), target_json(&visit.target)),
                ("zones".to_owned(), JsValue::Arr(zones)),
                (
                    "payload".to_owned(),
                    visit.payload.clone().unwrap_or(JsValue::Null),
                ),
            ])
        })
        .collect();
    let succeeded = visits.iter().filter(|visit| visit.succeeded).count();
    let zone_count = visits.iter().map(|visit| visit.zones.len()).sum::<usize>();
    object([
        (
            "radiusLy".to_owned(),
            radius_ly.map_or(JsValue::Null, JsValue::Num),
        ),
        ("settlements".to_owned(), JsValue::Bool(include_settlements)),
        ("detail".to_owned(), JsValue::Bool(detail)),
        ("systems".to_owned(), JsValue::Arr(systems)),
        (
            "summary".to_owned(),
            object([
                ("systems".to_owned(), JsValue::Num(visits.len() as f64)),
                ("succeeded".to_owned(), JsValue::Num(succeeded as f64)),
                (
                    "failed".to_owned(),
                    JsValue::Num((visits.len() - succeeded) as f64),
                ),
                ("zones".to_owned(), JsValue::Num(zone_count as f64)),
            ]),
        ),
    ])
}

fn target_json(target: &TargetSystem) -> JsValue {
    object([
        ("name".to_owned(), str_value(&target.name)),
        ("address".to_owned(), JsValue::Num(target.address)),
        ("distanceLy".to_owned(), JsValue::Num(target.distance_ly)),
    ])
}

fn zone_json(zone: &CombatZone) -> JsValue {
    object([
        ("siteId".to_owned(), JsValue::Num(zone.site_id)),
        ("kind".to_owned(), str_value(zone.kind.as_str())),
        (
            "name".to_owned(),
            zone.name.as_deref().map_or(JsValue::Null, str_value),
        ),
        ("location".to_owned(), str_value(zone.location())),
        ("intensity".to_owned(), str_value(zone.intensity.as_str())),
        (
            "difficulty".to_owned(),
            zone.difficulty
                .as_deref()
                .map_or(JsValue::Null, str_value),
        ),
        (
            "conflict".to_owned(),
            zone.conflict.as_deref().map_or(JsValue::Null, str_value),
        ),
        (
            "primaryFaction".to_owned(),
            zone.primary_faction
                .as_deref()
                .map_or(JsValue::Null, str_value),
        ),
        (
            "secondaryFaction".to_owned(),
            zone.secondary_faction
                .as_deref()
                .map_or(JsValue::Null, str_value),
        ),
        (
            "distanceLs".to_owned(),
            zone.dist_ls.map_or(JsValue::Null, JsValue::Num),
        ),
        ("gameplay".to_owned(), str_value(&zone.gameplay)),
    ])
}
