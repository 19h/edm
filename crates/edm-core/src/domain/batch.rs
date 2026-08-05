//! `runBatchTrade` (`market-request.ts:2091`) — buying or selling several
//! commodities, filling a hold, and retrying until it is full.
//!
//! The original is one `while` loop carrying five mutable flags (`outcome`,
//! `tradesThisRound`, `abandonRound`, `consecutiveFailures`, `skipped`) across
//! two `await`s, and it is the only code in the program that spends money. Here
//! it is a state machine whose every decision is a value: a run is a
//! `Vec<Step>`, so the exact sequence of network calls, sleeps and printed
//! lines is reviewable and snapshot-testable without a socket, a clock or a
//! commander's credentials.
//!
//! The driver keeps only the ambient parts — the listing, the entropy stamp,
//! the network and the sleep — and is a `match` over [`Step`]. Everything that
//! decides *what happens next* is here, and the round-end order in particular
//! is exactly the TypeScript's [R90].

use crate::js::json::JsValue;
use crate::js::{self, format_integer};
use crate::render::Row;

use super::trade::{self, Kind, Space, TradePlan};
use super::{MarketSnapshot, cargo_used, find_commodity, held_quantity};

/// `cargoUsed` (ts:1759), with the sign of zero the TypeScript produces.
///
/// The shared [`cargo_used`] folds with `Iterator::sum`, and Rust's `f64`
/// identity for that fold is **`-0.0`**; the TypeScript accumulates from a
/// literal `0`. They agree on every non-empty hold and disagree on an empty
/// one — where `formatInteger(-0)` is `"-0"` [R7], so every progress line of a
/// run that starts with an empty ship would read `cargo -0/200`. Adding `+0.0`
/// is exactly "start the accumulator at `+0`" for every input, including NaN
/// and the infinities.
///
/// This is a defect in the shared helper, not a quirk of the batch loop; it is
/// worked around here rather than fixed there only because that module belongs
/// to another change.
fn hold_used(inventory: &[JsValue]) -> f64 {
    cargo_used(inventory) + 0.0
}

/// `BatchSettings` (ts:1996), plus the two session flags the loop reads.
///
/// Already validated: `loadBatchSettings` (ts:2058) rejects the impossible
/// combinations while reading the command line, and its errors belong with the
/// argument accessors rather than with the loop.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchConfig {
    /// A string, never a number — `trade` passes `--market-id` through
    /// verbatim [R53].
    pub market_id: String,
    pub kind: Kind,
    pub items: Vec<String>,
    pub fill: bool,
    pub cargo: Option<f64>,
    /// Per-commodity ceiling; `None` only when `--fill` decides the amount.
    pub per_item_qty: Option<f64>,
    pub stolen: bool,
    pub explicit_black_market: Option<bool>,
    pub explicit_price: Option<f64>,
    pub watch: bool,
    pub interval_ms: f64,
    pub attempt_limit: f64,
    /// Starting balance, if `--credits` gave one; otherwise it is learned from
    /// the first reply that carries the key at all [R18].
    pub credits: Option<f64>,
    /// `session.dryRun`.
    pub dry_run: bool,
    /// `session.json` — suppresses every streamed line, not the trades.
    pub json: bool,
}

/// A commodity the run will visit, resolved once against the first listing.
///
/// Names are resolved before the loop and never again: the commodity set of a
/// market is fixed, only its prices and stock move (ts:2098). The id is what
/// each later round looks up, which is why a renamed commodity keeps trading
/// and a delisted one is skipped.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub id: f64,
    pub name: String,
}

/// One executed (or, under `--dry-run`, simulated) trade. `TradeRecord`
/// (ts:2007).
#[derive(Clone, Debug, PartialEq)]
pub struct TradeRecord {
    pub round: u32,
    pub commodity: String,
    pub commodity_id: f64,
    pub qty: f64,
    pub unit_price: f64,
    /// `None` renders as `-`: either nothing was sent, or nothing came back.
    pub status: Option<u16>,
    /// `None` when the reply did not carry a listing, so the hold is unknown.
    pub cargo_used: Option<f64>,
    pub credits: Option<f64>,
}

