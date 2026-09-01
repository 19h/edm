//! Price-index-first path for `edm route --quick`.
//!
//! A regional route survey discovers every station and then pays Frontier for
//! every survivor. That is the right exhaustive answer, but it is not the
//! right first question for "where can I buy or sell this commodity now?".
//! Ardent already maintains a price-ordered index for that question. This
//! module scores every seller–buyer pair by the same first-lap credits-per-hour
//! the live ranker will use — spread times cargo over travel time — keeps the
//! endpoints of the best hops, then deliberately discards Ardent's prices:
//! every retained market is read live through the normal route poller before
//! it can rank or reach EDDN. Taking the N cheapest sellers independently of
//! the N dearest buyers fills the prefix with galactic-average stations after
//! a handful of real outliers, and drops a nearer slightly-worse buyer that
//! would have won on rate.

use std::collections::HashSet;

use futures_util::StreamExt as _;

use edm_core::ardent::{self, CommodityDirection, CommodityPrice};
use edm_core::cli::config::RouteConfig;
use edm_core::pace::{Bucket, BucketState};
use edm_core::render::views::{self, EddnCoverage, RouteCoverage};
use edm_core::render::{Block, Row, columns};
use edm_core::select;
use edm_core::spend::{Counts, Exclusion, SizePrior};
use edm_route::num::{Credits, Millis, Ratio};
use edm_route::time::TimeModel;

use crate::ardent::ArdentClient;
use crate::cmd::{App, CmdResult};
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs, Timer};
use crate::route::access::{self, AccessIndex};
use crate::route::acquire;
use crate::route::cache::Cache;
use crate::route::pacer::Pacer;
use crate::route::pool::Job;
use crate::route::relay::Relayed;

/// One price-index row that survived the per-side cap.
#[derive(Clone, Debug)]
struct Candidate {
    commodity: String,
    price: CommodityPrice,
}

/// A commodity from `--item` that produced no candidate at all.
///
/// Reported by name: a lookup over several commodities otherwise prints a table
/// of the ones that worked and says nothing about the one that was misspelt.
struct Barren {
    commodity: String,
    /// Whether Ardent's price index returned any row for this name, before this
    /// program's own quantity floor and access filters ran.
    indexed: bool,
}

/// The result of filtering one Ardent response before taking its price prefix.
struct SideSelection {
    candidates: Vec<Candidate>,
    considered: usize,
    exclusions: Vec<Exclusion>,
}

/// Run a commodity-first, live-verified route lookup.
#[expect(
    clippy::too_many_lines,
    reason = "the sequence is the safety contract: free index, filters, priced gate, live reads, then rank"
)]
/// How many consecutive empty rounds end a `--follow` session \[C46\].
///
/// Three rather than one, because a single round can come back empty from a
/// transient failure -- a market that timed out is a market whose route drops
/// for that round only. Three in a row is the candidate set being genuinely
/// gone, not a bad read.
const BARREN_ROUNDS_BEFORE_STOPPING: usize = 3;

