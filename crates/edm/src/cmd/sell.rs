//! `edm sell` — where to sell what is already aboard \[C41\].
//!
//! A separate command rather than a mode of `route`, for the reason C33 gives
//! about `eddn`: route's nomination is the wrong shape. `nominate_hops` pairs
//! sellers against buyers and skips any row without a positive buy price, so a
//! hold that has already been bought is invisible to it by construction. And
//! `RouteConfig` would have to carry a `cargo` that means *free space* while
//! this command means *contents* — the same word for the opposite quantity.
//!
//! The phase order is the safety contract, as it is in `route`: free files,
//! free index, the filters, the priced gate, the live reads, then the plan.
//! Nothing enters a printed plan that this run did not read from the
//! game-internal API.
//!
//! The optimisation itself is [`edm_route::sell`], which is pure and exact; the
//! reasoning about *why* the objective is credits-minus-time rather than
//! credits-per-hour lives in that module's own doc.

use std::collections::HashMap;

use edm_core::ardent::{self, CommodityDirection, CommodityPrice};
use edm_core::cli::sell::SellConfig;
use edm_core::domain::commander::CommanderState;
use edm_core::domain::id64::Coordinates;
use edm_core::js;
use edm_core::render::{Block, Row, columns};
use edm_core::select;
use edm_core::spend::{Counts, Estimate, SizePrior};
use edm_route::model::{CommodityId, Market};
use edm_route::num::{Credits, Millis, Ratio, Tons};
use edm_route::sell::{Held, Plan};
use edm_route::time::Geometry;

use crate::ardent::ArdentClient;
use crate::cmd::{App, CmdResult};
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs, Timer};
use crate::route::acquire;
use crate::route::cache::Cache;
use crate::route::pacer::Pacer;

/// One stack of clean cargo, as the journal spells it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stack {
    /// Frontier's own symbol, e.g. `tritium`.
    symbol: String,
    tons: i64,
}

/// What was left out of the manifest, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Excluded {
    symbol: String,
    tons: i64,
    reason: &'static str,
}

/// Read the hold, excluding what cannot honestly be planned.
///
/// **Stolen tons and mission cargo are excluded and named, never guessed at.**
/// A stolen ton needs a black market even when the commodity is legal
/// everywhere — `derive_black_market` is `stolen || illegal`, and the two are
/// independent — so a station answers HTTP 401 for it. Worse, it cannot even be
/// *priced*: Ardent publishes one price per row and it is the open-market one,
/// and `RawCommodity` has no fence price at all. A plan that included them
/// would be a plan that fails at the counter.
fn manifest(state: &CommanderState, items: &[String]) -> (Vec<Stack>, Vec<Excluded>) {
    let mut clean = Vec::new();
    let mut excluded = Vec::new();
    let Some(inventory) = state.cargo.inventory.as_ref() else {
        return (clean, excluded);
    };
    for item in &inventory.value {
        let symbol = ardent::normalise_commodity_name(&item.name);
        if !items.is_empty() && !items.iter().any(|wanted| *wanted == symbol) {
            continue;
        }
        if item.mission_id.is_some() {
            excluded.push(Excluded {
                symbol,
                tons: item.count as i64,
                reason: "committed to a mission",
            });
            continue;
        }
        // `stolen` is a count, not a flag: one stack can be part clean.
        let stolen = item.stolen.min(item.count) as i64;
        if stolen > 0 {
            excluded.push(Excluded {
                symbol: symbol.clone(),
                tons: stolen,
                reason: "stolen; this program cannot see fence prices",
            });
        }
        let sellable = item.count as i64 - stolen;
        if sellable > 0 {
            clean.push(Stack {
                symbol,
                tons: sellable,
            });
        }
    }
    (clean, excluded)
}

/// The station filters, borrowed from `route`'s config so `--pad`,
/// `--station-types`, `--max-star-distance`, `--carriers` and the spend flags
/// mean exactly what they mean everywhere else.
///
/// `edm sell` deliberately does not define its own copies of those: a commander
/// who has learned what `--pad L` does should not have to learn it twice.
fn sell_route_config<H, C, E, F>(
    app: &App<'_, H, C, E, F>,
    config: &SellConfig,
) -> Result<edm_core::cli::config::RouteConfig, String> {
    let mut route =
        edm_core::cli::config::route_config_with_reference(&app.cli, Some("unused"))
            .map_err(|error| error.message().to_owned())?;
    route.radius_ly = config.radius_ly;
    route.top = config.top;
    route.min_demand = config.min_demand;
    // A disposal buys nothing, so a seller's stock is not a reason to keep or
    // drop a market. Leaving route's default here would filter buyers on a
    // quantity that has nothing to do with them.
    route.min_supply = 0.0;
    Ok(route)
}

