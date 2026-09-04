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
//!
//! `--follow` repeats the last three phases on an interval \[C52\]: the
//! journal is re-read for the hold and the ship's position, every candidate
//! buyer is re-read live, and the plan is solved again from what is left. The
//! buyer set is the one the first nomination produced; a round never asks
//! Ardent again, which is what keeps it a sweep rather than a search.

use std::collections::{HashMap, HashSet};

use edm_core::ardent::{self, CommodityDirection, CommodityPrice};
use edm_core::cli::config::RouteConfig;
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
use crate::route::access;
use crate::route::acquire;
use crate::route::cache::Cache;
use crate::route::follow::FollowState;
use crate::route::pacer::Pacer;

/// One stack of clean cargo, as the journal spells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Stack {
    /// Frontier's own symbol, e.g. `tritium`.
    pub(crate) symbol: String,
    pub(crate) tons: i64,
}

/// What was left out of the manifest, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Excluded {
    pub(crate) symbol: String,
    pub(crate) tons: i64,
    pub(crate) reason: &'static str,
}

/// Read the hold, excluding what cannot honestly be planned.
///
/// **Stolen tons and mission cargo are excluded and named, never guessed at.**
/// A stolen ton needs a black market even when the commodity is legal
/// everywhere — `derive_black_market` is `stolen || illegal`, and the two are
/// independent — so a station answers HTTP 401 for it. It also cannot be
/// *nominated*: Ardent publishes one price per row and it is the open-market
/// one, its `/markets` rows carry no `blackMarket` flag, and `RawCommodity`
/// drops the fence price that the live payload does carry. So the search has
/// no way to find a fence and no way to rank one. A plan that included them
/// would be a plan that fails at the counter.
///
/// Not a permanent limit: `/system/name/{s}/stations` publishes `blackMarket`
/// for free, and `Commodity::fence_price` is already read from every market —
/// `edm market` prints it. Measured first, though: over 317,599 illegal rows
/// the fence pays the open-market price in 66.2% of them and a median 0.02%
/// more in the rest, so the value of building this is the access, never a
/// better price.
pub(crate) fn manifest(state: &CommanderState, items: &[String]) -> (Vec<Stack>, Vec<Excluded>) {
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
                reason: "stolen; needs a black market, which nothing indexes yet",
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
pub(crate) fn sell_route_config<H, C, E, F>(
    app: &App<'_, H, C, E, F>,
    config: &SellConfig,
) -> Result<RouteConfig, String> {
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
pub(crate) fn sell_floors(config: &RouteConfig, hold: &[Stack]) -> edm_route::model::RowFloors {
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
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs, T: Timer, G: crate::route::plan::Gate>(
    app: &App<'_, H, C, E, F>,
    config: &SellConfig,
    commander: Option<&CommanderState>,
    timer: &T,
    gate: &G,
) -> CmdResult {
    let out = app.out;
    let json = app.cli.switch_value(edm_core::cli::Flag::Json, false).unwrap_or(false);
    // Refused, never ignored, for route's reason \[C43\]: a document is one
    // well-formed document or nothing, and a loop emits one per round.
    if config.follow_seconds.is_some() && json {
        return Err("--follow cannot be combined with --json: sell's document is one well-formed \
                    document or nothing, and a loop emits one per round. Run without --json to \
                    watch, or without --follow to capture"
            .to_owned());
    }
    if json {
        out.stdout_is_a_document();
    }
    let note = |text: String| out.line(&text);
    // One pacer for the run, as route's; the search opens its deadline window
    // where its first paid request can happen.
    let entropy = crate::ports::PinnedJitter {
        inner: &app.ports.entropy,
        unit: app.overrides.jitter.unwrap_or(f64::NAN),
    };
    let route_config = sell_route_config(app, config)?;
    let pacer = Pacer::new(
        crate::cmd::route::pacing(&route_config),
        &app.ports.clock,
        timer,
        &entropy,
    );
    let Some(mut found) = search(app, config, commander, timer, &pacer, &entropy, gate).await? else {
        return Ok(());
    };
    present(out, &found.solved, &found.route_config, found.origin);

    // --- --follow: keep re-planning until the hold is empty \[C52\] --------
    //
    // A round is the last three phases again: the journal for the hold and the
    // position, every candidate buyer live, then the solve. The buyer set is
    // fixed at what the first nomination produced, so a round costs one sweep
    // of `--top` buyers per commodity and never a search — and it cannot plan
    // cargo taken aboard since, which is why that is named rather than dropped.
    let Some(interval) = config.follow_seconds else {
        return Ok(());
    };
    let mut follow = FollowState::default();
    loop {
        if let Some(text) = follow.round_cap(config.follow_rounds) {
            note(text);
            break;
        }
        if let Some(text) = follow.ceiling(pacer.spent().requests, found.route_config.max_requests) {
            note(text);
            break;
        }
        timer.sleep_ms(interval * 1_000.0).await;
        follow.begin_round();
        let outcome = round(app, timer, &pacer, &entropy, &mut found).await?;
        for stack in &outcome.newly_unplanned {
            note(format!(
                "   {} t of {} is aboard but not in this plan: its buyers were never \
                 nominated. Re-run to plan it",
                js::format_integer(stack.tons as f64),
                stack.symbol,
            ));
        }
        if outcome.sold_out {
            note(if config.items.is_empty() {
                "the hold is empty: everything is sold".to_owned()
            } else {
                format!(
                    "none of {} is aboard any more: sold",
                    config.items.join(", ")
                )
            });
            break;
        }
        if let Some(changed) = &outcome.hold_changed {
            note(format!("the hold is now {changed}"));
        }
        note(format!(
            "round {}: {} of {} buyers re-read, {} requests, {} aboard{}",
            js::format_integer(follow.round as f64),
            js::format_integer(outcome.read as f64),
            js::format_integer(found.keep.len() as f64),
            js::format_integer(outcome.requests as f64),
            describe_stacks(&found.hold),
            if outcome.tripped {
                " (the rate limiter tripped this round)"
            } else {
                ""
            },
        ));
        match outcome.plan {
            Ok(()) => {
                follow.record(true);
                present(out, &found.solved, &found.route_config, found.origin);
            }
            Err(reason) => {
                note(reason);
                // A round re-reads a fixed buyer set and never re-nominates,
                // so when every buyer is gone no later round can recover it
                // \[C46\].
                if let Some(barren_rounds) = follow.record(false) {
                    note(format!(
                        "nothing could be planned for {} rounds: every nominated buyer has gone, \
                         and --follow re-reads the buyers it was given rather than nominating new \
                         ones. Re-run to search again",
                        js::format_integer(barren_rounds as f64),
                    ));
                    break;
                }
            }
        }
    }
    Ok(())
}

/// What a disposal search established, kept whole so a round — or a
/// full-screen UI — can plan it again \[C52\], \[C53\].
pub(crate) struct SellSearch {
    /// The current plan. A round that could not plan leaves the previous one
    /// here, so there is always something to show.
    pub solved: Solved,
    /// What is aboard of the commodities the buyers were nominated for.
    pub hold: Vec<Stack>,
    /// The commodities the buyer set was nominated for.
    pub nominated: HashSet<String>,
    /// Cargo already named as outside the plan, so it is said once.
    pub named_unplanned: HashSet<String>,
    pub origin: Coordinates,
    /// `--from` pins the origin; without it the origin follows the ship.
    pub origin_pinned: bool,
    /// Every nominated buyer, which is every market a round re-reads.
    pub keep: Vec<ardent::ArdentStation>,
    pub route_config: RouteConfig,
    /// `--item`, so a round knows which stacks it was asked about.
    pub items: Vec<String>,
    pub config: SellConfig,
}

/// What one round did.
#[derive(Debug)]
pub(crate) struct SellRound {
    /// Nothing planned is aboard any more: the loop's job is done.
    pub sold_out: bool,
    /// Cargo first seen aboard this round that no buyer was nominated for.
    pub newly_unplanned: Vec<Stack>,
    /// The hold as described now, when it changed this round.
    pub hold_changed: Option<String>,
    /// Buyers that answered.
    pub read: usize,
    pub requests: usize,
    pub tripped: bool,
    /// Whether a plan was solved; the reason when not.
    pub plan: Result<(), String>,
}

/// One round: the journal for the hold and the ship, every nominated buyer
/// live, then the plan again \[C52\].
pub(crate) async fn round<H: HttpTransport, C: Clock, E: Entropy, F: Fs, T: Timer>(
    app: &App<'_, H, C, E, F>,
    timer: &T,
    pacer: &Pacer<'_, C, T, crate::ports::PinnedJitter<'_, E>>,
    entropy: &crate::ports::PinnedJitter<'_, E>,
    found: &mut SellSearch,
) -> Result<SellRound, String> {
    let out = app.out;
    let before = pacer.spent();
    // A fresh deadline window and a cleared breaker latch, as route's
    // loop does: `--deadline` bounds one round, not the session.
    pacer.begin_round();

    // --- the journal again: what is left, and where the ship is --------
    //
    // Quietly. The malformed-observation warning is a property of the
    // file, already said once \[C49\].
    let mut newly_unplanned = Vec::new();
    let mut hold_changed = None;
    if let Some(state) = crate::cmd::reload_commander_state(&app.cli, app.ports) {
        if state.cargo.inventory.is_some() {
            let (aboard, _) = manifest(&state, &found.items);
            let (planned, unplanned) = split_hold(aboard, &found.nominated);
            for stack in unplanned {
                if found.named_unplanned.insert(stack.symbol.clone()) {
                    newly_unplanned.push(stack);
                }
            }
            if planned.is_empty() {
                return Ok(SellRound {
                    sold_out: true,
                    newly_unplanned,
                    hold_changed: None,
                    read: 0,
                    requests: 0,
                    tripped: false,
                    plan: Ok(()),
                });
            }
            if planned != found.hold {
                hold_changed = Some(describe_stacks(&planned));
                found.hold = planned;
            }
        }
        // `--from` pins the origin; without it the plan starts from
        // wherever the ship is now, so the first stop's distance is the
        // flight ahead rather than the one already flown.
        if !found.origin_pinned
            && let Some(xyz) = state
                .current_system
                .as_ref()
                .and_then(|seen| seen.value.coordinates)
        {
            found.origin = Coordinates {
                x: xyz[0],
                y: xyz[1],
                z: xyz[2],
            };
        }
    }

    // --- every candidate buyer, live ------------------------------------
    //
    // Through the refresh-mode cache, so the entries the previous round
    // wrote cannot answer for this one.
    let route_config = &found.route_config;
    let stamp_overrides = app.stamp_overrides()?;
    let query = edm_core::cli::config::starsystem_query(
        &app.cli,
        edm_core::cli::config::CachedTimestamp::SweepZero,
    )
    .map_err(|error| error.message().to_owned())?;
    let read_cache = crate::cmd::route::cache_for(app, route_config);
    let write_cache = Cache::new(
        read_cache.root().to_path_buf(),
        route_config.max_age_minutes,
        route_config.cache,
        true,
    );
    let relay_tally = std::cell::RefCell::new(crate::route::relay::Tally::default());
    let cx = acquire::Cx {
        http: app.http,
        clock: &app.ports.clock,
        timer,
        entropy,
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
        total: found.keep.len(),
    };
    let acquired = acquire::sweep(
        &cx,
        pacer,
        acquire::prepare(
            &write_cache,
            &app.ports.fs,
            &found.keep,
            app.ports.clock.now_ms(),
        ),
        &[],
    )
    .await;
    let spent = pacer.spent();

    // --- the plan again ------------------------------------------------
    //
    // The bar is derived afresh: every price here was read this round,
    // and the hold it clears is the one aboard now.
    let plan = match solve(
        &acquired.listings,
        &found.keep,
        route_config,
        &found.hold,
        found.origin,
        &found.config,
        None,
    ) {
        Ok(solved) => {
            found.solved = solved;
            Ok(())
        }
        Err(reason) => Err(reason),
    };
    Ok(SellRound {
        sold_out: false,
        newly_unplanned,
        hold_changed,
        read: acquired.listings.len(),
        requests: spent.requests - before.requests,
        tripped: pacer.tripped().is_some(),
        plan,
    })
}

/// The search itself: the hold, who buys it, the filters, the priced gate,
/// the live reads, the plan. Prints its progress; the plan is returned rather
/// than printed, and `None` means the run ended at the gate.
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence, and the order is the safety contract: the hold, the free index, the filters, the priced gate, the live reads, then the plan"
)]
pub(crate) async fn search<H, C, E, F, T, G>(
    app: &App<'_, H, C, E, F>,
    config: &SellConfig,
    commander: Option<&CommanderState>,
    timer: &T,
    pacer: &Pacer<'_, C, T, crate::ports::PinnedJitter<'_, E>>,
    entropy: &crate::ports::PinnedJitter<'_, E>,
    gate: &G,
) -> Result<Option<SellSearch>, String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
    T: Timer,
    G: crate::route::plan::Gate,
{
    let out = app.out;
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
    note(format!("planning the sale of {}", describe_stacks(&hold)));

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

    // --- the carriers among them, priced ----------------------------------
    //
    // Two questions in one request, and a disposal needs both. **Can I dock?**
    // — a squadron-only carrier is not a buyer. **Is it still there?** — a
    // carrier's market answers by id from anywhere, so its live price says
    // nothing about its position, and Ardent only learns of a jump when
    // somebody reports the carrier at its new home. Skipping this sent a
    // commander 170 Ly to an empty orbit for a `squadronfriends` carrier that
    // had been in another system for three days \[C42\].
    let carrier_ids: Vec<f64> = selection
        .keep
        .iter()
        .filter(|station| ardent::is_carrier(station.station_type.as_deref()))
        .map(|station| station.market_id)
        .collect();

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
    let notoriety = state.notoriety;
    let access_cache = access::CachePolicy {
        enabled: route_config.cache,
        refresh: route_config.refresh,
        max_age_minutes: Some(route_config.max_age_minutes),
    };
    let mut carriers = access::prepare(
        &app.ports.fs,
        read_cache.root(),
        &carrier_ids,
        app.ports.clock.now_ms(),
        access_cache,
        notoriety,
    );
    // Both classes are priced in one gate, before either is sent. The market
    // count is the pre-filter one, so the ceiling is checked against more
    // requests than the run will make once closed and departed carriers drop
    // out — over-pricing is the safe direction for a ceiling.
    let estimate = Estimate::build(
        Counts {
            systems: 0,
            systems_to_read: 0,
            stations_known: considered,
            markets_to_poll: selection.keep.len(),
            cached_fresh: prepared.hits.fresh,
            carriers_to_probe: carriers.cold.len(),
        },
        selection.exclusions.clone(),
        route_config.rate_per_second,
        &SizePrior::default(),
    );
    out.aside(&sell_gate_blocks(
        &estimate,
        considered,
        carriers.cold.len(),
        route_config.max_requests,
    ));
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
        let gated = crate::route::plan::Gated {
            estimate: estimate.clone(),
            verdict: edm_core::spend::Verdict::NeedsConfirmation,
            plan: sell_gate_blocks(&estimate, considered, carriers.cold.len(), route_config.max_requests),
        };
        if !gate.confirm(out, &gated).await {
            return Ok(None);
        }
    }
    if route_config.dry_run {
        return Ok(None);
    }

    // --- the live reads ----------------------------------------------------
    let stamp_overrides = app.stamp_overrides()?;
    pacer.begin_round();
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
        entropy,
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
    // Docking access *and* position, from the same reply \[C42\]. A carrier
    // that has jumped is dropped whatever its door says: its market answers by
    // id from anywhere, so a live price is no evidence at all about where the
    // ship would have to fly.
    if !carriers.cold.is_empty() {
        note(format!(
            "checking {} fleet {} for docking access and position...",
            js::format_integer(carriers.cold.len() as f64),
            if carriers.cold.len() == 1 { "carrier" } else { "carriers" },
        ));
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
        let cold = std::mem::take(&mut carriers.cold);
        access::probe(
            &probe_cx,
            pacer,
            &app.ports.fs,
            read_cache.root(),
            &cold,
            app.ports.clock.now_ms(),
            access_cache,
            notoriety,
            &mut carriers.index,
            &mut carriers.cost,
            None,
        )
        .await
        .map_err(|error| {
            format!("{error}\n   pass --carrier-access any to rank carriers without checking")
        })?;
    }
    if !carrier_ids.is_empty() {
        access::finish(
            &mut carriers.index,
            &carrier_ids,
            commander,
            &mut carriers.cost,
        );
        let dropped = access::apply(&mut selection, &carriers.index, route_config.carrier_access);
        if dropped.total() > 0 {
            note(access::note(carriers.cost, dropped));
        }
        if selection.keep.is_empty() {
            return Err("every buyer was a carrier you cannot reach or dock at".to_owned());
        }
    }
    // Re-prepared over what survived, so nothing is read for a market the
    // carrier filter has already removed.
    let prepared = acquire::prepare(
        &read_cache,
        &app.ports.fs,
        &selection.keep,
        app.ports.clock.now_ms(),
    );
    let acquired = acquire::sweep(&cx, pacer, prepared, &[]).await;

    // --- the plan ----------------------------------------------------------
    let solved = solve(
        &acquired.listings,
        &selection.keep,
        &route_config,
        &hold,
        origin,
        config,
        None,
    )?;

    // **Nothing is presented on a price this run did not read.** The plan is a
    // handful of markets out of hundreds nominated, so verifying exactly those
    // is cheap — and demand is the only thing capping revenue here, which is
    // the one field a cache cannot be trusted on \[C38\]. Re-read them through
    // a refresh-mode cache, because the entries the sweep just wrote would
    // otherwise answer for themselves.
    let shown: Vec<f64> = solved
        .plans
        .iter()
        .take(2)
        .flat_map(|plan| plan.market_ids(&solved.markets))
        .map(|id| id as f64)
        .collect();
    let stale: Vec<ardent::ArdentStation> = selection
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
        .cloned()
        .collect();

    let mut listings = acquired.listings;
    let (solved, verified) = if stale.is_empty() {
        (solved, 0)
    } else {
        note(format!(
            "verifying the {} {} the plan uses...",
            js::format_integer(stale.len() as f64),
            if stale.len() == 1 { "buyer" } else { "buyers" },
        ));
        let fresh = acquire::sweep(
            &cx,
            pacer,
            acquire::prepare(&write_cache, &app.ports.fs, &stale, app.ports.clock.now_ms()),
            &[],
        )
        .await;
        // Fold the fresh reads over the seed listings and rebuild, so the plan
        // and the alternatives are both priced on what was just measured. The
        // bar is the one the seed prices set: it is the commander's standard
        // for a further stop, not a fact about any one buyer.
        fold_listings(&mut listings, fresh.listings);
        let solved = solve(
            &listings,
            &selection.keep,
            &route_config,
            &hold,
            origin,
            config,
            Some(solved.bar),
        )
        .map_err(|_| "no buyer will take any of it once its live prices are read".to_owned())?;
        (solved, stale.len())
    };
    if verified > 0 {
        note(format!(
            "read {} {} live; the plan below is priced on that",
            js::format_integer(verified as f64),
            if verified == 1 { "buyer" } else { "buyers" },
        ));
    }
    // The commodities the buyer set was nominated for.
    let nominated: HashSet<String> = hold.iter().map(|stack| stack.symbol.clone()).collect();
    Ok(Some(SellSearch {
        solved,
        hold,
        nominated,
        named_unplanned: HashSet::new(),
        origin,
        origin_pinned: config.origin.is_some(),
        keep: selection.keep,
        route_config,
        items: config.items.clone(),
        config: config.clone(),
    }))
}

