//! `trade` — one request, or a batch that fills a hold.
//!
//! This is the only command that spends money, and the only one whose control
//! flow is a loop. The loop itself is [`edm_core::domain::batch`], a state
//! machine whose every decision is a value; what is left here is the I/O each
//! [`Step`] asks for. Keeping the split in that place is what makes the
//! round-end order, the three-failure limit and the sleep placement testable
//! without a socket \[R90\].

use std::borrow::Cow;

use edm_core::cli::Flag;
use edm_core::cli::config::{self, PlanField, PlanSource};
use edm_core::consts::MARKET_TRADE;
use edm_core::domain::batch::{self, Batch, Reply, Step};
use edm_core::domain::trade::TradePlan;
use edm_core::domain::{self, Commodity, MarketSnapshot, parse_market_snapshot};
use edm_core::js::json::JsValue;
use edm_core::js::{format_quantity, js_number};
use edm_core::render::{Block, Row, columns, views};

use crate::game_api::{self, Field};
use crate::exchange::SendOptions;
use crate::net::HttpTransport;
use crate::ports::{Clock, Entropy, Fs};

use super::{
    App, CmdResult, decrypted, message, num_or_null, object, str_value, timer_duration,
};

/// `runTrade` (ts:2327).
pub async fn run<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
) -> CmdResult {
    let dispatch = config::trade_dispatch(&app.cli).map_err(message)?;
    if dispatch.batch { batch_trade(app, dispatch.items).await } else { single(app).await }
}

// ---------------------------------------------------------------------------
// One trade
// ---------------------------------------------------------------------------