pub(super) async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs, T: Timer>(
    app: &App<'_, H, C, E, F>,
    config: &RouteConfig,
    commander: Option<&edm_core::domain::commander::CommanderState>,
    timer: &T,
) -> CmdResult {
    let quick = config
        .quick
        .as_ref()
        .expect("quick module is entered only for --quick");
    if config.verify_systems {
        return Err(
            "--verify-systems cannot be combined with --quick: the selected market ids are already polled live"
                .to_owned(),
        );
    }

    let out = app.out;
    let note = |text: String| {
        if !config.quiet {
            out.line(&text);
        }
    };
    let ardent = ArdentClient::new(app.http, &app.overrides.ardent_base);
    note(format!(
        "resolving \"{}\" through Ardent for quick lookup...",
        config.reference
    ));
    let centre = super::resolve(&ardent, &config.reference).await?;

    // `--from-here`: which markets a route may depart from \[C48\]. Docked, it
    // is the one market under the ship; undocked, every market in the current
    // system, because "here" is then a system and not a berth. Ardent's system
    // markets page is free, so this costs no Frontier request.
    let mut depart_stations: Vec<edm_core::ardent::ArdentStation> = Vec::new();
    if config.from_here {
        let mut here = ardent.system_markets(&centre).await.unwrap_or_default();
        edm_core::ardent::place(&mut here, centre.address, centre.coordinates);
        let docked =
            commander.and_then(edm_core::domain::commander::CommanderState::current_market_id);
        if let Some(id) = docked
            && let Some(mine) = here.iter().find(|s| s.market_id == id as f64)
        {
            depart_stations.push(mine.clone());
        } else {
            depart_stations = here;
        }
        if depart_stations.is_empty() {
            return Err(format!(
                "--from-here found no market to depart from: the journal does not say you are \
                 docked, and Ardent lists no market in {}",
                centre.name
            ));
        }
    }
    let depart_from: Vec<f64> = depart_stations.iter().map(|s| s.market_id).collect();
    // Say what was pinned. "No profitable hop" is a very different answer
    // depending on whether it means "nothing here sells anything worth hauling"
    // or "I could not work out where here is", and without this line the two
    // are indistinguishable \[C48\].
    if !depart_stations.is_empty() {
        note(match depart_stations.as_slice() {
            [only] => format!(
                "--from-here: every route departs from {} ({})",
                only.station_name, only.system_name
            ),
            many => format!(
                "--from-here: not docked, so every route departs from one of the {} markets in {}",
                edm_core::js::format_integer(many.len() as f64),
                centre.name,
            ),
        });
    }

    // Resolve every --item, and expand every --category, against Ardent's own
    // catalogue before spending a query on either. An id Ardent does not index
    // answers `200 []`, exactly like a region with no stock, so without this
    // check a misspelt or merely display-named commodity — or a category that
    // expands to nothing this catalogue knows — produces a confident empty
    // answer rather than an error. The catalogue is one free, cached read for
    // the whole run.
    let configured_cache = super::cache_for(app, config);
    let atlas =
        crate::route::atlas::Atlas::new(configured_cache.root(), config.cache, config.refresh);
    let (catalogue, catalogue_fetched) = ardent
        .commodity_catalogue_cached(&atlas, &app.ports.fs, app.ports.clock.now_ms())
        .await
        .map_err(|error| format!("reading Ardent's commodity catalogue: {error}"))?;
    let wanted = resolve_items(quick, &catalogue, &note)?;

    // The optimiser assumes a default hold when `--cargo` is unknown, so the
    // candidate floor has to assume the same one. Deriving 1 t here while the
    // ranking below reasons about 1,232 t fills the price prefix with markets
    // the ranking can never use — near Cromovit the cheapest gold in 200 Ly is
    // 3,984 cr for *eight tonnes*, and a hundred such rows will crowd out every
    // market worth flying to and be paid for one request each.
    let assumed_cargo = config
        .cargo
        .unwrap_or_else(|| edm_route::model::ShipConfig::default().cargo.0 as f64);
    let minimum = quick.minimum_quantity(Some(assumed_cargo));
    // Existing route floors still apply. Fold them into the index query before
    // its 1,000-row cap, rather than letting an ineligible price consume one
    // of this side's N candidate slots.
    let seller_minimum = edm_core::js::js_max(minimum, config.min_supply);
    let buyer_minimum = edm_core::js::js_max(minimum, config.min_demand);
    let hops = edm_core::js::format_integer(quick.markets_per_side as f64);
    note(format!(
        "scoring Ardent seller/buyer pairs by estimated credits per hour and keeping the \
         {hops} best {hop} of at least {supply} t to buy and {demand} published t of demand, \
         for {count} {item}{assumed}...",
        hop = plural(quick.markets_per_side, "hop", "hops"),
        supply = edm_core::js::format_integer(seller_minimum),
        demand = edm_core::js::format_integer(buyer_minimum),
        count = edm_core::js::format_integer(wanted.len() as f64),
        item = plural(wanted.len(), "commodity", "commodities"),
        // Said, because the floor is a tenth of it and the floor decides which
        // markets are even looked at.
        assumed = if config.cargo.is_none() {
            format!(
                " (no --cargo and none in local commander state, so a {} t hold is assumed)",
                edm_core::js::format_integer(assumed_cargo),
            )
        } else {
            String::new()
        },
    ));

    // Each commodity has two price-ranked remote prefixes plus the direct
    // reference-system commodity page: Ardent's nearby route omits its own
    // centre, and a quick answer must not silently omit zero-Ly markets.
    // `buffered` keeps at most the usual number of commodity lookups in
    // flight and yields in the user/item order, making tied selection stable.
    let price_index = &ardent;
    let system_name = centre.name.as_str();
    let radius_ly = config.radius_ly;
    let include_carriers = config.include_carriers;
    let answers =
        futures_util::stream::iter(wanted.iter().cloned().map(move |commodity| async move {
            let exports = price_index
                .commodity_nearby(
                    system_name,
                    &commodity,
                    CommodityDirection::Exports,
                    radius_ly,
                    include_carriers,
                    seller_minimum,
                )
                .await
                .map_err(|error| {
                    format!("querying Ardent seller prices for {commodity}: {error}")
                })?;
            let imports = price_index
                .commodity_nearby(
                    system_name,
                    &commodity,
                    CommodityDirection::Imports,
                    radius_ly,
                    include_carriers,
                    buyer_minimum,
                )
                .await
                .map_err(|error| {
                    format!("querying Ardent buyer prices for {commodity}: {error}")
                })?;
            let (local_exports, local_imports) = price_index
                .commodity_in_system(system_name, &commodity)
                .await
                .map_err(|error| {
                    format!("querying Ardent reference-system prices for {commodity}: {error}")
                })?;
            Ok::<_, String>((commodity, exports, imports, local_exports, local_imports))
        }))
        .buffered(super::ARDENT_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    // Hoisted out of the loop so the first Ardent error still ends the run
    // before a single Spansh request is spent on candidates that will not be
    // used. `collect` on a `Result` yields the first `Err` in iteration order,
    // which is exactly what the `?` inside the loop used to do.
    let answers = answers.into_iter().collect::<Result<Vec<_>, String>>()?;

    // Hoisted: the carrier-access phase spends metered requests now, and
    // everything metered belongs behind one pacer \[C37\].
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
    let pacer = Pacer::new(super::pacing(config), &app.ports.clock, timer, &entropy);

    // One resolution for the whole lookup, before any side is selected: every
    // commodity's price pages draw on the same regional pool of carriers, so
    // resolving per commodity would ask Spansh the same question once per
    // --item \[C36\].
    let (docking, docking_report) =
        quick_docking_access(
            app,
            config,
            &configured_cache,
            &pacer,
            &stamp_overrides,
            &answers,
            &centre.coordinates,
            wanted.len(),
            commander,
            &note,
        )
        .await?;

    let mut candidates = Vec::new();
    let mut considered = 0usize;
    let mut exclusions = Vec::new();
    let mut barren = Vec::new();
    for answer in answers {
        let (commodity, exports, imports, local_exports, local_imports) = answer;
        // Whether Ardent's index holds this name at all, before any floor or
        // filter of ours has run. A misspelt --item and a commodity nobody
        // trades nearby both end with no candidates, and they call for
        // different corrections.
        let indexed = !exports.is_empty()
            || !imports.is_empty()
            || !local_exports.is_empty()
            || !local_imports.is_empty();
        let before = candidates.len();
        let selected = select_commodity(
            [exports, local_exports].concat(),
            [imports, local_imports].concat(),
            &commodity,
            seller_minimum,
            buyer_minimum,
            quick.markets_per_side,
            config,
            &centre.coordinates,
            assumed_cargo,
            &docking,
        );
        considered += selected.considered;
        merge_exclusions(&mut exclusions, selected.exclusions);
        candidates.extend(selected.candidates);
        if candidates.len() == before {
            barren.push(Barren { commodity, indexed });
        }
    }

    if !config.json {
        emit_candidates(
            out,
            &candidates,
            &barren,
            seller_minimum,
            buyer_minimum,
            quick.markets_per_side,
            &centre.coordinates,
        );
        // Only reachable when the shape was asked for by name: the config layer
        // already defaults a one-commodity lookup to a single hop. Say it here,
        // before the spend gate, because the alternative is paying for every
        // candidate and then reading "no profitable round trip in this data" —
        // which describes the prices, not the impossible shape that was asked
        // for.
        if config.shape.is_cycle() && quick.cannot_cycle() {
            out.emit(&[Block::Note(format!(
                "a {} cannot be satisfied by one commodity: a cycle must carry different cargo on the way back, and no market sells a commodity for less than it pays for it. Name a second --item, or use --shape one-way.",
                config.shape.noun(),
            ))]);
        }
    }

    // A market can sell Gold, buy Silver, and occur in both price pages. It is
    // one live listing and one possible EDDN message, never two paid polls.
    let mut seen = HashSet::new();
    let mut stations = candidates
        .iter()
        .filter(|candidate| seen.insert(candidate.price.station.market_id.to_bits()))
        .map(|candidate| candidate.price.station.clone())
        .collect::<Vec<_>>();
    // The station being departed from has to be *read*, not merely allowed:
    // Ardent nominates by price, so the market under the ship is usually not in
    // any commodity's top page, and clearing every other market's supply would
    // then leave the search with no seller at all.
    for station in &depart_stations {
        if seen.insert(station.market_id.to_bits()) {
            stations.push(station.clone());
        }
    }

    let selected_markets = stations.len();
    let mut provenance = super::QuickProvenance {
        commodities: wanted.clone(),
        markets_per_side: quick.markets_per_side,
        seller_minimum,
        buyer_minimum,
        candidate_rows: candidates.len(),
        market_ids: stations.iter().map(|station| station.market_id).collect(),
        unpublished_buyer_candidates: candidates
            .iter()
            .filter(|candidate| has_unpublished_import_demand(&candidate.price))
            .count(),
        commodities_without_candidates: barren.iter().map(|item| item.commodity.clone()).collect(),
        commodities_absent_from_index: barren
            .iter()
            .filter(|item| !item.indexed)
            .map(|item| item.commodity.clone())
            .collect(),
        best_live: Vec::new(),
    };
    // Free, and before the gate, because the cache decides what the sweep
    // actually costs. `acquire::prepare` is file reads only.
    //
    // Two caches, deliberately. `read_cache` honours `--max-age` and is what
    // seeds the ranking; `write_cache` is refresh-mode and is what the sweep
    // and the verify pass poll through, because a market re-read seconds ago
    // must not come back out of the entry that read just wrote \[C38\].
    let read_cache = super::cache_for(app, config);
    let write_cache = Cache::new(
        read_cache.root().to_path_buf(),
        config.max_age_minutes,
        config.cache,
        true,
    );
    let prepared = acquire::prepare(
        &read_cache,
        &app.ports.fs,
        &stations,
        app.ports.clock.now_ms(),
    );

    let survey = super::Survey {
        complete_to_ly: config.radius_ly,
        price_index: true,
        // Two remote price prefixes plus the reference-system commodity page
        // per item, and the commodity catalogue when it was not already local.
        // They are free Ardent reads, not paid Frontier work.
        ardent_requests: (wanted.len().saturating_mul(3) + usize::from(catalogue_fetched)) as u32,
        counts: Counts {
            // Already spent. Folded forward so the ceiling stays cumulative
            // across the run's two gates rather than resetting at each.
            carriers_to_probe: docking_report.map_or(0, |report| report.cost.requests),
            systems: 0,
            systems_to_read: 0,
            stations_known: considered,
            markets_to_poll: selected_markets,
            cached_fresh: prepared.hits.fresh,
        },
        exclusions,
    };
    let decision = super::plan::gate(out, config, &survey, SizePrior::default());
    if !decision.proceeds() {
        return Ok(());
    }

    let started_ms = app.ports.clock.now_ms();

    let relayed_log = Relayed::new(write_cache.root(), config.eddn_max_age_minutes);
    let eddn_options = config
        .eddn
        .then(|| edm_core::cli::config::eddn_config(&app.cli, &app.session.credentials))
        .transpose()
        .map_err(|error| error.message().to_owned())?;
    let eddn_bucket = Bucket {
        rate: config.eddn_rate_per_second,
        burst: 1.0,
        min_rate: edm_core::js::js_min(config.eddn_rate_per_second, 0.5),
    };
    let eddn_tokens =
        std::cell::RefCell::new(BucketState::new(eddn_bucket, app.ports.clock.now_ms()));
    let eddn = eddn_options.as_ref().map(|options| acquire::Eddn {
        options,
        url: &app.overrides.eddn_url,
        relayed: &relayed_log,
        stations: &stations,
        bucket: eddn_bucket,
        tokens: &eddn_tokens,
    });
    let relay_tally = std::cell::RefCell::new(crate::route::relay::Tally::default());

    let total = stations.len();
    let report = |job: &Job,
                  outcome: &crate::route::pool::Outcome,
                  attempts: u32,
                  completed: usize| {
        let system = stations
            .iter()
            .find(|station| matches!(job, Job::Market { market_id, .. } if *market_id == station.market_id))
            .map_or("", |station| station.system_name.as_str());
        out.line(&views::sweep_line(&views::SweepLine {
            completed,
            total,
            station: job.label(),
            system,
            status: outcome.status,
            tradable: outcome.tradable,
            from_cache: false,
            attempts,
        }));
    };
    let trace = |event: &views::PaceEvent<'_>| out.line(&views::pace_line(event));
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
        eddn: eddn.as_ref(),
        workers: config.workers as usize,
        quiet: config.quiet,
        verify_systems: false,
        language: &query.language,
        report: (!config.quiet).then_some(&report as crate::route::pool::Report<'_>),
        trace: (config.verbose && !config.quiet).then_some(&trace as crate::route::pool::Trace<'_>),
        total,
    };
    // The price index selected the candidates, but only live listings may enter
    // the graph. Keep both directions on the same quantity floor and restrict
    // ingest to the explicitly named commodities.
    let mut rank_config = config.clone();
    rank_config.min_supply = seller_minimum;
    rank_config.min_demand = buyer_minimum;
    // `ingest::floors` reads this to decide which live rows may be ranked, and
    // it compares against the payload's own symbol. The typed spelling would
    // reject every row of a commodity whose display name is not its id.
    if let Some(settings) = rank_config.quick.as_mut() {
        settings.commodities.clone_from(&wanted);
    }
    let deadline_ms = started_ms + config.deadline_seconds * 1_000.0;
    let clock = &app.ports.clock;
    let expired = || clock.now_ms() >= deadline_ms;
    let watch = edm_route::watch::Watch::unlimited().until(&expired);

    let acquired = acquire::sweep(&cx, &pacer, prepared, &[]).await;
    // The verify rounds re-use the same transport, pacer and write cache, but
    // not the progress reporter: its `[k/N]` counter belongs to the sweep, and
    // a second pass restarting it at 1 would read as the sweep beginning again.
    // The rounds announce themselves instead.
    let verify_cx = acquire::Cx {
        report: None,
        trace: None,
        ..cx
    };

    let acquire::Acquired {
        mut listings,
        unreached: acquired_unreached,
        cache: acquired_cache,
        tally: acquired_tally,
        relayed: acquired_relayed,
    } = acquired;

    // Which markets this run actually measured. Everything else came out of the
    // cache and is a candidate for verification.
    let mut live: std::collections::HashSet<u64> = listings
        .iter()
        .filter(|listing| !listing.from_cache)
        .map(|listing| listing.market_id.to_bits())
        .collect();

    let origin = super::approach_origin(&ardent, config, commander).await?;
    let no_candidates = std::collections::HashMap::new();
    let mut ranked = super::solve_ranked_from(
        &rank_config,
        &listings,
        &stations,
        &no_candidates,
        watch,
        &depart_from,
    );

    // Re-read the markets behind the ranked routes until the presented list is
    // one that was measured rather than estimated \[C38\]. A cold cache makes
    // this a no-op: everything is already live.
    let (verified, fresh) = super::verify_ranked(
        &verify_cx,
        &pacer,
        &rank_config,
        &mut ranked,
        &stations,
        &no_candidates,
        &mut live,
        &note,
    )
    .await;
    // Fold the fresh reads back over the seed listings, so the coverage block
    // and the per-commodity table below describe the same prices the routes
    // were finally ranked on.
    let verified_markets = fresh.len();
    for listing in fresh {
        match listings
            .iter_mut()
            .find(|held| held.market_id == listing.market_id)
        {
            Some(held) => *held = listing,
            None => listings.push(listing),
        }
    }
    if verified.rounds > 0 {
        note(super::verify_note(verified));
    }

    let acquired = acquire::Acquired {
        listings,
        unreached: acquired_unreached,
        // A market read live during verification is no longer a cache hit, and
        // the coverage note keys its "came from the cache" sentence on exactly
        // this number.
        cache: crate::route::cache::Hits {
            fresh: acquired_cache.fresh.saturating_sub(verified_markets),
            ..acquired_cache
        },
        tally: acquired_tally,
        relayed: acquired_relayed,
    };

    let at_ms = app.ports.clock.now_ms();
    let coverage = coverage_of(
        &acquired,
        verified_markets,
        selected_markets,
        config.eddn,
        pacer.spent(),
        pacer.tripped().is_some(),
        (at_ms - started_ms) / 1_000.0,
        at_ms,
    );
    if !config.json {
        out.aside(&views::route_coverage(&coverage));
    }

    // The question this mode was asked. Ranking answers "what should I fly?";
    // this answers "where do I buy and sell each of these, right now?", and it
    // reads the live payloads rather than the index rows that nominated them.
    let best = best_live_prices(
        &acquired.listings,
        &candidates,
        &stations,
        &wanted,
        seller_minimum,
        buyer_minimum,
        config.include_illegal,
        &centre.coordinates,
    );
    if !config.json {
        emit_live_prices(out, &best, &wanted);
    }
    provenance.best_live = best;

    let unreached = !acquired.unreached.is_empty() || acquired.tally.markets_out_of_time > 0;
    super::render_ranked(
        out,
        &rank_config,
        &ranked,
        origin,
        &coverage,
        super::SpecialOpportunities::default(),
        Some(&provenance),
        docking_report,
    );

    // --- --follow: keep re-reading the ranking until told to stop \[C43\] ---
    //
    // A round is deliberately the verify pass with the live set cleared: that
    // code already re-polls exactly the markets behind the ranked routes,
    // patches them in place by index, and rescores. Nothing here re-solves --
    // the graph build is 127 s at five thousand markets, so a loop that
    // re-searched would spend its whole interval searching.
    if let Some(interval) = config.follow_seconds {
        // The shortlist as first solved. Every round is re-evaluated against
        // *this*, not against what survived the previous round, because
        // `rescore` only ever filters: without the restore, a carrier offline
        // for one poll would be deleted from the ranking permanently and could
        // never come back, so a long session would erode to nothing.
        let baseline = ranked.routes().to_vec();
        let mut round: usize = 0;
        // Consecutive rounds in which the whole shortlist came back unpriced.
        let mut barren_rounds: usize = 0;
        // The ingest counters describe the *first* solve. Re-printing them under
        // every round would restate a number the re-reads never recomputed, so
        // they are cleared once they have been shown.
        ranked.crossing = crate::route::ingest::Crossing::default();
        loop {
            if let Some(limit) = config.follow_rounds
                && round >= limit
            {
                note(format!(
                    "--follow-rounds {} reached",
                    edm_core::js::format_integer(limit as f64)
                ));
                break;
            }
            // The only live ceiling this program has. `--max-requests` is
            // checked at the gate against an *estimate* and never against what
            // was actually sent, so without this an indefinite loop would be
            // bounded by nothing at all.
            let before = pacer.spent();
            if before.requests as f64 >= config.max_requests {
                note(format!(
                    "--max-requests {} reached after {} {}",
                    edm_core::js::format_integer(config.max_requests),
                    edm_core::js::format_integer(round as f64),
                    if round == 1 { "round" } else { "rounds" },
                ));
                break;
            }
            timer.sleep_ms(interval * 1_000.0).await;
            round += 1;
            // A fresh deadline window and a cleared breaker latch. `--deadline`
            // bounds one sweep, and a session is many sweeps with sleep in
            // between; the latch is first-trip-wins and never healed, so one
            // transient outage would otherwise silently kill every later round
            // while the loop ticked on looking healthy.
            pacer.begin_round();
            *ranked.routes_mut() = baseline.clone();
            live.clear();

            let (verified, _fresh) = super::verify_ranked(
                &verify_cx,
                &pacer,
                &rank_config,
                &mut ranked,
                &stations,
                &no_candidates,
                &mut live,
                &note,
            )
            .await;

            let spent = pacer.spent();
            note(format!(
                "round {}: {} markets re-read, {} requests, {} of {} routes still priced{}",
                edm_core::js::format_integer(round as f64),
                edm_core::js::format_integer(verified.markets as f64),
                edm_core::js::format_integer((spent.requests - before.requests) as f64),
                edm_core::js::format_integer(ranked.routes().len() as f64),
                edm_core::js::format_integer(baseline.len() as f64),
                if pacer.tripped().is_some() {
                    " (the rate limiter tripped this round)"
                } else {
                    ""
                },
            ));
            super::render_ranked(
                out,
                &rank_config,
                &ranked,
                origin,
                &coverage,
                super::SpecialOpportunities::default(),
                None,
                None,
            );

            // A follow round re-prices a fixed candidate set; it never
            // re-nominates. So when every route dies at once -- which is what
            // happens when a whole ranking shares one buyer and that buyer's
            // order is filled or withdrawn -- no later round can recover it,
            // and the loop would otherwise re-read the same dead markets
            // forever to print "no profitable hop". Stop and say why, rather
            // than spend the ceiling proving it repeatedly \[C46\].
            if ranked.routes().is_empty() {
                barren_rounds += 1;
                if barren_rounds >= BARREN_ROUNDS_BEFORE_STOPPING {
                    note(format!(
                        "the whole shortlist has been unpriced for {} rounds: every route in it \
                         has lost a side, and --follow re-prices the routes it was given rather \
                         than nominating new ones. Re-run to search again",
                        edm_core::js::format_integer(barren_rounds as f64),
                    ));
                    break;
                }
            } else {
                barren_rounds = 0;
            }
        }
    }

    // A 410 is a reached answer, exactly as it is in a full survey. A failed
    // or deadline-abandoned live candidate is not a price that merely ranked
    // badly, so make the process status carry that distinction.
    out.set_exit(if unreached || coverage.breaker_tripped {
        crate::out::EXIT_FAILURE
    } else {
        0
    });
    Ok(())
}

/// One commodity's best live seller or best live buyer among the markets this
/// run actually read.
///
/// The prices here are Frontier's, not Ardent's. The index price is carried
/// alongside so the two can be compared, because "the index nominated it and
/// the live read decided" is the claim this whole mode rests on.
#[derive(Clone, Debug)]
pub(crate) struct BestLive {
    /// Ardent's canonical id, as `--item` was normalised to.
    pub commodity: String,
    /// The live payload's own spelling, which is what a commander sees in game.
    pub display: String,
    pub direction: CommodityDirection,
    pub price: f64,
    pub volume: f64,
    /// A buyer with a positive demand bracket and no published tonnage.
    pub unpublished: bool,
    /// Ardent's price for this same market and side, when it is the row that
    /// nominated it.
    pub index_price: Option<f64>,
    pub market_id: f64,
    pub station: String,
    pub system: String,
    pub distance_ly: f64,
    /// Whether this price was reused from the local cache rather than read
    /// during this run \[C38\]. The table's own heading turns on it.
    pub from_cache: bool,
}

/// Reduce the live listings to one best seller and one best buyer per commodity.
///
/// Only rows the optimiser could also price are eligible: a table that named a
/// best buyer the ranking below then dropped for a fractional quantity would
/// contradict itself on one screen.
#[expect(
    clippy::too_many_arguments,
    reason = "every one is a rule the answer depends on; folding them into a struct would hide which"
)]
fn best_live_prices(
    listings: &[crate::route::acquire::Listing],
    candidates: &[Candidate],
    stations: &[ardent::ArdentStation],
    wanted: &[String],
    seller_minimum: f64,
    buyer_minimum: f64,
    include_illegal: bool,
    centre: &edm_core::domain::id64::Coordinates,
) -> Vec<BestLive> {
    let mut best: Vec<BestLive> = Vec::new();
    for listing in listings {
        let Some(snapshot) = listing.snapshot() else {
            continue;
        };
        let Some(station) = stations
            .iter()
            .find(|station| station.market_id == listing.market_id)
        else {
            continue;
        };
        for row in &snapshot.commodities {
            let commodity = ardent::normalise_commodity_name(row.name);
            if !wanted.contains(&commodity) {
                continue;
            }
            if row.illegal && !include_illegal {
                continue;
            }
            if crate::route::ingest::raw_commodity(
                row,
                &mut crate::route::ingest::Crossing::default(),
            )
            .is_none()
            {
                continue;
            }
            for direction in [CommodityDirection::Exports, CommodityDirection::Imports] {
                let (price, volume, floor) = match direction {
                    CommodityDirection::Exports => (row.buy_price, row.stock, seller_minimum),
                    CommodityDirection::Imports => (row.sell_price, row.demand, buyer_minimum),
                };
                let unpublished = direction == CommodityDirection::Imports
                    && volume == 0.0
                    && row.demand_bracket >= 1.0;
                if !price.is_finite() || price <= 0.0 || !(volume >= floor || unpublished) {
                    continue;
                }
                let found = BestLive {
                    from_cache: listing.from_cache,
                    commodity: commodity.clone(),
                    // Frontier's payload spells this `LowTemperatureDiamond`.
                    // The ranking below prints "Low Temperature Diamond", and
                    // two tables on one screen must not name the same cargo
                    // two ways.
                    display: edm_route::view::readable(row.name),
                    direction,
                    price,
                    volume,
                    unpublished,
                    index_price: candidates
                        .iter()
                        .find(|candidate| {
                            candidate.commodity == commodity
                                && candidate.price.direction == direction
                                && candidate.price.station.market_id == listing.market_id
                        })
                        .map(|candidate| candidate.price.price),
                    market_id: listing.market_id,
                    station: listing.station_name.clone(),
                    system: listing.system_name.clone(),
                    distance_ly: ardent::separation_ly(&station.coordinates, centre),
                };
                match best
                    .iter_mut()
                    .find(|held| held.commodity == found.commodity && held.direction == direction)
                {
                    Some(held) if beats(&found, held) => *held = found,
                    Some(_) => {}
                    None => best.push(found),
                }
            }
        }
    }
    best
}