/// A plan and everything it is priced on, kept whole so the verify pass and a
/// follow round can rebuild it from fresh listings.
pub(crate) struct Solved {
    pub(crate) markets: Vec<Market>,
    pub(crate) commodities: edm_route::model::Commodities,
    /// Never empty: an instance with nothing to sell is an `Err` from [`solve`].
    pub(crate) plans: Vec<Plan>,
    pub(crate) bar: Ratio,
}

/// Ingest the listings and solve. The initial run, the verify pass and every
/// follow round go through this one door, so a plan is always built the same
/// way from whatever was most recently read.
///
/// `bar` is derived from these prices when `None`; the verify pass passes the
/// bar the seed prices set, because it is the commander's standard for a
/// further stop rather than a fact about any one buyer.
pub(crate) fn solve(
    listings: &[acquire::Listing],
    keep: &[ardent::ArdentStation],
    route_config: &RouteConfig,
    hold: &[Stack],
    origin: Coordinates,
    config: &SellConfig,
    bar: Option<Ratio>,
) -> Result<Solved, String> {
    let (markets, commodities, _) = crate::route::ingest::markets_from_listings(
        listings,
        keep,
        &sell_floors(route_config, hold),
        &HashMap::new(),
    );
    if markets.is_empty() {
        return Err("no buyer answered with a usable listing".to_owned());
    }
    let held: Vec<Held> = hold
        .iter()
        .filter_map(|stack| {
            Some(Held {
                commodity: commodities.id_of_symbol(&stack.symbol)?,
                tons: Tons(stack.tons),
            })
        })
        .collect();
    if held.is_empty() {
        return Err("none of the buyers that answered are buying what you are carrying".to_owned());
    }
    let geometry = Geometry::new(&markets, crate::cmd::route::time_model(route_config));
    let candidates: Vec<u32> = (0..markets.len() as u32).collect();
    let bar = bar.unwrap_or_else(|| worth_bar(&geometry, origin, &held, &candidates, config));
    let plans = edm_route::sell::plans(&geometry, origin, &held, &candidates, config.stops, bar)
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
    Ok(Solved {
        markets,
        commodities,
        plans,
        bar,
    })
}