/// What the driver reports back after a [`Step::Trade`].
///
/// Three cases, because the TypeScript distinguishes three: it tests
/// `exchange.decrypted` for the failure counter (ts:2235) but `result` — the
/// *parsed* listing — for whether the hold and the balance moved (ts:2208). A
/// reply that decrypts into something that is not a market listing is therefore
/// a successful trade about which nothing was learned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The response parsed as a market listing. The driver must already have
    /// adopted it as its `latest` snapshot before the next call (ts:2209).
    Listing { status: u16 },
    /// Decrypted, but not a market listing.
    Opaque { status: u16 },
    /// `!exchange || exchange.decrypted === null` — the only case that counts
    /// toward the three-in-a-row limit.
    Failed { status: Option<u16> },
}

impl Reply {
    const fn status(self) -> Option<u16> {
        match self {
            Self::Listing { status } | Self::Opaque { status } => Some(status),
            Self::Failed { status } => status,
        }
    }

    const fn is_failure(self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    const fn is_listing(self) -> bool {
        matches!(self, Self::Listing { .. })
    }
}

/// Why the run stopped, and after how many rounds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// `outcome` (ts:2120) — the phrase that ends the TRADES title.
    pub reason: String,
    pub rounds: u32,
}

/// Everything the final report needs that the loop accumulated.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BatchReport {
    pub trades: Vec<TradeRecord>,
    /// The last balance seen, or the one `--credits` supplied.
    pub credits: Option<f64>,
    pub rounds: u32,
    pub outcome: String,
}

impl BatchReport {
    /// `totalUnits` (ts:2288).
    #[must_use]
    pub fn total_units(&self) -> f64 {
        self.trades.iter().map(|record| record.qty).sum()
    }

    /// `totalValue` (ts:2289).
    #[must_use]
    pub fn total_value(&self) -> f64 {
        self.trades.iter().map(|record| record.qty * record.unit_price).sum()
    }
}

/// One thing the driver should do next.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Re-read the market listing and pass the result as `latest`.
    ///
    /// `requireMarketSnapshot` (ts:1932) **throws** on failure, and the throw
    /// escapes `runBatchTrade`: a mid-watch refresh that fails prints no TRADES
    /// table and no JSON, and exits 1. The driver must propagate the error
    /// rather than ending the run tidily [R90].
    Refresh,
    /// Send this trade.
    ///
    /// The driver draws the entropy stamp and prepares the request *before*
    /// looking at `--dry-run` (ts:2183-2191), so the stream advances by exactly
    /// one stamp per planned trade whether or not anything is sent. Emitting
    /// `Trade` on both paths is what forces that [R90].
    Trade(TradePlan),
    /// A commodity that cannot be traded this round.
    ///
    /// Not a printed line: the reasons accumulate and only the first three
    /// reach the waiting line at the end of an idle round (ts:2269). Surfaced
    /// as a step because it is the interesting half of what a watch round does.
    Skip { reason: String },
    /// A streamed line, to be clamped to the terminal width and printed
    /// (`emitProgressLine`, ts:2043) [R33]. Never produced under `--json`.
    Progress(String),
    /// Wait before the next round. Emitted at the end of every *continuing*
    /// round and never after the last one [R90].
    Sleep { millis: f64 },
    /// The loop is over; `report()` holds the rest. Calling `next` again
    /// repeats this step.
    Done(Outcome),
}

/// Where the loop is. Each variant is a point the TypeScript can be suspended
/// at — the two `await`s, plus the positions between two emitted lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// `round++`, and the refresh that every round after the first begins with.
    RoundStart,
    /// The hold is measured and the round-opening fill check runs (ts:2125).
    RoundBegin,
    Items { index: usize },
    /// A `Trade` is outstanding; the next call carries its `Reply`.
    AwaitReply { index: usize },
    /// The reply is recorded and its line emitted; the failure bookkeeping
    /// (ts:2235) still has to run.
    AfterTrade { index: usize, failed: bool },
    /// ts:2248.
    RetryNote,
    /// The four round-end tests, in the TypeScript's order (ts:2249-2264).
    Checks,
    /// ts:2266.
    WaitNote,
    Sleeping,
    Done,
}