/// Ingest floors for a disposal.
///
/// Restricted to the commodities actually aboard, which is what keeps the
/// market model — and therefore the search — the size of the hold rather than
/// the size of the galaxy's commodity list.
fn sell_floors(
    config: &edm_core::cli::config::RouteConfig,
    hold: &[Stack],
) -> edm_route::model::RowFloors {
    edm_route::model::RowFloors {
        min_stock: Tons(0),
        min_demand: Tons(config.min_demand as i64),
        categories: Vec::new(),
        commodities: hold.iter().map(|stack| stack.symbol.clone()).collect(),
        allow_illegal: config.include_illegal,
    }
}

/// Run the command.
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence, and the order is the safety contract: the hold, the free index, the filters, the priced gate, the live reads, then the plan"
)]
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs, T: Timer>(
    app: &App<'_, H, C, E, F>,
    config: &SellConfig,
    commander: Option<&CommanderState>,
    timer: &T,
) -> CmdResult {
    let out = app.out;
    if app.cli.switch_value(edm_core::cli::Flag::Json, false).unwrap_or(false) {
        out.stdout_is_a_document();
    }
    let note = |text: String| out.line(&text);

    // --- the hold, from files -------------------------------------------
    let Some(state) = commander else {
        return Err("no Elite Dangerous journal was found, so there is no hold to plan. \
             Set EDM_JOURNAL_DIR to the directory holding Cargo.json — this command sells what \
             you are carrying and cannot invent it."
            .to_owned());
    };
    if state.cargo.inventory.is_none() {
        return Err("the journal was read but Cargo.json was not, so what you are carrying is \
             unknown. It is written beside the journal; --no-cache does not affect it."
            .to_owned());
    }
    let (hold, excluded) = manifest(state, &config.items);
    for entry in &excluded {
        note(format!(
            "   excluding {} t of {}: {}",
            js::format_integer(entry.tons as f64),
            entry.symbol,
            entry.reason,
        ));
    }
    if hold.is_empty() {
        return Err(if config.items.is_empty() {
            "the hold is empty; there is nothing to sell".to_owned()
        } else {
            format!(
                "none of {} is aboard. Carrying: {}",
                config.items.join(", "),
                describe_hold(state),
            )
        });
    }
    note(format!(
        "planning the sale of {}",
        hold.iter()
            .map(|stack| format!(
                "{} t of {}",
                js::format_integer(stack.tons as f64),
                stack.symbol
            ))
            .collect::<Vec<_>>()
            .join(", "),
    ));

    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);
    let here = state
        .current_system
        .as_ref()
        .map(|located| located.value.name.clone());
    let centre_name = config
        .origin
        .clone()
        .or(here)
        .ok_or_else(|| {
            "the journal does not say where you are. Pass --from <system>.".to_owned()
        })?;
    let centre = ardent
        .resolve_location(&centre_name, ardent::Lookup::Auto)
        .await?
        .system;
    let origin = Coordinates {
        x: centre.coordinates.x,
        y: centre.coordinates.y,
        z: centre.coordinates.z,
    };

    // --- who buys it, free ------------------------------------------------
    note(format!(
        "asking Ardent who buys it within {} Ly of {}...",
        js::js_number(config.radius_ly),
        centre.name,
    ));
    let mut rows: Vec<CommodityPrice> = Vec::new();
    for stack in &hold {
        let nearby = ardent
            .commodity_nearby(
                &centre.name,
                &stack.symbol,
                CommodityDirection::Imports,
                config.radius_ly,
                true,
                config.min_demand,
            )
            .await
            .map_err(|error| format!("asking Ardent who buys {}: {error}", stack.symbol))?;
        // `/nearby` omits its own centre, and a zero-Ly buyer is the one most
        // likely to win a "is the flight worth it" comparison — so the hole
        // matters more here than it does for a route.
        let (_, local) = ardent
            .commodity_in_system(&centre.name, &stack.symbol)
            .await
            .map_err(|error| {
                format!("asking Ardent about {} here: {error}", stack.symbol)
            })?;
        rows.extend(nearby);
        rows.extend(local);
    }

    // --- the ordinary station filters, free -------------------------------
    let route_config = sell_route_config(app, config)?;
    let mut seen = std::collections::HashSet::new();
    let stations: Vec<ardent::ArdentStation> = rows
        .iter()
        .filter(|row| row.direction == CommodityDirection::Imports)
        .filter(|row| seen.insert(row.station.market_id.to_bits()))
        .map(|row| row.station.clone())
        .collect();
    let considered = stations.len();
    let mut selection = select::select(stations, &route_config, &centre.coordinates);
    if selection.keep.is_empty() {
        return Err(format!(
            "no buyer within {} Ly survives the station filters. Widen --radius, or relax --pad \
             and --max-star-distance.",
            js::js_number(config.radius_ly),
        ));
    }
    // Keep the best-priced buyers per commodity, so the search bound is the one
    // `--top` states rather than however many rows Ardent happened to return.
    let keep = best_per_commodity(&rows, &selection.keep, config.top);
    selection.keep.retain(|station| keep.contains(&station.market_id.to_bits()));

    // --- the priced gate ---------------------------------------------------
    let read_cache = crate::cmd::route::cache_for(app, &route_config);
    let write_cache = Cache::new(
        read_cache.root().to_path_buf(),
        route_config.max_age_minutes,
        route_config.cache,
        true,
    );
    let prepared = acquire::prepare(
        &read_cache,
        &app.ports.fs,
        &selection.keep,
        app.ports.clock.now_ms(),
    );
    let estimate = Estimate::build(
        Counts {
            systems: 0,
            systems_to_read: 0,
            stations_known: considered,
            markets_to_poll: selection.keep.len(),
            cached_fresh: prepared.hits.fresh,
            carriers_to_probe: 0,
        },
        selection.exclusions.clone(),
        route_config.rate_per_second,
        &SizePrior::default(),
    );
    out.aside(&[Block::Table {
        title: "SELL PLAN COST".to_owned(),
        columns: columns::ROUTE_FIELD_COLUMNS,
        rows: vec![
            field("buyers considered", js::format_integer(considered as f64)),
            field("buyers to read", js::format_integer(estimate.markets_to_poll as f64)),
            field("cached and still fresh", js::format_integer(estimate.cached_fresh as f64)),
            field("game-internal API requests", js::format_integer(estimate.requests)),
            field("ceiling", format!(
                "{} of {}   (--max-requests to raise)",
                js::format_integer(estimate.requests),
                js::format_integer(route_config.max_requests),
            )),
        ],
    }]);
    if estimate.requests > route_config.max_requests {
        out.set_exit(crate::out::EXIT_FAILURE);
        return Err(format!(
            "reading {} buyers is above the {} ceiling. Narrow with --radius or --top, or raise \
             it with --max-requests. Nothing has been sent.",
            js::format_integer(estimate.requests),
            js::format_integer(route_config.max_requests),
        ));
    }
    if !route_config.confirmed && estimate.requests > edm_core::spend::CONFIRM_THRESHOLD {
        out.line(&edm_core::spend::confirmation_message(&estimate));
        out.set_exit(crate::out::EXIT_FAILURE);
        return Ok(());
    }
    if route_config.dry_run {
        return Ok(());
    }

    // --- the live reads ----------------------------------------------------
    let stamp_overrides = app.stamp_overrides()?;
    let entropy = crate::ports::PinnedJitter {
        inner: &app.ports.entropy,
        unit: app.overrides.jitter.unwrap_or(f64::NAN),
    };
    let pacer = Pacer::new(
        crate::cmd::route::pacing(&route_config),
        &app.ports.clock,
        timer,
        &entropy,
    );
    let relay_tally = std::cell::RefCell::new(crate::route::relay::Tally::default());
    let query = edm_core::cli::config::starsystem_query(
        &app.cli,
        edm_core::cli::config::CachedTimestamp::SweepZero,
    )
    .map_err(|error| error.message().to_owned())?;
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
        cache: &write_cache,
        relayed: &relay_tally,
        eddn: None,
        workers: route_config.workers as usize,
        quiet: route_config.quiet,
        verify_systems: false,
        language: &query.language,
        report: None,
        trace: None,
        total: selection.keep.len(),
    };
    let to_read = prepared.to_poll.len();
    if to_read > 0 {
        note(format!(
            "reading {} {} live...",
            js::format_integer(to_read as f64),
            if to_read == 1 { "buyer" } else { "buyers" },
        ));
    }
    let acquired = acquire::sweep(&cx, &pacer, prepared, &[]).await;

    // --- the plan ----------------------------------------------------------
    let (markets, commodities, _) = crate::route::ingest::markets_from_listings(
        &acquired.listings,
        &selection.keep,
        &sell_floors(&route_config, &hold),
        &HashMap::new(),
    );
    if markets.is_empty() {
        return Err("no buyer answered with a usable listing".to_owned());
    }
    let held: Vec<Held> = hold
        .iter()
        .filter_map(|stack| {
            let id = commodities.id_of_symbol(&stack.symbol)?;
            Some(Held {
                commodity: id,
                tons: Tons(stack.tons),
            })
        })
        .collect();
    if held.is_empty() {
        return Err("none of the buyers that answered are buying what you are carrying".to_owned());
    }

    let geometry = Geometry::new(&markets, crate::cmd::route::time_model(&route_config));
    let candidates: Vec<u32> = (0..markets.len() as u32).collect();
    let bar = worth_bar(&geometry, origin, &held, &candidates, config);
    let plans = edm_route::sell::plans(
        &geometry,
        origin,
        &held,
        &candidates,
        config.stops,
        bar,
    )
    .map_err(|error| {
        format!(
            "{} buyers at --stops {} is {} orderings, past what this will enumerate. \
             Lower --stops, --top or --radius.",
            js::format_integer(error.candidates as f64),
            js::format_integer(error.stops as f64),
            js::format_integer(error.paths as f64),
        )
    })?;
    if plans.is_empty() {
        return Err("no buyer will take any of it".to_owned());
    }

    // **Nothing is presented on a price this run did not read.** The plan is a
    // handful of markets out of hundreds nominated, so verifying exactly those
    // is cheap — and demand is the only thing capping revenue here, which is
    // the one field a cache cannot be trusted on \[C38\]. Re-read them through
    // a refresh-mode cache, because the entries the sweep just wrote would
    // otherwise answer for themselves.
    let shown: Vec<f64> = plans
        .iter()
        .take(2)
        .flat_map(|plan| plan.market_ids(&markets))
        .map(|id| id as f64)
        .collect();
    let stale: Vec<&ardent::ArdentStation> = selection
        .keep
        .iter()
        .filter(|station| shown.contains(&station.market_id))
        .filter(|station| {
            acquired
                .listings
                .iter()
                .find(|listing| listing.market_id == station.market_id)
                .is_some_and(|listing| listing.from_cache)
        })
        .collect();

    let (markets, commodities, plans, verified) = if stale.is_empty() {
        (markets, commodities, plans, 0)
    } else {
        note(format!(
            "verifying the {} {} the plan uses...",
            js::format_integer(stale.len() as f64),
            if stale.len() == 1 { "buyer" } else { "buyers" },
        ));
        let owned: Vec<ardent::ArdentStation> = stale.into_iter().cloned().collect();
        let count = owned.len();
        let fresh = acquire::sweep(
            &acquire::Cx {
                cache: &write_cache,
                ..cx
            },
            &pacer,
            acquire::prepare(&write_cache, &app.ports.fs, &owned, app.ports.clock.now_ms()),
            &[],
        )
        .await;
        // Fold the fresh reads over the seed listings and rebuild, so the plan
        // and the alternatives are both priced on what was just measured.
        let mut listings = acquired.listings;
        for listing in fresh.listings {
            match listings
                .iter_mut()
                .find(|held| held.market_id == listing.market_id)
            {
                Some(held) => *held = listing,
                None => listings.push(listing),
            }
        }
        let (markets, commodities, _) = crate::route::ingest::markets_from_listings(
            &listings,
            &selection.keep,
            &sell_floors(&route_config, &hold),
            &HashMap::new(),
        );
        let held: Vec<Held> = hold
            .iter()
            .filter_map(|stack| {
                Some(Held {
                    commodity: commodities.id_of_symbol(&stack.symbol)?,
                    tons: Tons(stack.tons),
                })
            })
            .collect();
        let geometry = Geometry::new(&markets, crate::cmd::route::time_model(&route_config));
        let candidates: Vec<u32> = (0..markets.len() as u32).collect();
        let plans = edm_route::sell::plans(&geometry, origin, &held, &candidates, config.stops, bar)
            .unwrap_or_default();
        (markets, commodities, plans, count)
    };

    let Some(best) = plans.first() else {
        return Err("no buyer will take any of it once its live prices are read".to_owned());
    };
    if verified > 0 {
        note(format!(
            "read {} {} live; the plan below is priced on that",
            js::format_integer(verified as f64),
            if verified == 1 { "buyer" } else { "buyers" },
        ));
    }

    let geometry = Geometry::new(&markets, crate::cmd::route::time_model(&route_config));
    render(out, best, &plans, &markets, &commodities, &geometry, origin, bar);
    Ok(())
}