/// Replace each listing by market id, or add it.
pub(crate) fn fold_listings(listings: &mut Vec<acquire::Listing>, fresh: Vec<acquire::Listing>) {
    for listing in fresh {
        match listings
            .iter_mut()
            .find(|held| held.market_id == listing.market_id)
        {
            Some(held) => *held = listing,
            None => listings.push(listing),
        }
    }
}

pub(crate) fn present(out: &crate::out::Out, solved: &Solved, route_config: &RouteConfig, origin: Coordinates) {
    let geometry = Geometry::new(&solved.markets, crate::cmd::route::time_model(route_config));
    let Some(best) = solved.plans.first() else {
        return;
    };
    render(
        out,
        best,
        &solved.plans,
        &solved.markets,
        &solved.commodities,
        &geometry,
        origin,
        solved.bar,
    );
}

/// What the journal now says is aboard, split into what this search can plan
/// and what it cannot.
///
/// A follow round re-reads the buyers the first nomination produced; it never
/// asks Ardent again. So a commodity taken aboard since has no buyers here and
/// no plan can include it — it is returned separately to be named, because
/// silently leaving it off would read as "nobody buys this".
pub(crate) fn split_hold(aboard: Vec<Stack>, nominated: &HashSet<String>) -> (Vec<Stack>, Vec<Stack>) {
    aboard
        .into_iter()
        .partition(|stack| nominated.contains(&stack.symbol))
}