/// The batch loop.
#[derive(Clone, Debug)]
pub struct Batch {
    config: BatchConfig,
    targets: Vec<Target>,
    state: State,
    round: u32,
    /// Reset to zero by any trade that comes back (ts:2244), so it counts a
    /// *run* of failures rather than a total.
    consecutive_failures: u32,
    used: f64,
    free: Space,
    trades_this_round: u32,
    abandon_round: bool,
    skipped: Vec<String>,
    /// The plan handed out by the outstanding [`Step::Trade`].
    pending: Option<TradePlan>,
    /// `cargoUsed` of the listing the run opened with, for the BATCH PLAN
    /// table (ts:2108).
    opening_used: f64,
    report: BatchReport,
}

impl Batch {
    /// Resolves the item list against the opening listing.
    ///
    /// Everything that can fail before a single request is sent fails here:
    /// an unknown or ambiguous name (`findCommodity`, ts:1737) and a repeated
    /// one (ts:2100).
    pub fn new(config: BatchConfig, first: &MarketSnapshot<'_>) -> Result<Self, String> {
        let mut targets = Vec::with_capacity(config.items.len());
        for token in &config.items {
            let found = find_commodity(&first.commodities, token)?;
            targets.push(Target { id: found.id, name: found.name.to_owned() });
        }

        // ts:2100 — `findIndex(...) !== index` names the *second* occurrence,
        // and it compares resolved ids, so two spellings of one commodity
        // collide. `position` returning `None` for an unmatchable (NaN) id
        // reproduces `findIndex`'s `-1`, which is likewise never equal to the
        // index and likewise reports a duplicate.
        let duplicate = targets
            .iter()
            .enumerate()
            .find(|(index, target)| targets.iter().position(|o| o.id == target.id) != Some(*index));
        if let Some((_, target)) = duplicate {
            return Err(format!("--item lists {} more than once", target.name));
        }

        let credits = config.credits;
        Ok(Self {
            config,
            targets,
            state: State::RoundStart,
            round: 0,
            consecutive_failures: 0,
            used: 0.0,
            free: Space::UNBOUNDED,
            trades_this_round: 0,
            abandon_round: false,
            skipped: Vec::new(),
            pending: None,
            opening_used: hold_used(first.inventory),
            report: BatchReport { credits, ..BatchReport::default() },
        })
    }

    /// Advances one step.
    ///
    /// `latest` is the driver's current listing — the opening one, the one a
    /// [`Step::Refresh`] produced, or the one a `Reply::Listing` carried.
    /// `reply` is the result of the previous [`Step::Trade`] and is ignored
    /// under `--dry-run`, where nothing was sent; a missing reply on the live
    /// path reads as `Reply::Failed`, which is what an absent `exchange` means.
    pub fn next(&mut self, latest: &MarketSnapshot<'_>, reply: Option<Reply>) -> Step {
        let mut reply = reply;
        loop {
            let step = match self.state {
                State::RoundStart => self.start_round(),
                State::RoundBegin => self.begin_round(latest),
                State::Items { index } => self.item(latest, index),
                State::AwaitReply { index } => self.settle(latest, index, reply.take()),
                State::AfterTrade { index, failed } => self.after_trade(index, failed),
                State::RetryNote => self.retry_note(),
                State::Checks => self.checks(),
                State::WaitNote => self.wait_note(),
                State::Sleeping => {
                    self.state = State::RoundStart;
                    Some(Step::Sleep { millis: self.config.interval_ms })
                }
                State::Done => Some(Step::Done(self.outcome())),
            };
            if let Some(step) = step {
                return step;
            }
        }
    }

    #[must_use]
    pub const fn report(&self) -> &BatchReport {
        &self.report
    }

    /// The resolved item list, in the order the rounds visit it.
    #[must_use]
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    // -- the loop ------------------------------------------------------------

    /// ts:2122.
    fn start_round(&mut self) -> Option<Step> {
        self.round += 1;
        self.report.rounds = self.round;
        self.state = State::RoundBegin;
        (self.round > 1).then_some(Step::Refresh)
    }

    /// ts:2125.
    fn begin_round(&mut self, latest: &MarketSnapshot<'_>) -> Option<Step> {
        self.used = hold_used(latest.inventory);
        self.free = Space::of(self.config.cargo, self.used);
        // ts:2127 — a run that starts with a full hold does one listing read
        // and stops.
        if self.config.fill && self.free.exhausted() {
            return Some(self.finish("hold is full".to_owned()));
        }

        self.trades_this_round = 0;
        self.abandon_round = false;
        self.skipped.clear();
        self.state = State::Items { index: 0 };
        None
    }