/// Whether the challenger is the better side of the trade, ties broken by
/// market id so a re-run of the same data prints the same station.
fn beats(challenger: &BestLive, held: &BestLive) -> bool {
    let better = match challenger.direction {
        CommodityDirection::Exports => challenger.price.total_cmp(&held.price),
        CommodityDirection::Imports => held.price.total_cmp(&challenger.price),
    };
    better
        .then_with(|| challenger.market_id.total_cmp(&held.market_id))
        .is_lt()
}

/// Turn every `--item` and every `--category` into ids Ardent actually indexes,
/// or refuse.
///
/// Refusing is the point. Ardent answers an unknown commodity with `200` and an
/// empty page — indistinguishable, downstream, from a region with no stock — so
/// a lookup that does not check here reports "no candidates" for a typo and
/// exits successfully. A category that expands to nothing is the same silent
/// empty answer, just for a class instead of a spelling.
fn resolve_items(
    quick: &edm_core::cli::config::QuickLookup,
    catalogue: &[String],
    note: &impl Fn(String),
) -> Result<Vec<String>, String> {
    if catalogue.is_empty() {
        return Err(
            "Ardent's commodity catalogue came back empty, so no --item or --category can be checked against it"
                .to_owned(),
        );
    }
    let mut wanted = Vec::with_capacity(quick.commodities.len());
    for (candidate, typed) in quick.commodities.iter().zip(&quick.raw) {
        match ardent::resolve_commodity(typed, catalogue) {
            ardent::Resolution::Exact(id) => wanted.push(id),
            ardent::Resolution::Adjusted(id) => {
                // Said out loud rather than applied quietly: the user asked
                // about one spelling and is being answered about another.
                note(format!(
                    "--item \"{typed}\" is Ardent's \"{id}\"; asking about that",
                ));
                wanted.push(id);
            }
            ardent::Resolution::Unknown { suggestion } => {
                let hint = match suggestion {
                    Some(id) => format!(" Did you mean \"{id}\"?"),
                    None => match ardent::related_commodities(typed, catalogue, 4).as_slice() {
                        [] => " Ardent lists its ids at /v2/commodities.".to_owned(),
                        related => format!(
                            " Ardent does index {}; its full list is at /v2/commodities.",
                            related.join(", "),
                        ),
                    },
                };
                return Err(format!(
                    "--item \"{typed}\" is not a commodity Ardent indexes (it was asked for as \"{candidate}\").{hint}",
                ));
            }
        }
    }
    for category in &quick.categories {
        let mut members = Vec::new();
        for id in catalogue {
            if ardent::commodity_category(id).is_some_and(|held| held == category.as_str())
                && !members.contains(id)
            {
                members.push(id.clone());
            }
        }
        if members.is_empty() {
            return Err(format!(
                "--category \"{category}\" matches no commodity Ardent indexes"
            ));
        }
        note(format!(
            "--category \"{category}\" is {} {} Ardent indexes; asking about those",
            edm_core::js::format_integer(members.len() as f64),
            plural(members.len(), "commodity", "commodities"),
        ));
        wanted.extend(members);
    }
    // Two spellings of one commodity, or a category that contains an --item,
    // are one query, not two. `--item` order wins, then each category in the
    // order it was named, then Ardent's own catalogue order inside a category.
    let mut seen = HashSet::new();
    wanted.retain(|id| seen.insert(id.clone()));
    if wanted.is_empty() {
        return Err("--quick needs at least one commodity from --item or --category".to_owned());
    }
    Ok(wanted)
}