pub(crate) fn describe_stacks(stacks: &[Stack]) -> String {
    stacks
        .iter()
        .map(|stack| {
            format!(
                "{} t of {}",
                js::format_integer(stack.tons as f64),
                stack.symbol
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `SELL PLAN COST` table: what a disposal will spend, before it does.
///
/// Built apart from the verdict so a full-screen UI can show the same numbers
/// in a confirmation and ask for the same consent \[C53\].
pub(crate) fn sell_gate_blocks(
    estimate: &Estimate,
    considered: usize,
    carriers_cold: usize,
    max_requests: f64,
) -> Vec<Block<'static>> {
    let mut cost_rows = vec![
        field("buyers considered", js::format_integer(considered as f64)),
        field("buyers to read", js::format_integer(estimate.markets_to_poll as f64)),
        field("cached and still fresh", js::format_integer(estimate.cached_fresh as f64)),
        field("game-internal API requests", js::format_integer(estimate.requests)),
        field("ceiling", format!(
            "{} of {}   (--max-requests to raise)",
            js::format_integer(estimate.requests),
            js::format_integer(max_requests),
        )),
    ];
    // Only when there are any. A run over stations alone should not have to
    // read a row saying nothing was checked.
    if carriers_cold > 0 {
        cost_rows.insert(
            2,
            field("carriers to check", js::format_integer(carriers_cold as f64)),
        );
    }
    vec![Block::Table {
        title: "SELL PLAN COST".to_owned(),
        columns: columns::ROUTE_FIELD_COLUMNS,
        rows: cost_rows,
    }]
}

fn field(label: &str, value: String) -> Row<'static> {
    Row::Data(vec![
        std::borrow::Cow::Owned(label.to_owned()),
        std::borrow::Cow::Owned(value),
    ])
}

pub(crate) fn describe_hold(state: &CommanderState) -> String {
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
pub(crate) fn best_per_commodity(
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
pub(crate) fn worth_bar(
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
    // `reduce` with a strict `>` rather than `max_by_key`, which returns the
    // *last* maximum \[R27\]. Ties here are plans of equal merit and the first
    // is the one the enumeration's own total order already settled on.
    let clearing = singles
        .iter()
        .filter(|plan| plan.sold.0 >= total)
        .reduce(|a, b| {
            if b.rate().credits_per_hour_floor() > a.rate().credits_per_hour_floor() {
                b
            } else {
                a
            }
        });
    let fallback = singles
        .iter()
        .reduce(|a, b| if b.revenue.0 > a.revenue.0 { b } else { a });
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
    let most_credits = most_credits(best, plans);
    out.emit(&plan_blocks(best, most_credits, markets, commodities, geometry, origin, bar));
    out.emit(&sell_trade_commands(best, most_credits, markets, commodities));
}

/// The plan worth the most credits, when it is not the recommendation.
///
/// Ties break toward *less* flying. Without that, a plan matching the
/// recommendation's revenue but taking longer wins `max_by_key` and the row
/// reads "most credits ... 0 cr/h", which is a slower way to earn the same
/// money presented as an alternative worth considering.
pub(crate) fn most_credits<'a>(best: &Plan, plans: &'a [Plan]) -> Option<&'a Plan> {
    plans
        .iter()
        .reduce(|a, b| {
            if (b.revenue.0, -b.millis.0) > (a.revenue.0, -a.millis.0) {
                b
            } else {
                a
            }
        })
        .filter(|plan| plan.revenue.0 > best.revenue.0)
}

fn commodity_name(commodities: &edm_route::model::Commodities, id: CommodityId) -> String {
    commodities
        .name(id)
        .map_or_else(|| "?".to_owned(), edm_route::view::readable)
}

/// The plan table, its notes and the alternatives, as blocks \[C53\].
#[expect(clippy::too_many_lines, reason = "one table, its notes, and the alternatives table")]
pub(crate) fn plan_blocks(
    best: &Plan,
    most_credits: Option<&Plan>,
    markets: &[Market],
    commodities: &edm_route::model::Commodities,
    geometry: &Geometry<'_>,
    origin: Coordinates,
    bar: Ratio,
) -> Vec<Block<'static>> {
    let name = |id: CommodityId| commodity_name(commodities, id);
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
    let mut blocks = vec![
        Block::Table {
            title: "SELL PLAN".to_owned(),
            columns: columns::SELL_COLUMNS,
            rows,
        },
        Block::Note(format!(
            "sells {} t for {} cr over {}, at {} {}",
            int(best.sold.0 as f64),
            int(best.revenue.0 as f64),
            edm_core::spend::duration_estimate(best.millis.0 as f64 / 1_000.0),
            int(best.stops.len() as f64),
            if best.stops.len() == 1 { "stop" } else { "stops" },
        )),
    ];
    for left in &best.unsold {
        blocks.push(Block::Note(format!(
            "leaves {} t of {} aboard: no chosen buyer will take it",
            int(left.tons.0 as f64),
            name(left.commodity),
        )));
    }

    // The alternatives, so the refusal is arithmetic rather than assertion.
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
        let where_at = plan
            .stops
            .iter()
            .map(|stop| {
                let market = &markets[stop.market as usize];
                format!("{} ({})", market.station, market.system)
            })
            .collect::<Vec<_>>()
            .join(" > ");
        // Total flight, origin through every stop in order -- the same walk the
        // plan table's per-stop Ly column makes.
        let mut at = origin;
        let mut flown = 0.0;
        for stop in &plan.stops {
            flown += geometry.ly_from(at, stop.market);
            at = markets[stop.market as usize].coords;
        }
        alt.push(Row::Data(vec![
            label.into(),
            where_at.into(),
            js::to_fixed_1(flown).into(),
            int(plan.stops.len() as f64).into(),
            int(plan.sold.0 as f64).into(),
            format!("{} cr", int(plan.revenue.0 as f64)).into(),
            edm_core::spend::duration_estimate(plan.millis.0 as f64 / 1_000.0).into(),
            format!("{}/h", int(plan.rate().credits_per_hour_floor() as f64)).into(),
            marginal.into(),
        ]));
    }
    if alt.len() > 1 {
        blocks.push(Block::Table {
            title: "WHAT ELSE YOU COULD DO".to_owned(),
            columns: columns::SELL_ALTERNATIVES_COLUMNS,
            rows: alt,
        });
        blocks.push(Block::Note(format!(
            "your bar is {} cr/h (--worth); an extra stop is taken only when it beats that",
            int(bar.credits_per_hour_floor() as f64),
        )));
    }
    blocks
}