/// `runSingleTrade` (ts:1945).
async fn single<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
) -> CmdResult {
    let inputs = config::trade_inputs(&app.cli).map_err(message)?;

    let mut document = None;
    if let Some(market_id) = &inputs.market_id {
        if !app.session.json {
            // ts:1951
            app.note(format!(
                "resolving against {} for market {market_id}...",
                edm_core::cli::usage::MARKET_LIST_PATH
            ));
        }
        // The listing is read-only, so it runs under `--dry-run` too \[R74\].
        document = Some(app.require_market_snapshot(market_id).await?);
    }
    let snapshot = document.as_ref().and_then(parse_market_snapshot);

    // `resolveTrade` re-reads the *unsplit* `--item`, so a trailing comma makes
    // this look up a commodity literally named `gold,` \[R54\].
    let resolved = config::resolve_trade(&app.cli, snapshot.as_ref()).map_err(message)?;
    if !app.session.json {
        let fields: Vec<views::PlanField<'_>> = resolved.fields.iter().map(plan_field).collect();
        app.out.emit(&views::trade_plan(
            resolved.plan.kind,
            resolved.plan.qty,
            &resolved.plan.commodity_name,
            &fields,
            &resolved.notes,
        ));
    }

    let stamp = app.stamp()?;
    let request = app.prepare(
        MARKET_TRADE,
        trade_fields(&resolved.plan, &app.credentials, stamp.frontier_time),
        stamp,
    );
    let exchange = app.send(&request, SendOptions::default()).await;

    if app.session.json {
        app.emit_json(&request, exchange.as_ref(), vec![("plan", plan_json(&resolved.plan))]);
        return Ok(());
    }
    let Some(text) = decrypted(exchange.as_ref()) else { return Ok(()) };

    let document = JsValue::parse(text).ok();
    let Some(result) = document.as_ref().and_then(parse_market_snapshot) else {
        app.out.emit(&views::opaque_payload(text));
        return Ok(());
    };

    app.out.emit(&views::market_summary(
        &result,
        // ts:1981
        &format!(
            "TRADE RESULT  {} {}",
            resolved.plan.kind.as_str(),
            resolved.plan.commodity_name
        ),
    ));
    app.out.emit(&views::inventory_table(result.inventory));

    if let Some(traded) = result.by_id(resolved.plan.commodity_id) {
        // ts:1986 — a one-row commodity table, which is the only place the
        // program prints that column set outside the grouped listing.
        app.out.emit(&[Block::Table {
            title: format!("{}  after the trade", traded.name.to_uppercase()),
            columns: columns::COMMODITY_COLUMNS,
            rows: vec![commodity_row(traded)],
        }]);
    }

    if app.cli.switch_value(Flag::FullMarket, false).map_err(message)? {
        app.out.emit(&views::commodity_table(&result.commodities));
    } else {
        // ts:1989
        app.note(
            "pass --full-market to print the whole commodity table from this response".to_owned(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// A batch
// ---------------------------------------------------------------------------

/// `runBatchTrade` (ts:2091) — the driver for [`edm_core::domain::batch`].
///
/// Every branch of the `match` is one thing the TypeScript does between two
/// decisions, and nothing here decides anything: the state machine owns the
/// round-end ladder, the failure counter and the skip reasons.
async fn batch_trade<H: HttpTransport, C: Clock, E: Entropy, F: Fs>(
    app: &App<'_, H, C, E, F>,
    items: Vec<String>,
) -> CmdResult {
    let settings = config::batch_config(&app.cli, items).map_err(message)?;
    let mut document = app.require_market_snapshot(&settings.market_id).await?;

    let mut machine = {
        let opening = listing(&document)?;
        Batch::new(loop_config(&settings, app.session.dry_run, app.session.json), &opening)?
    };

    if !app.session.json {
        app.out.emit(&[Block::Table {
            title: "BATCH PLAN".to_owned(),
            columns: columns::FIELD_COLUMNS,
            rows: machine.plan_rows(),
        }]);
    }

    let mut reply: Option<Reply> = None;
    loop {
        let step = {
            let latest = listing(&document)?;
            machine.next(&latest, reply.take())
        };
        match step {
            // A refresh that fails **throws**: no TRADES table, no JSON, exit 1
            // \[R90\].
            Step::Refresh => document = app.require_market_snapshot(&settings.market_id).await?,
            Step::Trade(plan) => {
                // R90: the stamp is drawn — and the request prepared — *before*
                // the dry-run branch, so the entropy stream advances by exactly
                // one stamp per planned trade either way.
                let stamp = app.stamp()?;
                let request = app.prepare(
                    MARKET_TRADE,
                    trade_fields(&plan, &app.credentials, stamp.frontier_time),
                    stamp,
                );
                if app.session.dry_run {
                    // Nothing is sent; the machine simulates the fill locally.
                    continue;
                }
                let exchange =
                    app.send(&request, SendOptions { quiet: true, ignore_dry_run: false }).await;
                reply = Some(match exchange.as_ref() {
                    // ts:2235 — `!exchange` and a null `decrypted` are the two
                    // failures; a body that decoded into something that is not
                    // a listing is a *successful* trade about which nothing was
                    // learned.
                    None => Reply::Failed { status: None },
                    Some(exchange) => match exchange.decrypted.as_deref() {
                        None => Reply::Failed { status: Some(exchange.status) },
                        Some(text) => {
                            match JsValue::parse(text)
                                .ok()
                                .filter(|value| parse_market_snapshot(value).is_some())
                            {
                                Some(parsed) => {
                                    document = parsed;
                                    Reply::Listing { status: exchange.status }
                                }
                                None => Reply::Opaque { status: exchange.status },
                            }
                        }
                    },
                });
            }
            // Not printed: the reasons accumulate and only the first three
            // reach the waiting line of an idle round.
            Step::Skip { .. } => {}
            Step::Progress(line) => app.out.progress(&line),
            Step::Sleep { millis } => tokio::time::sleep(timer_duration(millis)).await,
            Step::Done(_) => break,
        }
    }

    let latest = listing(&document)?;
    // `+ 0.0` for the same reason `domain::batch::hold_used` does it: the shared
    // fold's identity is `-0.0`, and `formatInteger(-0)` is `"-0"`, so an empty
    // hold would otherwise close the table with `cargo -0/200`.
    let final_used = domain::cargo_used(latest.inventory) + 0.0;
    let report = machine.report();

    if app.session.json {
        // ts:2276
        app.out.line(
            &object([
                ("plan".to_owned(), batch_plan_json(&settings, machine.targets())),
                ("outcome".to_owned(), str_value(&report.outcome)),
                ("rounds".to_owned(), JsValue::Num(f64::from(report.rounds))),
                (
                    "trades".to_owned(),
                    JsValue::Arr(report.trades.iter().map(record_json).collect()),
                ),
                ("credits".to_owned(), num_or_null(report.credits)),
                ("cargoUsed".to_owned(), JsValue::Num(final_used)),
                ("inventory".to_owned(), JsValue::Arr(latest.inventory.to_vec())),
            ])
            .stringify(2),
        );
        return Ok(());
    }

    app.out.emit(&[Block::Table {
        title: machine.trades_title(),
        columns: columns::TRADE_LOG_COLUMNS,
        rows: machine.trades_rows(final_used),
    }]);
    if let Some(note) = machine.credits_note() {
        app.note(note);
    }
    app.out.emit(&views::inventory_table(latest.inventory));
    Ok(())
}

/// The listing every step is evaluated against.
///
/// Cannot fail in practice — a document only reaches this after
/// `parse_market_snapshot` has already accepted it — but the alternative to a
/// `Result` here is an `expect`, and a command-line tool has no business
/// panicking over a payload.
fn listing(document: &JsValue) -> Result<MarketSnapshot<'_>, String> {
    parse_market_snapshot(document)
        // ts:1939, the message the same shape failure produces upstream.
        .ok_or_else(|| "Market listing did not contain commodity data".to_owned())
}

/// The loop's copy of the settings, plus the two session flags it reads.
fn loop_config(
    settings: &config::BatchConfig,
    dry_run: bool,
    json: bool,
) -> batch::BatchConfig {
    batch::BatchConfig {
        market_id: settings.market_id.clone(),
        kind: settings.kind,
        items: settings.items.clone(),
        fill: settings.fill,
        cargo: settings.cargo,
        per_item_qty: settings.per_item_qty,
        stolen: settings.stolen,
        explicit_black_market: settings.explicit_black_market,
        explicit_price: settings.explicit_price,
        watch: settings.watch,
        interval_ms: settings.interval_ms,
        attempt_limit: settings.attempt_limit,
        credits: settings.credits,
        dry_run,
        json,
    }
}

// ---------------------------------------------------------------------------
// Envelope and rendering
// ---------------------------------------------------------------------------

/// `tradeEnvelopeFields` (ts:249).
///
/// `marketId` is a string and reaches the wire verbatim — `trade` never parses
/// it, so `0004306502403` keeps its leading zeros \[R53\]. The booleans go as
/// `1`/`0` **numbers**, not as strings.
fn trade_fields(plan: &TradePlan, credentials: &game_api::Credentials, frontier_time: f64) -> Vec<Field> {
    let mut fields = vec![
        Field::text("cmdrId", credentials.commander_id.clone()),
        Field::text("marketId", plan.market_id.clone()),
        Field::text("transactionType", plan.kind.as_str()),
        Field::number("commodityId", plan.commodity_id),
        Field::number("blackMarket", f64::from(u8::from(plan.black_market))),
        Field::number("stolen", f64::from(u8::from(plan.stolen))),
        Field::number("unitPrice", plan.unit_price),
        Field::number("qty", plan.qty),
        Field::number("finalQty", plan.final_qty),
    ];
    fields.extend(game_api::credential_fields(credentials, frontier_time));
    fields
}

fn plan_field(field: &PlanField) -> views::PlanField<'_> {
    views::PlanField {
        label: field.label,
        value: Cow::Borrowed(&field.value),
        source: match field.source {
            PlanSource::Flag => "flag",
            PlanSource::Market => "market",
            PlanSource::Default => "default",
        },
    }
}

/// `commodityRow` (ts:637), for the single "after the trade" row.
fn commodity_row<'a>(commodity: &Commodity<'a>) -> Row<'a> {
    let mut flags = String::with_capacity(4);
    for (enabled, symbol) in [
        (commodity.consumer, 'C'),
        (commodity.producer, 'P'),
        (commodity.rare, 'R'),
        (commodity.illegal, 'I'),
    ] {
        flags.push(if enabled { symbol } else { '.' });
    }
    Row::Data(vec![
        // `String(id)`, so no grouping separators.
        Cow::Owned(js_number(commodity.id)),
        Cow::Borrowed(commodity.name),
        Cow::Owned(format_quantity(commodity.stock)),
        Cow::Owned(views::bracket_meter(commodity.stock_bracket)),
        Cow::Owned(format_quantity(commodity.buy_price)),
        Cow::Owned(format_quantity(commodity.sell_price)),
        Cow::Owned(format_quantity(commodity.fence_price)),
        Cow::Owned(format_quantity(commodity.demand)),
        Cow::Owned(views::bracket_meter(commodity.demand_bracket)),
        Cow::Owned(format_quantity(commodity.mean_price)),
        Cow::Owned(flags),
    ])
}