/// One commodity's five Ardent price pages: the name, the two nearby sides,
/// and the two reference-system sides.
type CommodityAnswer = (
    String,
    Vec<CommodityPrice>,
    Vec<CommodityPrice>,
    Vec<CommodityPrice>,
    Vec<CommodityPrice>,
);

/// Resolve docking access for every fleet carrier anywhere in the Ardent price
/// pages, once \[C36\].
///
/// `--quick` never builds the region-wide station list the full sweep filters,
/// so there is no single `Selection` to hang this off. What it has instead is
/// four price-ranked row vectors per commodity, and the same carrier routinely
/// appears in several of them — a carrier that sells Gold and buys Silver is
/// two rows and one door. Deduplicating across the whole lookup before asking
/// is the difference between one batch and one batch per commodity.
#[expect(
    clippy::too_many_arguments,
    reason = "a priced phase needs the app, its config, the cache, the pacer, the stamp pins, the price pages it draws carriers from, and the commander fact that turns an answer into a verdict"
)]
#[expect(
    clippy::too_many_lines,
    reason = "the same linear free-then-gate-then-spend sequence as the full sweep's phase, and the order is the safeguard"
)]
async fn quick_docking_access<H, C, E, F, J, T>(
    app: &App<'_, H, C, E, F>,
    config: &RouteConfig,
    cache: &Cache,
    pacer: &Pacer<'_, C, T, J>,
    stamp_overrides: &crate::cmd::StampOverrides,
    answers: &[CommodityAnswer],
    centre: &edm_core::domain::id64::Coordinates,
    ardent_requests: usize,
    commander: Option<&edm_core::domain::commander::CommanderState>,
    note: &dyn Fn(String),
) -> Result<(AccessIndex, Option<access::Report>), String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    F: Fs,
    J: Entropy,
    T: Timer,
{
    if !config.carrier_access.filters() {
        return Ok((AccessIndex::default(), None));
    }

    let mut seen = HashSet::new();
    let mut stations = Vec::new();
    for (_, exports, imports, local_exports, local_imports) in answers {
        for row in exports
            .iter()
            .chain(imports)
            .chain(local_exports)
            .chain(local_imports)
        {
            if !ardent::is_carrier(row.station.station_type.as_deref()) {
                continue;
            }
            if seen.insert(row.station.market_id.to_bits()) {
                stations.push(row.station.clone());
            }
        }
    }

    // Through the ordinary filters first. Ardent's price pages are capped at a
    // thousand rows a side and are not bounded by `--pad` or
    // `--max-star-distance`, so a wide lookup carries carriers that
    // `select_side` is about to drop for reasons that cost nothing to check.
    // Under Spansh that saved batch slots; now it saves metered requests, and
    // the rules applied here are the same ones applied there, so this can only
    // remove carriers that were leaving anyway.
    let carriers: Vec<f64> = select::select(stations, config, centre)
        .keep
        .iter()
        .map(|station| station.market_id)
        .collect();
    if carriers.is_empty() {
        return Ok((AccessIndex::default(), None));
    }

    let notoriety = commander.map_or(0.0, |state| state.notoriety);
    let cache_policy = access::CachePolicy {
        enabled: config.cache,
        refresh: config.refresh,
        max_age_minutes: Some(config.max_age_minutes),
    };
    let now_ms = app.ports.clock.now_ms();
    let mut prepared = access::prepare(
        &app.ports.fs,
        cache.root(),
        &carriers,
        now_ms,
        cache_policy,
        notoriety,
    );

    if !prepared.cold.is_empty() {
        // Gate A. `--quick` reaches its market gate at `super::plan::gate`
        // further down; this one prices the probes alone, under its own
        // heading, before any of them exists \[C37\].
        let survey = super::plan::Survey {
            complete_to_ly: config.radius_ly,
            price_index: true,
            ardent_requests: ardent_requests as u32,
            counts: Counts {
                systems: 0,
                systems_to_read: 0,
                stations_known: carriers.len(),
                markets_to_poll: 0,
                cached_fresh: 0,
                carriers_to_probe: prepared.cold.len(),
            },
            exclusions: Vec::new(),
        };
        let decision = super::plan::gate_titled(
            app.out,
            config,
            &survey,
            SizePrior::default(),
            "CARRIER ACCESS PLAN",
            super::plan::Stage::Intermediate,
        );
        if decision.ends_the_run() {
            return Err(String::new());
        }
        if decision.proceeds() {
            note(format!(
                "reading docking access for {} fleet {} from the game-internal API...",
                edm_core::js::format_integer(prepared.cold.len() as f64),
                plural(prepared.cold.len(), "carrier", "carriers"),
            ));
            // `--language` reaches the wire unvalidated, so a non-ASCII value
            // changes the envelope's byte length \[R65\].
            let language = edm_core::cli::config::starsystem_query(
                &app.cli,
                edm_core::cli::config::CachedTimestamp::SweepZero,
            )
            .map_err(|error| error.message().to_owned())?
            .language;
            let cx = access::ProbeCx {
                http: app.http,
                out: app.out,
                origin: &app.overrides.origin,
                clock: &app.ports.clock,
                entropy: &app.ports.entropy,
                credentials: &app.credentials,
                headers: &app.headers,
                language: &language,
                method_override: app.session.method_override.as_deref(),
                dry_run: config.dry_run,
                nonce_override: stamp_overrides.nonce,
                frontier_time_override: stamp_overrides.frontier_time,
                request_time_override: stamp_overrides.request_time,
            };
            let cold = std::mem::take(&mut prepared.cold);
            access::probe(
                &cx,
                pacer,
                &app.ports.fs,
                cache.root(),
                &cold,
                now_ms,
                cache_policy,
                notoriety,
                &mut prepared.index,
                &mut prepared.cost,
                None,
            )
            .await
            .map_err(|error| {
                format!("{error}\n   pass --carrier-access any to rank carriers without checking")
            })?;
        } else {
            prepared.cost.unprobed += prepared.cold.len();
            note(format!(
                "docking access was not read; {} {} ranked unchecked",
                edm_core::js::format_integer(prepared.cold.len() as f64),
                plural(prepared.cold.len(), "carrier is", "carriers are"),
            ));
        }
    }

    access::finish(
        &mut prepared.index,
        &carriers,
        commander,
        &mut prepared.cost,
    );
    let cost = prepared.cost;

    // Counted over *distinct* carriers rather than by summing the per-side
    // `apply` calls: one carrier appears in as many price pages as it trades
    // commodities, and it is still one door.
    let proven = config.carrier_access == edm_core::carrier::Policy::Proven;
    let removed = access::Removed {
        restricted: cost.restricted,
        // Counted by `apply` per side, not derivable from the deduped cost.
        moved: 0,
        unproven: if proven { cost.unknown } else { 0 },
        unproven_kept: if proven { 0 } else { cost.unknown },
    };
    note(access::note(cost, removed));
    Ok((prepared.index, Some(access::Report { cost, removed })))
}