fn field(label: &str, value: String) -> Row<'static> {
    Row::Data(vec![
        std::borrow::Cow::Owned(label.to_owned()),
        std::borrow::Cow::Owned(value),
    ])
}

fn describe_hold(state: &CommanderState) -> String {
    state
        .cargo
        .inventory
        .as_ref()
        .map(|held| {
            held.value
                .iter()
                .map(|item| {
                    format!(
                        "{} t of {}",
                        js::format_integer(item.count as f64),
                        ardent::normalise_commodity_name(&item.name)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "nothing".to_owned())
}

/// The best-priced `top` buyers per commodity, by market id bits.
fn best_per_commodity(
    rows: &[CommodityPrice],
    keep: &[ardent::ArdentStation],
    top: usize,
) -> std::collections::HashSet<u64> {
    let allowed: std::collections::HashSet<u64> =
        keep.iter().map(|s| s.market_id.to_bits()).collect();
    let mut by_commodity: HashMap<String, Vec<(f64, u64)>> = HashMap::new();
    for row in rows {
        if row.direction != CommodityDirection::Imports {
            continue;
        }
        let bits = row.station.market_id.to_bits();
        if !allowed.contains(&bits) {
            continue;
        }
        by_commodity
            .entry(ardent::normalise_commodity_name(&row.commodity_name))
            .or_default()
            .push((row.price, bits));
    }
    let mut chosen = std::collections::HashSet::new();
    for (_, mut offers) in by_commodity {
        offers.sort_by(|a, b| b.0.total_cmp(&a.0));
        offers.dedup_by_key(|(_, bits)| *bits);
        for (_, bits) in offers.into_iter().take(top) {
            chosen.insert(bits);
        }
    }
    chosen
}

/// The bar a further stop must clear.
///
/// `--worth` when given. Otherwise the rate of the best single stop **that
/// clears the hold** — deliberately not the best single stop by rate, which is
/// usually a partial sale and would set the bar to something nothing else can
/// beat, collapsing the objective back onto the rate it exists to avoid.
fn worth_bar(
    geometry: &Geometry<'_>,
    origin: Coordinates,
    held: &[Held],
    candidates: &[u32],
    config: &SellConfig,
) -> Ratio {
    if let Some(per_hour) = config.worth {
        return Ratio::new(Credits(per_hour as i64), Millis(3_600_000));
    }
    let total: i64 = held.iter().map(|item| item.tons.0).sum();
    let singles = edm_route::sell::plans(
        geometry,
        origin,
        held,
        candidates,
        1,
        Ratio::new(Credits(1), Millis(3_600_000)),
    )
    .unwrap_or_default();
    let clearing = singles
        .iter()
        .filter(|plan| plan.sold.0 >= total)
        .max_by_key(|plan| plan.rate().credits_per_hour_floor());
    let fallback = singles.iter().max_by_key(|plan| plan.revenue.0);
    clearing
        .or(fallback)
        .map_or(Ratio::new(Credits(0), Millis(3_600_000)), Plan::rate)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the plan, its alternatives, the instance they address, and where the ship is"
)]
fn render(
    out: &crate::out::Out,
    best: &Plan,
    plans: &[Plan],
    markets: &[Market],
    commodities: &edm_route::model::Commodities,
    geometry: &Geometry<'_>,
    origin: Coordinates,
    bar: Ratio,
) {
    let name = |id: CommodityId| {
        commodities
            .name(id)
            .map_or_else(|| "?".to_owned(), edm_route::view::readable)
    };
    let int = js::format_integer;

    let mut rows = Vec::new();
    let mut at = origin;
    for (nth, stop) in best.stops.iter().enumerate() {
        let market = &markets[stop.market as usize];
        let ly = geometry.ly_from(at, stop.market);
        at = market.coords;
        for (row, drop) in stop.drops.iter().enumerate() {
            rows.push(Row::Data(vec![
                (if row == 0 { int((nth + 1) as f64) } else { String::new() }).into(),
                (if row == 0 {
                    format!("{} ({})", market.station, market.system)
                } else {
                    String::new()
                })
                .into(),
                (if row == 0 { js::to_fixed_1(ly) } else { String::new() }).into(),
                name(drop.commodity).into(),
                int(drop.tons.0 as f64).into(),
                int(drop.unit_price.0 as f64).into(),
                format!("{} cr", int(drop.credits.0 as f64)).into(),
            ]));
        }
    }
    out.emit(&[Block::Table {
        title: "SELL PLAN".to_owned(),
        columns: columns::SELL_COLUMNS,
        rows,
    }]);
    out.emit(&[Block::Note(format!(
        "sells {} t for {} cr over {}, at {} {}",
        int(best.sold.0 as f64),
        int(best.revenue.0 as f64),
        edm_core::spend::duration_estimate(best.millis.0 as f64 / 1_000.0),
        int(best.stops.len() as f64),
        if best.stops.len() == 1 { "stop" } else { "stops" },
    ))]);
    for left in &best.unsold {
        out.emit(&[Block::Note(format!(
            "leaves {} t of {} aboard: no chosen buyer will take it",
            int(left.tons.0 as f64),
            name(left.commodity),
        ))]);
    }

    // The alternatives, so the refusal is arithmetic rather than assertion.
    // Ties break toward *less* flying. Without that, a plan matching the
    // recommendation's revenue but taking longer wins `max_by_key` and the row
    // reads "most credits ... 0 cr/h", which is a slower way to earn the same
    // money presented as an alternative worth considering.
    let most_credits = plans
        .iter()
        .max_by_key(|plan| (plan.revenue.0, -plan.millis.0))
        .filter(|plan| plan.revenue.0 > best.revenue.0);
    let mut alt = Vec::new();
    for (label, plan) in [("recommended", Some(best)), ("most credits", most_credits)] {
        let Some(plan) = plan else { continue };
        let marginal = if std::ptr::eq(plan, best) {
            "-".to_owned()
        } else {
            let extra = plan.revenue.0 - best.revenue.0;
            let longer = plan.millis.0 - best.millis.0;
            if longer <= 0 {
                "-".to_owned()
            } else {
                format!(
                    "{} cr/h",
                    int(Ratio::new(Credits(extra), Millis(longer)).credits_per_hour_floor() as f64)
                )
            }
        };
        alt.push(Row::Data(vec![
            label.into(),
            int(plan.stops.len() as f64).into(),
            int(plan.sold.0 as f64).into(),
            format!("{} cr", int(plan.revenue.0 as f64)).into(),
            edm_core::spend::duration_estimate(plan.millis.0 as f64 / 1_000.0).into(),
            format!("{}/h", int(plan.rate().credits_per_hour_floor() as f64)).into(),
            marginal.into(),
        ]));
    }
    if alt.len() > 1 {
        out.emit(&[Block::Table {
            title: "WHAT ELSE YOU COULD DO".to_owned(),
            columns: columns::SELL_ALTERNATIVES_COLUMNS,
            rows: alt,
        }]);
        out.emit(&[Block::Note(format!(
            "your bar is {} cr/h (--worth); an extra stop is taken only when it beats that",
            int(bar.credits_per_hour_floor() as f64),
        ))]);
    }

    out.emit(&[Block::Heading("TRADE COMMANDS".to_owned())]);
    for stop in &best.stops {
        let market = &markets[stop.market as usize];
        for drop in &stop.drops {
            out.line(&format!(
                "  at {:<28} edm trade --market-id {} --type sell --item {} --qty {}",
                market.station,
                js::js_number(market.market_id as f64),
                name(drop.commodity).replace(' ', ""),
                js::js_number(drop.tons.0 as f64),
            ));
        }
    }
}