    /// One turn of the `for (const target of targets)` loop (ts:2136).
    fn item(&mut self, latest: &MarketSnapshot<'_>, index: usize) -> Option<Step> {
        // ts:2137 — the list ran out, or the hold filled mid-round and the rest
        // of it waits for the round-end check to end the run.
        if index >= self.targets.len() || (self.config.fill && self.free.exhausted()) {
            self.state = State::RetryNote;
            return None;
        }
        self.state = State::Items { index: index + 1 };

        let target = &self.targets[index];
        let Some(current) = latest.by_id(target.id) else {
            // ts:2141 — resolved once, so a commodity the market has since
            // stopped listing is simply absent.
            return Some(self.skip(format!("{}: no longer listed", target.name)));
        };
        let current = *current;

        let black_market =
            trade::derive_black_market(Some(&current), self.config.stolen, self.config.explicit_black_market);
        let unit_price = match self.config.explicit_price {
            Some(price) => price,
            // ts:2150 — a commodity this market does not sell names itself and
            // the round carries on with the others.
            None => match trade::derive_price(&current, self.config.kind, black_market) {
                Ok(price) => price,
                Err(message) => return Some(self.skip(message)),
            },
        };

        let held = held_quantity(latest.inventory, &current, self.config.stolen);
        let available = trade::available(&current, held, self.config.kind);
        let qty = trade::plan_quantity(
            self.config.kind,
            self.config.fill,
            self.config.per_item_qty,
            available,
            self.free,
            self.report.credits,
            unit_price,
        );

        if qty == 0.0 {
            // ts:2165 — `available === 0` wins over an unaffordable price,
            // which wins over a full hold [R91].
            let reason =
                trade::zero_quantity_reason(self.config.kind, available, self.report.credits, unit_price);
            return Some(self.skip(format!("{}: {reason}", current.name)));
        }

        let plan = TradePlan {
            market_id: self.config.market_id.clone(),
            kind: self.config.kind,
            commodity_id: current.id,
            commodity_name: current.name.to_owned(),
            black_market,
            stolen: self.config.stolen,
            unit_price,
            qty,
            final_qty: trade::resulting_stack(held, qty, self.config.kind),
        };
        self.pending = Some(plan.clone());
        self.state = State::AwaitReply { index };
        Some(Step::Trade(plan))
    }

    /// The reply to the outstanding trade (ts:2191 dry, ts:2206 live).
    fn settle(&mut self, latest: &MarketSnapshot<'_>, index: usize, reply: Option<Reply>) -> Option<Step> {
        let plan = self.pending.take().expect("a Trade step is always answered before the next one");

        if self.config.dry_run {
            return self.simulate(&plan, index);
        }

        // ts:2206 — no reply at all is `exchange === undefined`.
        let reply = reply.unwrap_or(Reply::Failed { status: None });
        if reply.is_listing() {
            // ts:2210 — the balance is learned from the first reply that
            // carries the key. A present-but-null `credits` reads as zero and
            // clamps every later buy to nothing [R18].
            self.report.credits = latest.credits().or(self.report.credits);
            self.used = hold_used(latest.inventory);
            self.free = Space::of(self.config.cargo, self.used);
        }

        self.report.trades.push(TradeRecord {
            round: self.round,
            commodity: plan.commodity_name.clone(),
            commodity_id: plan.commodity_id,
            qty: plan.qty,
            unit_price: plan.unit_price,
            status: reply.status(),
            cargo_used: reply.is_listing().then_some(self.used),
            credits: self.report.credits,
        });
        self.trades_this_round += 1;
        self.state = State::AfterTrade { index, failed: reply.is_failure() };

        if self.config.json {
            return None;
        }
        // ts:2229.
        let status = reply.status().map_or_else(|| "?".to_owned(), |status| status.to_string());
        let credits = match self.report.credits {
            None => String::new(),
            Some(credits) => format!("  credits {}", format_integer(credits)),
        };
        Some(Step::Progress(format!(
            "[{}] {} {} x {} @ {} = {} cr  HTTP {status}  cargo {}{credits}",
            self.round,
            self.config.kind.as_str(),
            format_integer(plan.qty),
            plan.commodity_name,
            format_integer(plan.unit_price),
            format_integer(plan.qty * plan.unit_price),
            format_cargo(self.used, self.config.cargo),
        )))
    }