/// English agreement for a count this module prints.
///
/// `--quick 1` is the common case, and "1 sellers" would announce a bug that is
/// not there.
const fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

/// Filter, apply ordinary route access rules, and retain a price-optimal side.
#[expect(
    clippy::too_many_arguments,
    reason = "the rows, what they must match, the ranking bound, the ship's access rules and the docking index; a struct would hide which of them is the filter"
)]
fn select_side(
    rows: Vec<CommodityPrice>,
    wanted: &str,
    direction: CommodityDirection,
    minimum: f64,
    count: usize,
    config: &RouteConfig,
    centre: &edm_core::domain::id64::Coordinates,
    docking: &AccessIndex,
) -> SideSelection {
    let mut eligible = rows
        .into_iter()
        .filter(|row| row.direction == direction)
        .filter(|row| ardent::normalise_commodity_name(&row.commodity_name) == wanted)
        .filter(|row| row.price.is_finite() && row.price > 0.0)
        .filter(|row| meets_volume(row, minimum))
        .collect::<Vec<_>>();
    // Before anything is counted. One market must not consume two of this
    // side's N slots, and must not be reported twice in the plan's arithmetic
    // either — the nearby page and the reference-system page are concatenated
    // here, and a row Ardent repeats is one market, not two candidates.
    let mut seen = HashSet::new();
    eligible.retain(|row| seen.insert(row.station.market_id.to_bits()));
    let mut station_selection = select::select(
        eligible
            .iter()
            .map(|row| row.station.clone())
            .collect::<Vec<_>>(),
        config,
        centre,
    );
    // Here rather than after the price sort, so a carrier that cannot be
    // entered gives its slot back: the hop it would have taken is re-filled by
    // the next best one instead of being silently lost from the answer.
    access::apply(&mut station_selection, docking, config.carrier_access);
    let keep = station_selection
        .keep
        .iter()
        .map(|station| station.market_id.to_bits())
        .collect::<HashSet<_>>();
    eligible.retain(|row| keep.contains(&row.station.market_id.to_bits()));
    eligible.sort_by(|left, right| {
        let price = match direction {
            CommodityDirection::Exports => left.price.total_cmp(&right.price),
            CommodityDirection::Imports => right.price.total_cmp(&left.price),
        };
        price
            .then_with(|| left.station.market_id.total_cmp(&right.station.market_id))
            .then_with(|| left.station.system_name.cmp(&right.station.system_name))
            .then_with(|| left.station.station_name.cmp(&right.station.station_name))
    });
    let ranked = eligible.len();
    let candidates = eligible
        .into_iter()
        .take(count)
        .map(|price| Candidate {
            commodity: wanted.to_owned(),
            price,
        })
        .collect::<Vec<_>>();
    // The cap is the largest filter this mode applies, and it is the one no
    // other line accounts for: without it the plan shows a considered count, two
    // small exclusions and a much smaller poll, and the arithmetic does not
    // close. Everything the price ranking dropped is nameable and raisable.
    let mut exclusions = station_selection.exclusions;
    if ranked > candidates.len() {
        exclusions.push(Exclusion {
            label: "beyond the --quick prefix",
            removed: ranked - candidates.len(),
            keep_with: "a larger --quick",
        });
    }
    SideSelection {
        candidates,
        considered: station_selection.considered,
        exclusions,
    }
}

/// Score both Ardent sides together and keep the markets of the best hops.
#[expect(
    clippy::too_many_arguments,
    reason = "the hop scorer has to see both Ardent sides, both quantity floors, the ship, and the access filters; folding those into a struct would hide which"
)]
fn select_commodity(
    exports: Vec<CommodityPrice>,
    imports: Vec<CommodityPrice>,
    wanted: &str,
    seller_minimum: f64,
    buyer_minimum: f64,
    count: usize,
    config: &RouteConfig,
    centre: &edm_core::domain::id64::Coordinates,
    cargo: f64,
    docking: &AccessIndex,
) -> SideSelection {
    let sell = select_side(
        exports,
        wanted,
        CommodityDirection::Exports,
        seller_minimum,
        usize::MAX,
        config,
        centre,
        docking,
    );
    let buy = select_side(
        imports,
        wanted,
        CommodityDirection::Imports,
        buyer_minimum,
        usize::MAX,
        config,
        centre,
        docking,
    );
    let mut exclusions = Vec::new();
    merge_exclusions(&mut exclusions, sell.exclusions);
    merge_exclusions(&mut exclusions, buy.exclusions);
    let sellers: Vec<CommodityPrice> = sell
        .candidates
        .into_iter()
        .map(|candidate| candidate.price)
        .collect();
    let buyers: Vec<CommodityPrice> = buy
        .candidates
        .into_iter()
        .map(|candidate| candidate.price)
        .collect();
    let eligible = sellers.len() + buyers.len();
    let candidates = if sellers.is_empty() || buyers.is_empty() {
        one_sided_prefix(wanted, sellers, buyers, count)
    } else {
        let nominated = nominate_hops(wanted, &sellers, &buyers, count, config, cargo);
        if nominated.is_empty() {
            one_sided_prefix(wanted, sellers, buyers, count)
        } else {
            nominated
        }
    };
    if eligible > candidates.len() {
        exclusions.push(Exclusion {
            label: "beyond the --quick prefix",
            removed: eligible - candidates.len(),
            keep_with: "a larger --quick",
        });
    }
    SideSelection {
        candidates,
        considered: sell.considered + buy.considered,
        exclusions,
    }
}

fn one_sided_prefix(
    wanted: &str,
    sellers: Vec<CommodityPrice>,
    buyers: Vec<CommodityPrice>,
    count: usize,
) -> Vec<Candidate> {
    let mut rows = take_price_prefix(wanted, sellers, CommodityDirection::Exports, count);
    rows.extend(take_price_prefix(
        wanted,
        buyers,
        CommodityDirection::Imports,
        count,
    ));
    rows
}

fn take_price_prefix(
    wanted: &str,
    mut eligible: Vec<CommodityPrice>,
    direction: CommodityDirection,
    count: usize,
) -> Vec<Candidate> {
    eligible.sort_by(|left, right| price_order(left, right, direction));
    eligible
        .into_iter()
        .take(count)
        .map(|price| Candidate {
            commodity: wanted.to_owned(),
            price,
        })
        .collect()
}

fn price_order(
    left: &CommodityPrice,
    right: &CommodityPrice,
    direction: CommodityDirection,
) -> std::cmp::Ordering {
    let price = match direction {
        CommodityDirection::Exports => left.price.total_cmp(&right.price),
        CommodityDirection::Imports => right.price.total_cmp(&left.price),
    };
    price
        .then_with(|| left.station.market_id.total_cmp(&right.station.market_id))
        .then_with(|| left.station.system_name.cmp(&right.station.system_name))
        .then_with(|| left.station.station_name.cmp(&right.station.station_name))
}

/// Keep the endpoints of the N highest first-lap-rate hops.
///
/// Rate is the live ranker's: `profit / (startup at the seller + leg to the
/// buyer)`, with `profit = (sell − buy) × min(cargo, stock, demand)`. A thin
/// 4,430 cr seller of 434 t therefore loses to a 4,543 cr seller of 19,110 t,
/// and a 67 kcr buyer 200 Ly away loses to a 50 kcr buyer 20 Ly away.
#[expect(
    clippy::too_many_lines,
    reason = "the scoring is one loop over pairs; splitting it would hide the quantity, credit, and time rules"
)]
fn nominate_hops(
    wanted: &str,
    sellers: &[CommodityPrice],
    buyers: &[CommodityPrice],
    count: usize,
    config: &RouteConfig,
    cargo: f64,
) -> Vec<Candidate> {
    let time = TimeModel {
        jump_range_ly: config.jump_range_ly,
        ..TimeModel::default()
    };
    let cargo = floor_positive(cargo);
    let credits = config.credits.and_then(floor_positive);
    let min_profit = floor_positive(config.min_profit).unwrap_or(0);
    let mut pairs: Vec<ScoredHop> = Vec::new();
    for (seller_idx, seller) in sellers.iter().enumerate() {
        let Some(buy_price) = floor_positive(seller.price) else {
            continue;
        };
        let Some(stock) = floor_positive(seller.volume) else {
            continue;
        };
        let afford = credits.map_or(i64::MAX, |balance| balance / buy_price.max(1));
        for (buyer_idx, buyer) in buyers.iter().enumerate() {
            if seller.station.market_id.to_bits() == buyer.station.market_id.to_bits() {
                continue;
            }
            let Some(sell_price) = floor_positive(buyer.price) else {
                continue;
            };
            if sell_price <= buy_price {
                continue;
            }
            let demand = if has_unpublished_import_demand(buyer) {
                cargo.unwrap_or(stock)
            } else {
                floor_positive(buyer.volume).unwrap_or(0)
            };
            let units = [cargo.unwrap_or(i64::MAX), stock, demand, afford]
                .into_iter()
                .min()
                .unwrap_or(0);
            if units < 1 {
                continue;
            }
            let profit = (sell_price - buy_price).saturating_mul(units);
            if profit < min_profit {
                continue;
            }
            let millis = hop_millis(seller, buyer, time);
            pairs.push(ScoredHop {
                seller_idx,
                buyer_idx,
                rate: Ratio::new(Credits(profit), millis),
                profit: Credits(profit),
                seller_id: seller.station.market_id,
                buyer_id: buyer.station.market_id,
            });
        }
    }
    pairs.sort_by(|left, right| {
        right
            .rate
            .cmp(&left.rate)
            .then_with(|| right.profit.cmp(&left.profit))
            .then_with(|| left.seller_id.total_cmp(&right.seller_id))
            .then_with(|| left.buyer_id.total_cmp(&right.buyer_id))
    });
    let mut seen_sell = HashSet::new();
    let mut seen_buy = HashSet::new();
    let mut out = Vec::new();
    for pair in pairs.into_iter().take(count) {
        if seen_sell.insert(pair.seller_idx) {
            out.push(Candidate {
                commodity: wanted.to_owned(),
                price: sellers[pair.seller_idx].clone(),
            });
        }
        if seen_buy.insert(pair.buyer_idx) {
            out.push(Candidate {
                commodity: wanted.to_owned(),
                price: buyers[pair.buyer_idx].clone(),
            });
        }
    }
    out.sort_by(
        |left, right| match (left.price.direction, right.price.direction) {
            (CommodityDirection::Exports, CommodityDirection::Imports) => std::cmp::Ordering::Less,
            (CommodityDirection::Imports, CommodityDirection::Exports) => {
                std::cmp::Ordering::Greater
            }
            (direction, _) => price_order(&left.price, &right.price, direction),
        },
    );
    out
}

