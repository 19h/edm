//! The batch trade loop, held to `game-internal-api.ts:2091-2319`.
//!
//! Every test here drives the state machine with a scripted driver: the market
//! listings are fixtures, the replies are a script, and the run is the `Vec` of
//! steps that comes out. That is the whole point of modelling the loop as a
//! state machine — the decision sequence of a run that would otherwise spend
//! real credits is an ordinary value, and the properties that matter (never buy
//! more than the balance covers, never overfill the hold, always terminate) are
//! assertions over it rather than hopes about a `while` loop.

use edm_core::domain::batch::{Batch, BatchConfig, Reply, Step};
use edm_core::domain::trade::Kind;
use edm_core::domain::{MarketSnapshot, held_quantity, parse_market_snapshot};
use edm_core::js::js_number;
use edm_core::js::json::JsValue;
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Market fixtures
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Credits {
    /// The key is absent, so the balance stays unknown.
    #[default]
    Absent,
    /// Present and null — reads as zero and clamps every buy to nothing [R18].
    Null,
    Value(f64),
}

#[derive(Clone, Debug)]
struct Item {
    id: f64,
    name: String,
    stock: f64,
    buy: f64,
    sell: f64,
}

impl Item {
    fn new(id: f64, name: &str, stock: f64, price: f64) -> Self {
        Self {
            id,
            name: name.to_owned(),
            stock,
            buy: price,
            sell: price,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Market {
    credits: Credits,
    items: Vec<Item>,
    hold: Vec<(String, f64)>,
}

impl Market {
    fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            ..Self::default()
        }
    }

    fn credits(mut self, credits: Credits) -> Self {
        self.credits = credits;
        self
    }

    fn holding(mut self, name: &str, qty: f64) -> Self {
        self.hold.push((name.to_owned(), qty));
        self
    }

    /// The shape `parseMarketSnapshot` (ts:758) expects: `commodities` is an
    /// *object*, and its keys are what a commodity falls back to for a name.
    fn document(&self) -> JsValue {
        let commodities = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                format!(
                    "\"k{index}\":{{\"id\":{},\"name\":\"{}\",\"categoryname\":\"Metals\",\
                     \"stock\":{},\"buyPrice\":{},\"sellPrice\":{},\"fencePrice\":{},\"legality\":\"\"}}",
                    js_number(item.id),
                    item.name,
                    js_number(item.stock),
                    js_number(item.buy),
                    js_number(item.sell),
                    js_number(item.sell),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let inventory = self
            .hold
            .iter()
            .map(|(name, qty)| {
                format!(
                    "{{\"commodity\":\"{name}\",\"qty\":{},\"stolen\":false}}",
                    js_number(*qty)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let credits = match self.credits {
            Credits::Absent => String::new(),
            Credits::Null => "\"credits\":null,".to_owned(),
            Credits::Value(value) => format!("\"credits\":{},", js_number(value)),
        };
        let source =
            format!("{{{credits}\"commodities\":{{{commodities}}},\"inventory\":[{inventory}]}}");
        JsValue::parse(&source).expect("fixture is JSON")
    }
}

fn config(kind: Kind, items: &[&str]) -> BatchConfig {
    BatchConfig {
        market_id: "3223343616".to_owned(),
        kind,
        items: items.iter().map(|item| (*item).to_owned()).collect(),
        fill: false,
        cargo: None,
        per_item_qty: Some(10.0),
        stolen: false,
        explicit_black_market: None,
        explicit_price: None,
        watch: false,
        interval_ms: 1000.0,
        attempt_limit: 0.0,
        credits: None,
        dry_run: false,
        json: false,
    }
}

// ---------------------------------------------------------------------------
// The scripted driver
// ---------------------------------------------------------------------------

/// What the driver hands back for one [`Step::Trade`]. `Listing` also names the
/// document the driver adopts as its `latest`, which is what the TypeScript's
/// `latest = result` (ts:2209) does.
#[derive(Clone, Copy, Debug)]
enum Answer {
    Listing(usize, u16),
    Opaque(u16),
    Failed(Option<u16>),
}

#[derive(Clone, Debug, Default)]
struct Script {
    /// The document each successive `Refresh` yields; the last one repeats.
    refreshes: Vec<usize>,
    /// The reply to each successive `Trade`; the last one repeats.
    replies: Vec<Answer>,
}

impl Script {
    fn replying(replies: Vec<Answer>) -> Self {
        Self {
            refreshes: Vec::new(),
            replies,
        }
    }
}

/// One trade as it was handed out, with the context the clamps used.
#[derive(Clone, Debug)]
struct Sent {
    plan: edm_core::domain::trade::TradePlan,
    /// The listing in force when the plan was built.
    listing: usize,
    /// `credits` as the loop knew it at that moment.
    credits: Option<f64>,
}

struct Run {
    trace: Vec<Step>,
    sent: Vec<Sent>,
    batch: Batch,
}

/// Drives a `Batch` to completion against a script.
///
/// The panic on `budget` is the termination property: every test asserts it by
/// existing.
fn drive(config: BatchConfig, docs: &[JsValue], script: &Script, budget: usize) -> Run {
    let snapshots: Vec<MarketSnapshot<'_>> = docs
        .iter()
        .map(|doc| parse_market_snapshot(doc).expect("a market listing"))
        .collect();
    let dry_run = config.dry_run;
    let mut latest = 0usize;
    let mut batch = Batch::new(config, &snapshots[0]).expect("the item list resolves");

    let mut trace = Vec::new();
    let mut sent = Vec::new();
    let mut reply = None;
    let (mut refreshes, mut replies) = (0usize, 0usize);
    loop {
        let step = batch.next(&snapshots[latest], reply.take());
        trace.push(step.clone());
        assert!(
            trace.len() <= budget,
            "the loop did not terminate: {trace:#?}"
        );
        match step {
            Step::Refresh => {
                latest = last_or(&script.refreshes, refreshes).unwrap_or(latest);
                refreshes += 1;
            }
            Step::Trade(plan) => {
                sent.push(Sent {
                    plan,
                    listing: latest,
                    credits: batch.report().credits,
                });
                let answer = last_or(&script.replies, replies).unwrap_or(Answer::Failed(None));
                replies += 1;
                if !dry_run {
                    reply = Some(match answer {
                        Answer::Listing(doc, status) => {
                            latest = doc;
                            Reply::Listing { status }
                        }
                        Answer::Opaque(status) => Reply::Opaque { status },
                        Answer::Failed(status) => Reply::Failed { status },
                    });
                }
            }
            Step::Done(_) => break,
            Step::Skip { .. } | Step::Progress(_) | Step::Sleep { .. } => {}
        }
    }
    Run { trace, sent, batch }
}

/// A script shorter than the run repeats its last entry, so a fixture only has
/// to spell out what changes.
fn last_or<T: Copy>(script: &[T], index: usize) -> Option<T> {
    script.get(index).or_else(|| script.last()).copied()
}

// ---------------------------------------------------------------------------
// Snapshots of whole runs
// ---------------------------------------------------------------------------

#[test]
fn single_pass_visits_every_item_once() {
    let market = Market::new(vec![
        Item::new(101.0, "Gold", 50.0, 100.0),
        Item::new(102.0, "Silver", 40.0, 30.0),
    ])
    .credits(Credits::Value(1_000_000.0));
    let docs = [market.document()];
    let run = drive(
        config(Kind::Buy, &["gold", "silver"]),
        &docs,
        &Script::replying(vec![Answer::Listing(0, 200)]),
        40,
    );
    insta::assert_debug_snapshot!("single_pass", run.trace);
}

/// A fill stops the moment the hold is full, mid-list if need be.
#[test]
fn a_fill_stops_when_the_hold_is_full() {
    let empty = Market::new(vec![
        Item::new(101.0, "Gold", 5.0, 100.0),
        Item::new(102.0, "Silver", 400.0, 30.0),
    ])
    .credits(Credits::Value(1_000_000.0));
    let after_gold = empty.clone().holding("Gold", 5.0);
    let full = empty.clone().holding("Gold", 5.0).holding("Silver", 15.0);
    let docs = [empty.document(), after_gold.document(), full.document()];

    let mut config = config(Kind::Buy, &["gold", "silver"]);
    config.fill = true;
    config.cargo = Some(20.0);
    config.per_item_qty = None;

    let script = Script::replying(vec![Answer::Listing(1, 200), Answer::Listing(2, 200)]);
    let run = drive(config, &docs, &script, 40);
    insta::assert_debug_snapshot!("fill", run.trace);
}

/// A watch with `--attempts` sleeps between rounds and not after the last one
/// [R90], and an idle round is the only one that prints why it did nothing.
#[test]
fn a_watch_stops_at_its_attempt_limit() {
    let market =
        Market::new(vec![Item::new(101.0, "Gold", 0.0, 100.0)]).credits(Credits::Value(500.0));
    let docs = [market.document()];

    let mut config = config(Kind::Buy, &["gold"]);
    config.watch = true;
    config.attempt_limit = 2.0;
    config.interval_ms = 1500.0;

    let run = drive(config, &docs, &Script::default(), 40);
    insta::assert_debug_snapshot!("watch_with_attempts", run.trace);
}

/// Three *consecutive* failures end a watch; the first two only cost a round
/// [R90].
#[test]
fn three_consecutive_failures_end_the_run() {
    let market = Market::new(vec![Item::new(101.0, "Gold", 50.0, 100.0)])
        .credits(Credits::Value(1_000_000.0));
    let docs = [market.document()];

    let mut config = config(Kind::Buy, &["gold"]);
    config.fill = true;
    config.cargo = Some(200.0);
    config.per_item_qty = None;
    config.watch = true;

    let run = drive(
        config,
        &docs,
        &Script::replying(vec![Answer::Failed(Some(502))]),
        40,
    );
    insta::assert_debug_snapshot!("three_failures", run.trace);
}

/// `--dry-run` previews the whole sequence, because `used` and `free` move
/// locally (ts:2192).
#[test]
fn a_dry_run_fill_previews_the_whole_sequence() {
    let market = Market::new(vec![
        Item::new(101.0, "Gold", 5.0, 100.0),
        Item::new(102.0, "Silver", 400.0, 30.0),
    ])
    .credits(Credits::Value(1_000_000.0));
    let docs = [market.document()];

    let mut config = config(Kind::Buy, &["gold", "silver"]);
    config.fill = true;
    config.cargo = Some(20.0);
    config.per_item_qty = None;
    config.dry_run = true;

    let run = drive(config, &docs, &Script::default(), 40);
    insta::assert_debug_snapshot!("dry_run_fill", run.trace);
}

// ---------------------------------------------------------------------------
// The details that decide correctness
// ---------------------------------------------------------------------------

fn outcome(run: &Run) -> String {
    run.batch.report().outcome.clone()
}

/// R90: `hold is full` is tested before `--dry-run: nothing was sent`, so a
/// dry-run fill that fills the hold reports the former.
#[test]
fn a_filled_dry_run_reports_the_hold_before_the_dry_run() {
    let market =
        Market::new(vec![Item::new(101.0, "Gold", 500.0, 100.0)]).credits(Credits::Value(1e9));
    let docs = [market.document()];

    let mut filled = config(Kind::Buy, &["gold"]);
    filled.fill = true;
    filled.cargo = Some(20.0);
    filled.per_item_qty = None;
    filled.dry_run = true;
    assert_eq!(
        outcome(&drive(filled.clone(), &docs, &Script::default(), 40)),
        "hold is full"
    );

    // The same run with room left over falls through to the dry-run branch.
    let mut roomy = filled;
    roomy.cargo = Some(1000.0);
    assert_eq!(
        outcome(&drive(roomy, &docs, &Script::default(), 40)),
        "--dry-run: nothing was sent"
    );
}

/// R90: the phrase counting failures appears only above one.
#[test]
fn a_single_failure_is_not_counted_out_loud() {
    let market =
        Market::new(vec![Item::new(101.0, "Gold", 50.0, 100.0)]).credits(Credits::Value(1e9));
    let docs = [market.document()];
    let run = drive(
        config(Kind::Buy, &["gold"]),
        &docs,
        &Script::replying(vec![Answer::Failed(None)]),
        40,
    );
    assert_eq!(outcome(&run), "a trade request failed");
}

/// A reply that decrypts into something that is not a market listing is *not* a
/// failed trade — the TypeScript tests `exchange.decrypted`, not the parsed
/// result (ts:2235).
#[test]
fn an_unparseable_reply_is_not_a_failure() {
    let market =
        Market::new(vec![Item::new(101.0, "Gold", 50.0, 100.0)]).credits(Credits::Value(1e9));
    let docs = [market.document()];
    let run = drive(
        config(Kind::Buy, &["gold"]),
        &docs,
        &Script::replying(vec![Answer::Opaque(200)]),
        40,
    );
    assert_eq!(outcome(&run), "single pass complete");
    assert_eq!(
        run.batch.report().trades[0].cargo_used,
        None,
        "nothing was learned about the hold"
    );
    assert_eq!(run.batch.report().trades[0].status, Some(200));
}

/// Names resolve once, before the loop; a commodity the market has since
/// stopped listing is skipped by id (ts:2141).
#[test]
fn a_delisted_commodity_is_skipped_by_name() {
    let before = Market::new(vec![
        Item::new(101.0, "Gold", 50.0, 100.0),
        Item::new(102.0, "Silver", 40.0, 30.0),
    ])
    .credits(Credits::Value(1e9));
    let after =
        Market::new(vec![Item::new(102.0, "Silver", 40.0, 30.0)]).credits(Credits::Value(1e9));
    let docs = [before.document(), after.document()];

    let mut config = config(Kind::Buy, &["gold", "silver"]);
    config.watch = true;
    config.attempt_limit = 2.0;

    let script = Script {
        refreshes: vec![1],
        replies: vec![Answer::Listing(0, 200)],
    };
    let run = drive(config, &docs, &script, 60);
    let skipped: Vec<&str> = run
        .trace
        .iter()
        .filter_map(|step| match step {
            Step::Skip { reason } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(skipped, ["Gold: no longer listed"]);
}

#[test]
fn duplicate_items_are_rejected_before_anything_is_sent() {
    let market = Market::new(vec![Item::new(101.0, "Gold", 50.0, 100.0)]);
    let doc = market.document();
    let snapshot = parse_market_snapshot(&doc).unwrap();
    assert_eq!(
        Batch::new(config(Kind::Buy, &["gold", "gol"]), &snapshot).unwrap_err(),
        "--item lists Gold more than once"
    );
}

/// R18: a present-but-null `credits` reads as zero, and zero credits buys
/// nothing. The reason chain names the balance, not the hold [R91].
#[test]
fn a_null_credits_field_clamps_every_later_buy_to_nothing() {
    let rich = Market::new(vec![
        Item::new(101.0, "Gold", 50.0, 100.0),
        Item::new(102.0, "Silver", 40.0, 30.0),
    ]);
    let broke = rich.clone().credits(Credits::Null);
    let docs = [rich.document(), broke.document()];

    let run = drive(
        config(Kind::Buy, &["gold", "silver"]),
        &docs,
        &Script::replying(vec![Answer::Listing(1, 200)]),
        40,
    );
    assert_eq!(run.sent.len(), 1, "the second item cannot be afforded");
    assert_eq!(run.batch.report().credits, Some(0.0));
    let skipped: Vec<&str> = run
        .trace
        .iter()
        .filter_map(|step| match step {
            Step::Skip { reason } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(skipped, ["Silver: not enough credits"]);
}

/// R91: `available === 0` wins over an unaffordable price.
#[test]
fn an_empty_market_reports_no_stock_even_when_broke() {
    let market =
        Market::new(vec![Item::new(101.0, "Gold", 0.0, 100.0)]).credits(Credits::Value(0.0));
    let docs = [market.document()];
    let mut config = config(Kind::Buy, &["gold"]);
    config.credits = Some(0.0);
    let run = drive(config, &docs, &Script::default(), 40);
    assert!(matches!(&run.trace[0], Step::Skip { reason } if reason == "Gold: no stock"));
}

/// R92: `${intervalMs / 1000}s` is `Number::toString`, so it renders `1`, `1.5`
/// and `0.1` — never a fixed-point `1.0`.
#[test]
fn the_interval_renders_as_javascript_would_print_it() {
    for (millis, expected) in [
        (1000.0, "1s"),
        (1500.0, "1.5s"),
        (100.0, "0.1s"),
        (3_600_000.0, "3600s"),
    ] {
        let market = Market::new(vec![Item::new(101.0, "Gold", 0.0, 100.0)]);
        let docs = [market.document()];
        let mut config = config(Kind::Buy, &["gold"]);
        config.watch = true;
        config.attempt_limit = 2.0;
        config.interval_ms = millis;

        let run = drive(config, &docs, &Script::default(), 40);
        let waiting = run
            .trace
            .iter()
            .find_map(|step| match step {
                Step::Progress(line) if line.contains("waiting") => Some(line.clone()),
                _ => None,
            })
            .expect("an idle round says what it is waiting for");
        assert_eq!(
            waiting,
            format!("[1] waiting {expected} \u{2014} cargo 0  (Gold: no stock)")
        );
    }
}

/// R90: a sleep ends every continuing round and none follows the last.
#[test]
fn sleeps_separate_rounds_and_never_end_the_run() {
    let market = Market::new(vec![Item::new(101.0, "Gold", 0.0, 100.0)]);
    let docs = [market.document()];
    let mut config = config(Kind::Buy, &["gold"]);
    config.watch = true;
    config.attempt_limit = 3.0;

    let run = drive(config, &docs, &Script::default(), 40);
    let sleeps = run
        .trace
        .iter()
        .filter(|step| matches!(step, Step::Sleep { .. }))
        .count();
    let refreshes = run
        .trace
        .iter()
        .filter(|step| matches!(step, Step::Refresh))
        .count();
    assert_eq!(sleeps, 2, "three rounds, two gaps");
    assert_eq!(refreshes, 2, "the opening listing is the caller's");
    assert!(!matches!(
        run.trace[run.trace.len() - 2],
        Step::Sleep { .. }
    ));
}

/// R90: the stamp is drawn before the dry-run branch, so the entropy stream
/// advances by one stamp per *planned* trade either way. The driver draws it on
/// `Step::Trade`, so the two runs must hand out the same trades.
#[test]
fn a_dry_run_plans_exactly_what_a_live_run_would_send() {
    let market = Market::new(vec![
        Item::new(101.0, "Gold", 5.0, 100.0),
        Item::new(102.0, "Silver", 40.0, 30.0),
    ])
    .credits(Credits::Value(1e9));
    let docs = [market.document()];

    let live = drive(
        config(Kind::Buy, &["gold", "silver"]),
        &docs,
        &Script::replying(vec![Answer::Opaque(200)]),
        40,
    );
    let mut dry = config(Kind::Buy, &["gold", "silver"]);
    dry.dry_run = true;
    let dry = drive(dry, &docs, &Script::default(), 40);

    let plans = |run: &Run| {
        run.sent
            .iter()
            .map(|sent| sent.plan.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(plans(&live), plans(&dry));
}

/// The `--json` stream carries no progress lines at all (ts:2196, ts:2227,
/// ts:2248, ts:2266), but the same trades still happen.
#[test]
fn json_runs_emit_no_progress_lines() {
    let market =
        Market::new(vec![Item::new(101.0, "Gold", 5.0, 100.0)]).credits(Credits::Value(1e9));
    let docs = [market.document()];
    let mut config = config(Kind::Buy, &["gold"]);
    config.json = true;
    let run = drive(
        config,
        &docs,
        &Script::replying(vec![Answer::Listing(0, 200)]),
        40,
    );
    assert!(
        !run.trace
            .iter()
            .any(|step| matches!(step, Step::Progress(_)))
    );
    assert_eq!(run.batch.report().trades.len(), 1);
}

/// The tables the run ends with (ts:2290-2317).
#[test]
fn the_trades_table_totals_what_was_sent() {
    let market = Market::new(vec![
        Item::new(101.0, "Gold", 5.0, 100.0),
        Item::new(102.0, "Silver", 40.0, 30.0),
    ])
    .credits(Credits::Value(1_000_000.0));
    let docs = [market.document()];
    let run = drive(
        config(Kind::Buy, &["gold", "silver"]),
        &docs,
        &Script::replying(vec![Answer::Listing(0, 200)]),
        40,
    );

    assert_eq!(
        run.batch.trades_title(),
        "TRADES  2 requests over 1 round \u{2014} single pass complete"
    );
    let rows = run.batch.trades_rows(15.0);
    insta::assert_debug_snapshot!("trades_rows", rows);
    assert_eq!(run.batch.credits_note().unwrap(), "credits now 1,000,000");
    insta::assert_debug_snapshot!("plan_rows", run.batch.plan_rows());
}

// ---------------------------------------------------------------------------
// Properties over whole simulated runs
// ---------------------------------------------------------------------------

fn any_market() -> impl Strategy<Value = Market> {
    let item = (1u32..4, 0u32..60, 1u32..200).prop_map(|(id, stock, price)| {
        Item::new(
            f64::from(id) * 100.0,
            ["Gold", "Silver", "Water"][id as usize - 1],
            f64::from(stock),
            f64::from(price),
        )
    });
    let credits = prop_oneof![
        Just(Credits::Absent),
        Just(Credits::Null),
        (0u32..100_000).prop_map(|value| Credits::Value(f64::from(value))),
    ];
    let hold = proptest::collection::vec((0usize..3, 0u32..40), 0..3).prop_map(|entries| {
        entries
            .into_iter()
            .map(|(index, qty)| {
                (
                    ["Gold", "Silver", "Water"][index].to_owned(),
                    f64::from(qty),
                )
            })
            .collect::<Vec<_>>()
    });
    (proptest::collection::vec(item, 1..4), credits, hold).prop_map(|(items, credits, hold)| {
        // Distinct ids, because the loop's own duplicate check is about the
        // *item list*, not about a market that lists one commodity twice.
        let mut seen = Vec::new();
        let items: Vec<Item> = items
            .into_iter()
            .filter(|item| {
                let fresh = !seen.contains(&item.name);
                seen.push(item.name.clone());
                fresh
            })
            .collect();
        Market {
            credits,
            items,
            hold,
        }
    })
}

/// The command line as a handful of primitives, so that a generated run can
/// pair any market with any settings.
#[derive(Clone, Copy, Debug)]
struct Seed {
    buy: bool,
    fill: bool,
    cargo: Option<u32>,
    qty: Option<u32>,
    watch: bool,
    attempts: u32,
    dry_run: bool,
    credits: Option<u32>,
}

fn any_seed() -> impl Strategy<Value = Seed> {
    (
        prop::bool::ANY,
        prop::bool::ANY,
        prop::option::of(0u32..40),
        prop::option::of(1u32..30),
        prop::bool::ANY,
        1u32..4,
        prop::bool::ANY,
        prop::option::of(0u32..50_000),
    )
        .prop_map(
            |(buy, fill, cargo, qty, watch, attempts, dry_run, credits)| Seed {
                buy,
                fill,
                cargo,
                qty,
                watch,
                attempts,
                dry_run,
                credits,
            },
        )
}

/// The settings `loadBatchSettings` (ts:2058) would have produced. Its
/// validation is applied here rather than generated around: `--fill` is
/// buy-only and needs a capacity (ts:2073), and a watch without a stopping
/// condition is rejected outright (ts:2079), so an unbounded run is not a case
/// the loop can be asked to handle.
fn settings(seed: Seed, market: &Market) -> BatchConfig {
    BatchConfig {
        market_id: "3223343616".to_owned(),
        kind: if seed.buy { Kind::Buy } else { Kind::Sell },
        items: market
            .items
            .iter()
            .map(|item| item.name.to_lowercase())
            .collect(),
        fill: seed.fill && seed.buy && seed.cargo.is_some(),
        cargo: seed.cargo.map(f64::from),
        per_item_qty: Some(seed.qty.map_or(10.0, f64::from)),
        stolen: false,
        explicit_black_market: None,
        explicit_price: None,
        watch: seed.watch,
        interval_ms: 1000.0,
        attempt_limit: f64::from(seed.attempts),
        credits: seed.credits.map(f64::from),
        dry_run: seed.dry_run,
        json: false,
    }
}

fn any_script() -> impl Strategy<Value = Script> {
    let answer = prop_oneof![
        Just(Answer::Listing(0, 200)),
        Just(Answer::Opaque(200)),
        Just(Answer::Failed(Some(500))),
        Just(Answer::Failed(None)),
    ];
    proptest::collection::vec(answer, 1..6).prop_map(Script::replying)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// The whole contract of the loop, asserted over a run rather than a step:
    /// it terminates, every trade it hands out is a whole number of units it
    /// can afford and has room for, and nothing non-finite ever reaches the
    /// wire.
    #[test]
    fn a_run_never_plans_something_it_cannot_pay_for(
        market in any_market(),
        script in any_script(),
        seed in any_seed(),
    ) {
        let config = settings(seed, &market);
        let docs = [market.document()];
        // Two hundred steps is far above the ceiling of four rounds over three
        // items; overrunning it means the machine stopped making progress.
        let run = drive(config.clone(), &docs, &script, 200);
        let snapshots: Vec<MarketSnapshot<'_>> =
            docs.iter().map(|doc| parse_market_snapshot(doc).unwrap()).collect();

        for sent in &run.sent {
            let plan = &sent.plan;
            prop_assert!(plan.qty >= 1.0, "a planned trade always moves at least one unit");
            prop_assert_eq!(plan.qty, plan.qty.floor(), "quantities are whole units");
            prop_assert!(plan.qty.is_finite() && plan.unit_price.is_finite());
            prop_assert!(plan.final_qty.is_finite() && plan.commodity_id.is_finite());

            if config.kind == Kind::Buy
                && plan.unit_price > 0.0
                && let Some(credits) = sent.credits
            {
                prop_assert!(
                    plan.qty * plan.unit_price <= credits,
                    "planned {} x {} against {} credits",
                    plan.qty,
                    plan.unit_price,
                    credits
                );
            }

            // `finalQty` is the resulting stack, not a copy of `qty` (ts:2181).
            let listing = &snapshots[sent.listing];
            let commodity = listing.by_id(plan.commodity_id).expect("the plan names a listed commodity");
            let held = held_quantity(listing.inventory, commodity, config.stolen);
            let expected = match config.kind {
                Kind::Buy => held + plan.qty,
                Kind::Sell => edm_core::js::js_max(0.0, held - plan.qty),
            };
            prop_assert_eq!(plan.final_qty, expected);
        }

        // Under `--dry-run` the hold is simulated, so the records are the only
        // evidence that the loop respects the capacity it was given.
        if config.dry_run
            && config.kind == Kind::Buy
            && let Some(cargo) = config.cargo
        {
            for record in &run.batch.report().trades {
                prop_assert!(record.cargo_used.unwrap_or(0.0) <= cargo);
            }
        }

        prop_assert!(!run.batch.report().outcome.is_empty(), "a finished run always says why");
        prop_assert!(matches!(run.trace.last(), Some(Step::Done(_))));
    }

    /// A `Trade` is always followed by exactly one settlement, and `Done` is
    /// final: calling `next` again repeats it rather than restarting the loop.
    #[test]
    fn done_is_absorbing(market in any_market(), script in any_script()) {
        let docs = [market.document()];
        let snapshots: Vec<MarketSnapshot<'_>> =
            docs.iter().map(|doc| parse_market_snapshot(doc).unwrap()).collect();
        let mut config = config(Kind::Buy, &["gold"]);
        config.items = market.items.iter().map(|item| item.name.to_lowercase()).collect();

        let mut run = drive(config, &docs, &script, 200);
        let last = run.trace.last().cloned();
        prop_assert!(matches!(last, Some(Step::Done(_))));
        let again = run.batch.next(&snapshots[0], None);
        prop_assert_eq!(last.unwrap(), again);
    }
}