/// The `TRADE COMMANDS` block for the plan and its alternative \[C53\].
///
/// Every label carries its system. A bare `H7H-75X` is a fleet-carrier
/// callsign and names nothing a commander can fly to; the route command block
/// has always printed the system in its per-route header, and this one simply
/// never did \[C50\]. Widths are measured over every line that will be
/// printed, across both plans, so the commands stay in one column instead of
/// being padded to a constant that the longest name overruns.
pub(crate) fn sell_trade_commands(
    best: &Plan,
    most_credits: Option<&Plan>,
    markets: &[Market],
    commodities: &edm_route::model::Commodities,
) -> Vec<Block<'static>> {
    let name = |id: CommodityId| commodity_name(commodities, id);
    let int = js::format_integer;
    let mut blocks = vec![Block::Heading("TRADE COMMANDS".to_owned())];
    let lines: Vec<(String, String)> = [Some(best), most_credits]
        .into_iter()
        .flatten()
        .flat_map(|plan| {
            plan.stops.iter().flat_map(|stop| {
                let market = &markets[stop.market as usize];
                stop.drops.iter().map(move |drop| {
                    (
                        format!("{} ({})", market.station, market.system),
                        format!(
                            "edm trade --market-id {} --type sell --item {} --qty {}",
                            js::js_number(market.market_id as f64),
                            name(drop.commodity).replace(' ', ""),
                            js::js_number(drop.tons.0 as f64),
                        ),
                    )
                })
            })
        })
        .collect();
    let width = lines.iter().map(|(label, _)| label.len()).max().unwrap_or(0);

    let commands = |plan: &Plan, blocks: &mut Vec<Block<'static>>| {
        for stop in &plan.stops {
            let market = &markets[stop.market as usize];
            for drop in &stop.drops {
                blocks.push(Block::Raw(format!(
                    "  at {:<width$}  edm trade --market-id {} --type sell --item {} --qty {}",
                    format!("{} ({})", market.station, market.system),
                    js::js_number(market.market_id as f64),
                    name(drop.commodity).replace(' ', ""),
                    js::js_number(drop.tons.0 as f64),
                )));
            }
        }
    };
    commands(best, &mut blocks);
    // The alternative gets its commands too. Naming a plan worth thirty million
    // more and then printing no way to fly it makes the row decoration: the
    // reader has to go and find the market ids by hand, which is the work the
    // block exists to save \[C50\].
    if let Some(plan) = most_credits {
        blocks.push(Block::Raw(String::new()));
        blocks.push(Block::Raw(format!(
            "  most credits instead ({} more, {} longer):",
            int((plan.revenue.0 - best.revenue.0) as f64),
            edm_core::spend::duration_estimate((plan.millis.0 - best.millis.0) as f64 / 1_000.0),
        )));
        commands(plan, &mut blocks);
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use edm_core::domain::commander::{CargoItem, ObservationSource, Observed};

    fn state_with(items: Vec<CargoItem>) -> CommanderState {
        let mut state = CommanderState::default();
        state.cargo.inventory = Some(Observed {
            source: ObservationSource::CargoSidecar,
            timestamp: None,
            ordinal: 1,
            value: items,
        });
        state
    }

    fn item(name: &str, count: u64, stolen: u64) -> CargoItem {
        CargoItem {
            name: name.to_owned(),
            name_localised: None,
            count,
            stolen,
            mission_id: None,
        }
    }

    /// A round's journal read is the same manifest as the first: a partly
    /// stolen stack still splits, so a sale of the clean part shows as the
    /// hold shrinking rather than as the stack changing shape.
    #[test]
    fn a_partly_stolen_stack_plans_only_its_clean_part() {
        let (clean, excluded) = manifest(&state_with(vec![item("Tritium", 100, 30)]), &[]);
        assert_eq!(
            clean,
            vec![Stack {
                symbol: "tritium".to_owned(),
                tons: 70
            }]
        );
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].tons, 30);
    }

    /// Cargo the search never nominated buyers for is set aside to be named,
    /// and an empty planned side is what "everything is sold" means even
    /// while something else is still aboard \[C52\].
    #[test]
    fn cargo_taken_aboard_since_the_search_is_split_from_the_plan() {
        let nominated: HashSet<String> = ["tritium".to_owned()].into_iter().collect();
        let (clean, _) = manifest(
            &state_with(vec![item("Gold", 40, 0), item("Tritium", 12, 0)]),
            &[],
        );
        let (planned, unplanned) = split_hold(clean, &nominated);
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].symbol, "tritium");
        assert_eq!(unplanned.len(), 1);
        assert_eq!(unplanned[0].symbol, "gold");

        let (clean, _) = manifest(&state_with(vec![item("Gold", 40, 0)]), &[]);
        let (planned, unplanned) = split_hold(clean, &nominated);
        assert!(planned.is_empty(), "the nominated cargo is gone: sold");
        assert_eq!(unplanned.len(), 1);
    }

    /// A fresh read replaces the seed listing for the same market and adds a
    /// market the seed never had, so a re-solve never prices one market twice.
    #[test]
    fn fresh_listings_replace_by_market_id() {
        let listing = |id: f64, at: f64| acquire::Listing {
            market_id: id,
            station_name: String::new(),
            system_name: String::new(),
            document: edm_core::js::json::JsValue::Null,
            read_at_ms: at,
            observed_at_ms: None,
            from_cache: false,
        };
        let mut seed = vec![listing(1.0, 10.0), listing(2.0, 10.0)];
        fold_listings(&mut seed, vec![listing(2.0, 20.0), listing(3.0, 20.0)]);
        let read: Vec<(f64, f64)> = seed.iter().map(|l| (l.market_id, l.read_at_ms)).collect();
        assert_eq!(read, vec![(1.0, 10.0), (2.0, 20.0), (3.0, 20.0)]);
    }
}