struct ScoredHop {
    seller_idx: usize,
    buyer_idx: usize,
    rate: Ratio,
    profit: Credits,
    seller_id: f64,
    buyer_id: f64,
}

fn hop_millis(seller: &CommodityPrice, buyer: &CommodityPrice, time: TimeModel) -> Millis {
    let ly = edm_route::time::distance_ly(seller.station.coordinates, buyer.station.coordinates);
    let origin_ls = seller.station.distance_to_arrival.unwrap_or(0.0);
    let dest_ls = buyer.station.distance_to_arrival.unwrap_or(0.0);
    time.startup_millis(origin_ls) + time.leg_millis(ly, dest_ls)
}

fn floor_positive(value: f64) -> Option<i64> {
    if !value.is_finite() || value < 1.0 {
        return None;
    }
    Some(value.floor() as i64)
}

/// Whether the index row can satisfy its side's quantity rule.
///
/// A positive import `demandBracket` with zero demand is Frontier's
/// "quantity unpublished" representation. The ordinary live-ingest path
/// treats it as cargo-limited rather than as zero demand, so candidate lookup
/// must retain it too; otherwise the price-index prefix and normal ranking
/// disagree about which buyers are eligible.
fn has_unpublished_import_demand(row: &CommodityPrice) -> bool {
    row.direction == CommodityDirection::Imports
        && row.volume == 0.0
        && row.volume_bracket.is_some_and(|bracket| bracket >= 1.0)
}

fn meets_volume(row: &CommodityPrice, minimum: f64) -> bool {
    row.volume.is_finite()
        && match row.direction {
            CommodityDirection::Exports => row.volume >= minimum,
            CommodityDirection::Imports => {
                row.volume >= minimum || has_unpublished_import_demand(row)
            }
        }
}

/// Combine per-query filter ledgers into the single spend-plan explanation.
///
/// Two commodities and two sides mean up to four ledgers, and which filter each
/// one happened to trigger is a property of the data. Re-sort into the order the
/// filters actually run so the merged list still explains the arithmetic from
/// the top down; the hop-rate cap has no rank here and therefore lands last,
/// which is also when it applies.
fn merge_exclusions(into: &mut Vec<Exclusion>, from: Vec<Exclusion>) {
    for exclusion in from {
        if let Some(existing) = into
            .iter_mut()
            .find(|current| current.label == exclusion.label)
        {
            existing.removed += exclusion.removed;
        } else {
            into.push(exclusion);
        }
    }
    into.sort_by_key(|exclusion| select::exclusion_rank(exclusion.label));
}

/// Show exactly what Ardent nominated before a Frontier request can happen.
fn emit_candidates(
    out: &crate::out::Out,
    candidates: &[Candidate],
    barren: &[Barren],
    seller_minimum: f64,
    buyer_minimum: f64,
    per_side: usize,
    centre: &edm_core::domain::id64::Coordinates,
) {
    let floor = format!(
        "{} t seller / {} t published-buyer minimum",
        edm_core::js::format_integer(seller_minimum),
        edm_core::js::format_integer(buyer_minimum),
    );
    let prefix_note = "Ardent returns a price-index prefix, not a complete regional survey; hops are scored by estimated credits per hour (spread × cargo / travel time) after local station filters, and Ardent's 1,000-row per-side page cap still applies before that";
    if candidates.is_empty() {
        let mut blocks = vec![
            Block::Heading("QUICK LOOKUP  no eligible Ardent candidates".to_owned()),
            Block::Note(format!(
                "no price-index rows met the {floor} and route access filters"
            )),
        ];
        blocks.extend(barren_notes(barren));
        blocks.push(Block::Note(prefix_note.to_owned()));
        out.emit(&blocks);
        return;
    }
    let rows = candidates
        .iter()
        .map(|candidate| {
            let station = &candidate.price.station;
            Row::data([
                candidate.commodity.clone(),
                candidate.price.direction.market_role().to_owned(),
                edm_core::js::format_integer(candidate.price.price),
                if has_unpublished_import_demand(&candidate.price) {
                    "unreported".to_owned()
                } else {
                    edm_core::js::format_integer(candidate.price.volume)
                },
                edm_core::js::js_number(station.market_id),
                station.station_name.clone(),
                station.system_name.clone(),
                edm_core::js::to_fixed_1(edm_route::time::distance_ly(
                    *centre,
                    station.coordinates,
                )),
            ])
        })
        .collect();
    let mut blocks = vec![Block::Table {
        title: format!(
            "QUICK LOOKUP  {} hop-ranked candidate{} (up to {} best {} per commodity; {floor})",
            candidates.len(),
            if candidates.len() == 1 { "" } else { "s" },
            edm_core::js::format_integer(per_side as f64),
            plural(per_side, "hop", "hops"),
        ),
        columns: columns::QUICK_LOOKUP_COLUMNS,
        rows,
    }];
    if candidates
        .iter()
        .any(|candidate| has_unpublished_import_demand(&candidate.price))
    {
        blocks.push(Block::Note(
            "a buyer marked unreported has a positive demand bracket but no published tonnage; live ranking treats it as cargo-limited"
                .to_owned(),
        ));
    }
    blocks.extend(barren_notes(barren));
    let distinct = candidates
        .iter()
        .map(|candidate| candidate.price.station.market_id.to_bits())
        .collect::<HashSet<_>>()
        .len();
    if distinct < candidates.len() {
        // Otherwise "markets to poll" below is smaller than this table and
        // nothing says why. A market that sells one commodity and buys another
        // is one listing and one request, however many rows nominated it.
        blocks.push(Block::Note(format!(
            "{} of these rows name a market another row already names; each market is read once, so {} {} polled",
            edm_core::js::format_integer((candidates.len() - distinct) as f64),
            edm_core::js::format_integer(distinct as f64),
            if distinct == 1 { "is" } else { "are" },
        )));
    }
    blocks.push(Block::Note(prefix_note.to_owned()));
    out.emit(&blocks);
}

/// Name each `--item` that contributed nothing, and say which kind of nothing.
///
/// Silence here is the failure mode that matters: `--item gold,unobtainium`
/// otherwise prints a table of gold and never mentions that half the request
/// was a typo.
fn barren_notes(barren: &[Barren]) -> Vec<Block<'static>> {
    barren
        .iter()
        .map(|item| {
            Block::Note(if item.indexed {
                format!(
                    "\"{}\": Ardent has price rows for it, but none survived the quantity floor and access filters",
                    item.commodity,
                )
            } else {
                format!(
                    "\"{}\": Ardent's price index returned no row at all — check the name against its commodity ids, or widen --radius",
                    item.commodity,
                )
            })
        })
        .collect()
}

/// Print the answer the lookup was asked for, in the order `--item` named.
fn emit_live_prices(out: &crate::out::Out, best: &[BestLive], wanted: &[String]) {
    if best.is_empty() {
        return;
    }
    let mut rows = Vec::new();
    for commodity in wanted {
        for direction in [CommodityDirection::Exports, CommodityDirection::Imports] {
            let Some(entry) = best
                .iter()
                .find(|entry| entry.commodity == *commodity && entry.direction == direction)
            else {
                continue;
            };
            rows.push(Row::data([
                entry.display.clone(),
                direction.market_role().to_owned(),
                edm_core::js::format_integer(entry.price),
                if entry.unpublished {
                    "unreported".to_owned()
                } else {
                    edm_core::js::format_integer(entry.volume)
                },
                entry
                    .index_price
                    .map_or_else(|| "-".to_owned(), edm_core::js::format_integer),
                entry.station.clone(),
                entry.system.clone(),
                edm_core::js::to_fixed_1(entry.distance_ly),
            ]));
        }
    }
    // The heading is decided by the rows, not by the mode. `--quick` used to
    // assert "read live this run" unconditionally, which was true only while it
    // polled every market it ranked; a cache-seeded run must not inherit the
    // claim \[C38\].
    let cached = best.iter().filter(|entry| entry.from_cache).count();
    let title = if cached == 0 {
        "BEST LIVE PRICES  where to buy and sell each commodity, read live this run".to_owned()
    } else {
        format!(
            "BEST PRICES  where to buy and sell each commodity; {} of {} read live this run",
            edm_core::js::format_integer((best.len() - cached) as f64),
            edm_core::js::format_integer(best.len() as f64),
        )
    };
    let mut blocks = vec![Block::Table {
        title,
        columns: columns::QUICK_LIVE_COLUMNS,
        rows,
    }];
    blocks.push(Block::Note(
        "best among the markets this run polled, which Ardent's price index chose: a market its index does not carry cannot appear here, however good its price"
            .to_owned(),
    ));
    if cached > 0 {
        blocks.push(Block::Note(
            "the rest were reused from the local cache; only the markets behind the ranked routes below are re-read live, so a row here can be older than the route it supports"
                .to_owned(),
        ));
    }
    if best.iter().any(|entry| entry.index_price.is_none()) {
        blocks.push(Block::Note(
            "a blank index price means this side of the market was not what nominated it"
                .to_owned(),
        ));
    }
    out.emit(&blocks);
}