    /// ts:2191 — `--dry-run` simulates locally, so a multi-item fill still
    /// previews the whole sequence instead of stopping at the first item.
    fn simulate(&mut self, plan: &TradePlan, index: usize) -> Option<Step> {
        self.used += match self.config.kind {
            Kind::Buy => plan.qty,
            Kind::Sell => -plan.qty,
        };
        self.free = Space::of(self.config.cargo, self.used);
        self.report.trades.push(TradeRecord {
            round: self.round,
            commodity: plan.commodity_name.clone(),
            commodity_id: plan.commodity_id,
            qty: plan.qty,
            unit_price: plan.unit_price,
            status: None,
            cargo_used: Some(self.used),
            credits: self.report.credits,
        });
        self.trades_this_round += 1;
        self.state = State::Items { index: index + 1 };

        if self.config.json {
            return None;
        }
        // ts:2198.
        Some(Step::Progress(format!(
            "[{}] would {} {} x {} @ {} = {} cr  cargo {}",
            self.round,
            self.config.kind.as_str(),
            format_integer(plan.qty),
            plan.commodity_name,
            format_integer(plan.unit_price),
            format_integer(plan.qty * plan.unit_price),
            format_cargo(self.used, self.config.cargo),
        )))
    }

    /// ts:2235 — what a failed trade does to the run.
    fn after_trade(&mut self, index: usize, failed: bool) -> Option<Step> {
        if !failed {
            self.consecutive_failures = 0;
            self.state = State::Items { index: index + 1 };
            return None;
        }

        // Stock or the balance may have moved under us; a watcher re-reads and
        // tries again, and only a third consecutive failure ends the run.
        self.consecutive_failures += 1;
        self.abandon_round = true;
        if !self.config.watch || self.consecutive_failures >= 3 {
            let repeated = if self.consecutive_failures > 1 {
                format!(" {} times in a row", self.consecutive_failures)
            } else {
                String::new()
            };
            return Some(self.finish(format!("a trade request failed{repeated}")));
        }
        self.state = State::RetryNote;
        None
    }

    /// ts:2248.
    fn retry_note(&mut self) -> Option<Step> {
        self.state = State::Checks;
        (self.abandon_round && !self.config.json)
            .then(|| Step::Progress(format!("[{}] retrying after a failed request", self.round)))
    }

    /// The round-end ladder (ts:2249-2264), in the TypeScript's order.
    ///
    /// `hold is full` is tested **before** `--dry-run: nothing was sent`, so a
    /// dry-run fill that fills the hold reports the former [R90].
    fn checks(&mut self) -> Option<Step> {
        if self.config.fill && self.free.exhausted() {
            return Some(self.finish("hold is full".to_owned()));
        }
        if self.config.dry_run {
            return Some(self.finish("--dry-run: nothing was sent".to_owned()));
        }
        if !self.config.watch {
            return Some(self.finish("single pass complete".to_owned()));
        }
        // ts:2261 — `attemptLimit` is a raw `Number`, so a NaN limit is falsy
        // at `> 0` and the watch runs until something else stops it, exactly as
        // it does there.
        if self.config.attempt_limit > 0.0 && f64::from(self.round) >= self.config.attempt_limit {
            return Some(self.finish(format!("stopped after {} rounds", self.round)));
        }
        self.state = State::WaitNote;
        None
    }

    /// ts:2266 — the only line an idle watch round prints, and the only place
    /// the skip reasons surface.
    fn wait_note(&mut self) -> Option<Step> {
        self.state = State::Sleeping;
        if self.trades_this_round != 0 || self.abandon_round || self.config.json {
            return None;
        }
        let reasons = if self.skipped.is_empty() {
            String::new()
        } else {
            format!("  ({})", self.skipped.iter().take(3).cloned().collect::<Vec<_>>().join("; "))
        };
        // `${intervalMs / 1_000}s` is `Number::toString`, so a 1500 ms interval
        // is `1.5s` and a 100 ms one is `0.1s` — never `1.5000` [R92].
        Some(Step::Progress(format!(
            "[{}] waiting {}s \u{2014} cargo {}{reasons}",
            self.round,
            js::js_number(self.config.interval_ms / 1000.0),
            format_cargo(self.used, self.config.cargo),
        )))
    }