/// `TradePlan` (ts:1707) as JSON.
fn plan_json(plan: &TradePlan) -> JsValue {
    object([
        ("marketId".to_owned(), str_value(&plan.market_id)),
        ("transactionType".to_owned(), str_value(plan.kind.as_str())),
        ("commodityId".to_owned(), JsValue::Num(plan.commodity_id)),
        ("commodityName".to_owned(), str_value(&plan.commodity_name)),
        ("blackMarket".to_owned(), JsValue::Bool(plan.black_market)),
        ("stolen".to_owned(), JsValue::Bool(plan.stolen)),
        ("unitPrice".to_owned(), JsValue::Num(plan.unit_price)),
        ("qty".to_owned(), JsValue::Num(plan.qty)),
        ("finalQty".to_owned(), JsValue::Num(plan.final_qty)),
    ])
}

/// `{...settings, items}` (ts:2277).
///
/// The spread keeps each key in the settings literal's own position, so `items`
/// stays third rather than moving to the end \[R6\], and an `undefined` value
/// omits its key entirely.
fn batch_plan_json(settings: &config::BatchConfig, targets: &[batch::Target]) -> JsValue {
    let mut entries = vec![
        ("marketId".to_owned(), str_value(&settings.market_id)),
        ("transactionType".to_owned(), str_value(settings.kind.as_str())),
        (
            "items".to_owned(),
            JsValue::Arr(
                targets
                    .iter()
                    .map(|target| {
                        object([
                            ("id".to_owned(), JsValue::Num(target.id)),
                            ("name".to_owned(), str_value(&target.name)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("fill".to_owned(), JsValue::Bool(settings.fill)),
    ];
    if let Some(cargo) = settings.cargo {
        entries.push(("cargo".to_owned(), JsValue::Num(cargo)));
    }
    if let Some(qty) = settings.per_item_qty {
        entries.push(("perItemQty".to_owned(), JsValue::Num(qty)));
    }
    entries.push(("stolen".to_owned(), JsValue::Bool(settings.stolen)));
    if let Some(black_market) = settings.explicit_black_market {
        entries.push(("explicitBlackMarket".to_owned(), JsValue::Bool(black_market)));
    }
    if let Some(price) = settings.explicit_price {
        entries.push(("explicitPrice".to_owned(), JsValue::Num(price)));
    }
    entries.push(("watch".to_owned(), JsValue::Bool(settings.watch)));
    entries.push(("intervalMs".to_owned(), JsValue::Num(settings.interval_ms)));
    entries.push(("attemptLimit".to_owned(), JsValue::Num(settings.attempt_limit)));
    if let Some(credits) = settings.credits {
        entries.push(("credits".to_owned(), JsValue::Num(credits)));
    }
    object(entries)
}

/// `TradeRecord` (ts:2007) as JSON.
fn record_json(record: &batch::TradeRecord) -> JsValue {
    object([
        ("round".to_owned(), JsValue::Num(f64::from(record.round))),
        ("commodity".to_owned(), str_value(&record.commodity)),
        ("commodityId".to_owned(), JsValue::Num(record.commodity_id)),
        ("qty".to_owned(), JsValue::Num(record.qty)),
        ("unitPrice".to_owned(), JsValue::Num(record.unit_price)),
        ("status".to_owned(), num_or_null(record.status.map(f64::from))),
        ("cargoUsed".to_owned(), num_or_null(record.cargo_used)),
        ("credits".to_owned(), num_or_null(record.credits)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::Secret;

    fn credentials() -> game_api::Credentials {
        game_api::Credentials {
            commander_id: "F1234567".to_owned(),
            machine_id: "machine-1".to_owned(),
            machine_token: Secret::new("m".repeat(80)),
            auth_token: Secret::new("a".repeat(2024)),
        }
    }

    /// The two flags go on the wire as bare `1`/`0` and the market id keeps its
    /// leading zeros \[R53\].
    #[test]
    fn the_trade_envelope_sends_flags_as_numbers_and_the_id_verbatim() {
        let plan = TradePlan {
            market_id: "0004306502403".to_owned(),
            kind: edm_core::domain::trade::Kind::Buy,
            commodity_id: 128_049_204.0,
            commodity_name: "Gold".to_owned(),
            black_market: false,
            stolen: true,
            unit_price: 9000.0,
            qty: 10.0,
            final_qty: 10.0,
        };
        let plaintext = game_api::serialize_envelope(&trade_fields(&plan, &credentials(), 1.0));
        assert!(plaintext.starts_with(
            "cmdrId=F1234567&marketId=0004306502403&transactionType=buy&commodityId=128049204&blackMarket=0&stolen=1&unitPrice=9000&qty=10&finalQty=10&"
        ));
    }
}