/// The subset coverage a quick lookup can honestly claim.
#[expect(
    clippy::too_many_arguments,
    reason = "a coverage block is a report of what happened, and each argument is one measured fact it states"
)]
fn coverage_of(
    acquired: &acquire::Acquired,
    verified_markets: usize,
    markets_found: usize,
    eddn_enabled: bool,
    spent: crate::route::pacer::Spent,
    breaker_tripped: bool,
    elapsed_seconds: f64,
    measured_at_ms: f64,
) -> RouteCoverage {
    let oldest_observed_ms = acquired
        .listings
        .iter()
        .filter_map(|listing| listing.observed_at_ms)
        .min_by(f64::total_cmp);
    let newest_observed_ms = acquired
        .listings
        .iter()
        .filter_map(|listing| listing.observed_at_ms)
        .fold(None::<f64>, |newest, observed| {
            Some(newest.map_or(observed, |current| {
                if current.total_cmp(&observed).is_lt() {
                    observed
                } else {
                    current
                }
            }))
        });
    RouteCoverage {
        markets_found,
        // The verify pass polls too, and its reads are the ones the ranked
        // routes are actually made of. Leaving them out let a fully cache-seeded
        // run report "0 of 4" polled in the same block as four requests sent.
        markets_polled: acquired.tally.markets_polled
            + acquired.tally.markets_absent
            + verified_markets,
        markets_priced: super::ingest::priced(&acquired.listings),
        markets_failed: acquired.tally.markets_failed,
        markets_absent: acquired.tally.markets_absent,
        eddn: eddn_enabled.then_some(EddnCoverage {
            sent: acquired.relayed.sent,
            failed: acquired.relayed.failed,
            recent: acquired.relayed.recent,
            cached: acquired.relayed.cached,
            unnamed: acquired.relayed.unnamed,
            abandoned: acquired.relayed.abandoned,
        }),
        cache_hits: acquired.cache.fresh,
        requests_sent: spent.requests,
        throttled: spent.throttled,
        elapsed_seconds,
        oldest_observed_ms,
        newest_observed_ms,
        observation_time_unknown: acquired
            .listings
            .iter()
            .filter(|listing| listing.observed_at_ms.is_none())
            .count(),
        measured_at_ms,
        breaker_tripped,
        ranked: true,
        eddn_refusal: acquired.relayed.first_refusal.clone(),
        ..RouteCoverage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edm_core::ardent::ArdentStation;
    use edm_core::domain::id64::Coordinates;

    fn station(market_id: f64, x: f64) -> ArdentStation {
        ArdentStation {
            market_id,
            station_name: format!("Station {market_id}"),
            system_name: "Sol".to_owned(),
            system_address: 10_477_373_803.0,
            station_type: Some("Coriolis".to_owned()),
            max_landing_pad_size: Some(3.0),
            distance_to_arrival: Some(10.0),
            coordinates: Coordinates { x, y: 0.0, z: 0.0 },
        }
    }

    fn row(price: f64, volume: f64, market_id: f64) -> CommodityPrice {
        CommodityPrice {
            commodity_name: "gold".to_owned(),
            direction: CommodityDirection::Exports,
            price,
            volume,
            volume_bracket: None,
            station: station(market_id, market_id),
        }
    }

    fn unpublished_import(price: f64, market_id: f64) -> CommodityPrice {
        CommodityPrice {
            commodity_name: "gold".to_owned(),
            direction: CommodityDirection::Imports,
            price,
            volume: 0.0,
            volume_bracket: Some(1.0),
            station: station(market_id, market_id),
        }
    }

    fn config(argv: &[&str]) -> RouteConfig {
        let argv = argv.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let parsed = edm_core::cli::parse_with(&argv, edm_core::cli::Table::Extended)
            .expect("quick route parses");
        let env = edm_core::cli::EnvSnapshot::empty();
        edm_core::cli::config::route_config(&edm_core::cli::Cli::new(&parsed, &env))
            .expect("quick route config")
    }

    #[test]
    fn exports_take_the_lowest_price_after_the_quantity_floor() {
        let config = config(&["route", "Sol", "--quick", "2", "--item", "gold"]);
        let side = select_side(
            vec![
                row(10.0, 50.0, 1.0),
                row(5.0, 9.0, 2.0),
                row(7.0, 50.0, 3.0),
            ],
            "gold",
            CommodityDirection::Exports,
            10.0,
            2,
            &config,
            &Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            &AccessIndex::default(),
        );
        assert_eq!(side.candidates.len(), 2);
        assert_eq!(side.candidates[0].price.price, 7.0);
        assert_eq!(side.candidates[1].price.price, 10.0);
    }

    #[test]
    fn the_per_side_cap_is_named_in_the_ledger_rather_than_left_to_arithmetic() {
        let config = config(&["route", "Sol", "--quick", "1", "--item", "gold"]);
        let side = select_side(
            vec![
                row(7.0, 50.0, 1.0),
                row(10.0, 50.0, 2.0),
                row(12.0, 50.0, 3.0),
                // The same market twice is one candidate and one ledger entry,
                // not two of either.
                row(7.0, 50.0, 1.0),
            ],
            "gold",
            CommodityDirection::Exports,
            10.0,
            1,
            &config,
            &Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            &AccessIndex::default(),
        );
        assert_eq!(
            side.considered, 3,
            "the repeated row is not a fourth market"
        );
        assert_eq!(side.candidates.len(), 1);
        let cap = side
            .exclusions
            .iter()
            .find(|exclusion| exclusion.label == "beyond the --quick prefix")
            .expect("the cap explains itself");
        assert_eq!(cap.removed, 2);
        assert_eq!(
            side.considered - side.exclusions.iter().map(|e| e.removed).sum::<usize>(),
            side.candidates.len(),
            "the plan's subtraction has to reach the candidate count"
        );
    }

    fn payload(name: &str, buy: &str, stock: &str, sell: &str, demand: &str) -> String {
        format!(
            r#"{{"commodities":{{"1":{{"id":1,"name":"{name}","categoryname":"Metals",
               "buyPrice":{buy},"stock":{stock},"stockBracket":3,
               "sellPrice":{sell},"demand":{demand},"demandBracket":3,"meanPrice":9000}}}},
               "inventory":[]}}"#
        )
    }

    fn listing(market_id: f64, station: &str, body: &str) -> crate::route::acquire::Listing {
        crate::route::acquire::Listing {
            market_id,
            station_name: station.to_owned(),
            system_name: "Sol".to_owned(),
            document: edm_core::js::json::JsValue::parse(body).expect("a market payload"),
            read_at_ms: 0.0,
            observed_at_ms: None,
            from_cache: false,
        }
    }

    #[test]
    fn the_live_table_picks_the_cheapest_seller_and_never_a_row_the_ranking_would_drop() {
        let stations = [station(1.0, 0.0), station(2.0, 3.0), station(3.0, 0.0)];
        let listings = [
            listing(1.0, "Dearer", &payload("Gold", "9000", "5000", "0", "0")),
            listing(2.0, "Cheaper", &payload("Gold", "8000", "5000", "0", "0")),
            // A fractional stock is exactly what the optimiser refuses, so the
            // table above it may not advertise this market's better price.
            listing(
                3.0,
                "Fractional",
                &payload("Gold", "10", "5000.5", "0", "0"),
            ),
        ];
        let best = best_live_prices(
            &listings,
            &[],
            &stations,
            &["gold".to_owned()],
            100.0,
            100.0,
            false,
            &Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        );
        assert_eq!(best.len(), 1, "one commodity, one side with any stock");
        assert_eq!(best[0].station, "Cheaper");
        assert_eq!(best[0].price, 8_000.0);
        assert_eq!(best[0].direction, CommodityDirection::Exports);
        assert_eq!(best[0].distance_ly, 3.0);
        assert_eq!(best[0].index_price, None, "no candidate row nominated it");
    }

    #[test]
    fn an_unpublished_buyer_survives_the_demand_floor_it_cannot_answer() {
        let stations = [station(1.0, 0.0)];
        let listings = [listing(
            1.0,
            "Quiet",
            r#"{"commodities":{"1":{"id":1,"name":"Gold","categoryname":"Metals",
               "buyPrice":0,"stock":0,"stockBracket":0,
               "sellPrice":11500,"demand":0,"demandBracket":2,"meanPrice":9000}},
               "inventory":[]}"#,
        )];
        let best = best_live_prices(
            &listings,
            &[],
            &stations,
            &["gold".to_owned()],
            100.0,
            100.0,
            false,
            &Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        );
        assert_eq!(best.len(), 1);
        assert!(best[0].unpublished);
        assert_eq!(best[0].price, 11_500.0);
    }

    #[test]
    fn every_item_is_resolved_against_the_catalogue_before_a_query_is_spent() {
        let catalogue = [
            "gold".to_owned(),
            "lowtemperaturediamond".to_owned(),
            "silver".to_owned(),
        ];
        let said = std::cell::RefCell::new(Vec::new());
        let note = |text: String| said.borrow_mut().push(text);

        let parsed = config(&["route", "Sol", "--quick", "1", "--item", "Gold"]);
        let quick = parsed.quick.expect("quick settings");
        assert_eq!(
            resolve_items(&quick, &catalogue, &note),
            Ok(vec!["gold".to_owned()])
        );
        assert!(said.borrow().is_empty(), "an exact id needs no explanation");

        // The display name pluralises Ardent's symbol. Resolved, and said.
        let parsed = config(&[
            "route",
            "Sol",
            "--quick",
            "1",
            "--item",
            "Low Temperature Diamonds",
        ]);
        let quick = parsed.quick.expect("quick settings");
        assert_eq!(
            resolve_items(&quick, &catalogue, &note),
            Ok(vec!["lowtemperaturediamond".to_owned()])
        );
        assert!(
            said.borrow()
                .last()
                .is_some_and(|line| line.contains("lowtemperaturediamond")),
            "{:?}",
            said.borrow()
        );

        // Two spellings of one commodity are one query.
        let parsed = config(&[
            "route",
            "Sol",
            "--quick",
            "1",
            "--item",
            "gold, Low Temperature Diamonds, lowtemperaturediamond",
        ]);
        let quick = parsed.quick.expect("quick settings");
        assert_eq!(
            resolve_items(&quick, &catalogue, &note),
            Ok(vec!["gold".to_owned(), "lowtemperaturediamond".to_owned()])
        );

        // An unknown id is refused with the spelling that was typed, not with
        // the normalisation of it, and with the nearest id Ardent does index.
        let parsed = config(&["route", "Sol", "--quick", "1", "--item", "Gild"]);
        let quick = parsed.quick.expect("quick settings");
        let refusal = resolve_items(&quick, &catalogue, &note).expect_err("unknown commodity");
        assert!(refusal.contains("\"Gild\""), "{refusal}");
        assert!(refusal.contains("Did you mean \"gold\"?"), "{refusal}");

        // An empty catalogue is an outage, not a galaxy without commodities.
        let parsed = config(&["route", "Sol", "--quick", "1", "--item", "gold"]);
        let quick = parsed.quick.expect("quick settings");
        assert!(resolve_items(&quick, &[], &note).is_err());
    }

    #[test]
    fn the_merged_ledger_reads_in_the_order_the_filters_ran() {
        // Deliberately merged in the wrong order: the data decides which side
        // triggers which filter, and the plan may not inherit that accident.
        let mut merged = Vec::new();
        merge_exclusions(
            &mut merged,
            vec![
                Exclusion {
                    label: "beyond the --quick prefix",
                    removed: 4,
                    keep_with: "a larger --quick",
                },
                Exclusion {
                    label: "outside the radius",
                    removed: 1,
                    keep_with: "--radius",
                },
            ],
        );
        merge_exclusions(
            &mut merged,
            vec![
                Exclusion {
                    label: "Odyssey settlements",
                    removed: 2,
                    keep_with: "--settlements",
                },
                Exclusion {
                    label: "beyond the --quick prefix",
                    removed: 3,
                    keep_with: "a larger --quick",
                },
            ],
        );
        assert_eq!(
            merged.iter().map(|e| e.label).collect::<Vec<_>>(),
            [
                "Odyssey settlements",
                "outside the radius",
                "beyond the --quick prefix"
            ]
        );
        assert_eq!(
            merged.last().expect("the cap").removed,
            7,
            "both sides' caps add up"
        );
    }

    #[test]
    fn an_unpublished_import_demand_is_selected_without_claiming_zero_tons() {
        let config = config(&["route", "Sol", "--quick", "1", "--item", "gold"]);
        let side = select_side(
            vec![unpublished_import(50_000.0, 5.0)],
            "gold",
            CommodityDirection::Imports,
            100.0,
            1,
            &config,
            &Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            &AccessIndex::default(),
        );
        assert_eq!(side.candidates.len(), 1);
        assert!(has_unpublished_import_demand(&side.candidates[0].price));
    }

    fn buy(price: f64, volume: f64, market_id: f64, x: f64) -> CommodityPrice {
        CommodityPrice {
            commodity_name: "gold".to_owned(),
            direction: CommodityDirection::Imports,
            price,
            volume,
            volume_bracket: None,
            station: station(market_id, x),
        }
    }

    fn sell(price: f64, volume: f64, market_id: f64, x: f64) -> CommodityPrice {
        CommodityPrice {
            commodity_name: "gold".to_owned(),
            direction: CommodityDirection::Exports,
            price,
            volume,
            volume_bracket: None,
            station: station(market_id, x),
        }
    }

    #[test]
    fn a_full_hold_at_a_slightly_dearer_pad_beats_a_thin_cheaper_one() {
        // Linnehan-style: cheapest unit price, not enough stock to fill the
        // hold. Marino-style: a few credits more, 19 kt on the pad. `--quick 1`
        // used to keep the thin pad because Ardent sorts by price.
        let config = config(&[
            "route", "Sol", "--quick", "1", "--item", "gold", "--qty", "100", "--cargo", "1232",
        ]);
        let selected = select_commodity(
            vec![
                sell(1_000.0, 200.0, 1.0, 0.0),
                sell(1_100.0, 5_000.0, 2.0, 0.0),
            ],
            vec![buy(50_000.0, 5_000.0, 3.0, 10.0)],
            "gold",
            100.0,
            100.0,
            1,
            &config,
            &Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            1_232.0,
            &AccessIndex::default(),
        );
        let sellers: Vec<f64> = selected
            .candidates
            .iter()
            .filter(|row| row.price.direction == CommodityDirection::Exports)
            .map(|row| row.price.station.market_id)
            .collect();
        assert_eq!(sellers, [2.0], "the full pad is the hop that actually pays");
        assert!(
            selected
                .candidates
                .iter()
                .any(|row| row.price.station.market_id == 3.0),
            "the buyer of that hop is polled too"
        );
    }

    #[test]
    fn a_nearer_slightly_worse_buyer_beats_a_distant_headline_price() {
        let config = config(&[
            "route", "Sol", "--quick", "1", "--item", "gold", "--qty", "100", "--cargo", "1232",
            "--jump", "30", "--radius", "500",
        ]);
        let selected = select_commodity(
            vec![sell(1_000.0, 5_000.0, 1.0, 0.0)],
            vec![
                buy(60_000.0, 5_000.0, 2.0, 300.0),
                buy(50_000.0, 5_000.0, 3.0, 10.0),
            ],
            "gold",
            100.0,
            100.0,
            1,
            &config,
            &Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            1_232.0,
            &AccessIndex::default(),
        );
        let buyers: Vec<f64> = selected
            .candidates
            .iter()
            .filter(|row| row.price.direction == CommodityDirection::Imports)
            .map(|row| row.price.station.market_id)
            .collect();
        assert_eq!(
            buyers,
            [3.0],
            "credits per hour prefers 50 kcr at 10 Ly over 60 kcr at 300 Ly"
        );
    }

    #[test]
    fn quick_config_normalises_items_and_derives_one_tenth_of_cargo() {
        let parsed_config = config(&[
            "route",
            "Sol",
            "--quick",
            "3",
            "--item",
            "Gold, Low-Temperature Diamonds, gold",
            "--cargo",
            "784",
        ]);
        let cargo = parsed_config.cargo;
        let shape = parsed_config.shape;
        let quick = parsed_config.quick.expect("quick settings");
        // The parse stage only normalises. "lowtemperaturediamonds" is the
        // display name's plural and not Ardent's id; `resolve_items` is what
        // turns it into `lowtemperaturediamond` against the live catalogue.
        assert_eq!(quick.commodities, ["gold", "lowtemperaturediamonds"]);
        assert_eq!(quick.raw, ["Gold", "Low-Temperature Diamonds"]);
        assert_eq!(quick.markets_per_side, 3);
        assert_eq!(quick.minimum_quantity(cargo), 79.0);
        assert_eq!(shape, edm_core::cli::config::Shape::OneWay);

        let explicit = config(&[
            "route", "Sol", "--quick", "1", "--item", "gold", "--qty", "12", "--shape", "one-way",
        ]);
        assert_eq!(explicit.shape, edm_core::cli::config::Shape::OneWay);
        let quick = explicit.quick.expect("quick settings");
        assert_eq!(quick.minimum_quantity(Some(784.0)), 12.0);
    }

    #[test]
    fn quick_defaults_to_a_hop_even_when_a_cycle_could_exist() {
        // `--quick` is a lookup of where to buy and sell. Defaulting a metals
        // class to a round trip hid a 77 Mcr gold hop behind a 30 Mcr cycle.
        let single = config(&["route", "Sol", "--quick", "2", "--item", "gold"]);
        assert_eq!(single.shape, edm_core::cli::config::Shape::OneWay);
        assert!(single.quick.expect("quick settings").cannot_cycle());

        let pair = config(&["route", "Sol", "--quick", "2", "--item", "gold,silver"]);
        assert_eq!(pair.shape, edm_core::cli::config::Shape::OneWay);
        assert!(!pair.quick.expect("quick settings").cannot_cycle());

        let by_class = config(&["route", "Sol", "--quick", "2", "--category", "metals"]);
        assert_eq!(by_class.shape, edm_core::cli::config::Shape::OneWay);
        assert!(!by_class.quick.expect("quick settings").cannot_cycle());

        // An explicit shape is never overridden, however futile it is.
        let asked = config(&[
            "route", "Sol", "--quick", "2", "--item", "gold", "--shape", "loop:3",
        ]);
        assert_eq!(asked.shape, edm_core::cli::config::Shape::BoundedLoop(3));
        assert!(asked.shape.is_cycle());

        // A route with no --quick keeps the round trip it has always had.
        let plain = config(&["route", "Sol"]);
        assert_eq!(plain.shape, edm_core::cli::config::Shape::RoundTrip);
    }

    #[test]
    fn a_category_expands_to_the_indexed_commodities_in_it() {
        let catalogue = [
            "gold".to_owned(),
            "unobtainium".to_owned(),
            "silver".to_owned(),
            "painite".to_owned(),
            "algae".to_owned(),
        ];
        let said = std::cell::RefCell::new(Vec::new());
        let note = |text: String| said.borrow_mut().push(text);

        let parsed = config(&["route", "Sol", "--quick", "1", "--category", "metals"]);
        let quick = parsed.quick.expect("quick settings");
        assert_eq!(
            resolve_items(&quick, &catalogue, &note),
            Ok(vec!["gold".to_owned(), "silver".to_owned()])
        );
        assert!(
            said.borrow()
                .iter()
                .any(|line| line.contains("--category \"Metals\" is 2 commodities")),
            "{:?}",
            said.borrow()
        );

        // Catalogue order inside a category, category order across them, and
        // an --item already in the class is not queried twice.
        said.borrow_mut().clear();
        let parsed = config(&[
            "route",
            "Sol",
            "--quick",
            "1",
            "--item",
            "gold",
            "--category",
            "minerals,metals",
        ]);
        let quick = parsed.quick.expect("quick settings");
        assert_eq!(
            resolve_items(&quick, &catalogue, &note),
            Ok(vec![
                "gold".to_owned(),
                "painite".to_owned(),
                "silver".to_owned(),
            ])
        );

        // A known class this catalogue does not actually index is an outage,
        // not a successful empty lookup.
        let parsed = config(&["route", "Sol", "--quick", "1", "--category", "salvage"]);
        let quick = parsed.quick.expect("quick settings");
        let refusal = resolve_items(&quick, &catalogue, &note).expect_err("empty class");
        assert!(refusal.contains("Salvage"), "{refusal}");
        assert!(
            refusal.contains("matches no commodity Ardent indexes"),
            "{refusal}"
        );
    }
}