    fn skip(&mut self, reason: String) -> Step {
        self.skipped.push(reason.clone());
        Step::Skip { reason }
    }

    fn finish(&mut self, reason: String) -> Step {
        self.report.outcome = reason;
        self.report.rounds = self.round;
        self.state = State::Done;
        Step::Done(self.outcome())
    }

    fn outcome(&self) -> Outcome {
        Outcome { reason: self.report.outcome.clone(), rounds: self.report.rounds }
    }

    // -- the two tables ------------------------------------------------------

    /// The rows of `BATCH PLAN` (ts:2104), which is printed before the first
    /// round and only when `--json` is off.
    #[must_use]
    pub fn plan_rows(&self) -> Vec<Row<'static>> {
        let mut rows = vec![
            field("market", self.config.market_id.clone()),
            field(
                "action",
                if self.config.fill {
                    "buy until the hold is full".to_owned()
                } else {
                    format!("{} up to --qty each", self.config.kind.as_str())
                },
            ),
            field(
                "order",
                self.targets.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(" -> "),
            ),
        ];
        if let Some(cargo) = self.config.cargo {
            rows.push(field("cargo", format!("{} units", format_cargo(self.opening_used, Some(cargo)))));
        }
        if let Some(qty) = self.config.per_item_qty {
            rows.push(field("per item max", format_integer(qty)));
        }
        rows.push(field(
            "retry",
            if self.config.watch {
                // ts:2113 — a zero (or NaN) attempt limit is falsy, and then
                // only a full hold stops the run.
                let limit = if js::truthy(self.config.attempt_limit) {
                    format!(", up to {} rounds", js::js_number(self.config.attempt_limit))
                } else {
                    " until filled".to_owned()
                };
                format!("every {}s{limit}", js::js_number(self.config.interval_ms / 1000.0))
            } else {
                "single pass".to_owned()
            },
        ));
        rows
    }

    /// The title of the `TRADES` table (ts:2290).
    #[must_use]
    pub fn trades_title(&self) -> String {
        format!(
            "TRADES  {} requests over {} round{} \u{2014} {}",
            self.report.trades.len(),
            self.report.rounds,
            if self.report.rounds == 1 { "" } else { "s" },
            self.report.outcome,
        )
    }

    /// The rows of the `TRADES` table (ts:2291), one per request plus the
    /// total.
    ///
    /// `final_used` is `cargoUsed` of the listing the run ended with, which is
    /// the driver's to compute because a failed trade leaves the run holding an
    /// older snapshot than the last record does.
    #[must_use]
    pub fn trades_rows(&self, final_used: f64) -> Vec<Row<'static>> {
        let mut rows: Vec<Row<'static>> = self
            .report
            .trades
            .iter()
            .map(|record| {
                Row::data([
                    record.round.to_string(),
                    record.commodity.clone(),
                    format_integer(record.qty),
                    format_integer(record.unit_price),
                    format_integer(record.qty * record.unit_price),
                    record.status.map_or_else(|| "-".to_owned(), |status| status.to_string()),
                    record
                        .cargo_used
                        .map_or_else(|| "-".to_owned(), |used| format_cargo(used, self.config.cargo)),
                ])
            })
            .collect();
        rows.push(Row::Rule);
        rows.push(Row::data([
            String::new(),
            "TOTAL".to_owned(),
            format_integer(self.report.total_units()),
            String::new(),
            format_integer(self.report.total_value()),
            String::new(),
            format_cargo(final_used, self.config.cargo),
        ]));
        rows
    }

    /// The note under the table (ts:2317) — absent when no reply ever carried a
    /// balance.
    #[must_use]
    pub fn credits_note(&self) -> Option<String> {
        self.report.credits.map(|credits| format!("credits now {}", format_integer(credits)))
    }
}

/// `fieldRow` (ts:506).
fn field(name: &'static str, value: String) -> Row<'static> {
    Row::data([name.to_owned(), value])
}

/// `formatCargo` (ts:2036) — `12` without a capacity, `12/100` with one.
#[must_use]
pub fn format_cargo(used: f64, cargo: Option<f64>) -> String {
    cargo.map_or_else(
        || format_integer(used),
        |capacity| format!("{}/{}", format_integer(used), format_integer(capacity)),
    )
}
