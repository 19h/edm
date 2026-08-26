//! The market sweep: N workers pulling from one shared queue.
//!
//! The original spawns N promises that each `shift()` a shared array and, when
//! it is momentarily empty, `await sleep(25)` and try again — because a peer
//! may still requeue something, so an empty queue does not mean the work is
//! done. That polling loop is the one thing here worth replacing outright: it
//! adds up to 25 ms of latency to every requeue and burns a 40 Hz timer per
//! idle worker.
//!
//! The replacement is an MPMC channel that workers *park* on. The subtlety is
//! knowing when to close it: every worker holds a sender in order to requeue,
//! so the usual "drop all senders and let `recv` end" idiom never fires. The
//! invariant that makes an explicit close sound is spelled out at the counter.

use std::cell::Cell;
use std::time::Duration;

use edm_core::consts::MARKET_LIST;
use edm_core::domain::eddn::{EddnOptions, EddnStation, build_message};
use edm_core::domain::starsystem::MarketPoint;
use edm_core::domain::{MarketSnapshot, parse_market_snapshot};
use edm_core::js;
use edm_core::js::json::JsValue;

use crate::eddn::EddnResult;
use crate::game_api::{self, Credentials, HeaderConfig, Stamp};
use crate::net::HttpTransport;
use crate::out::Out;
use crate::ports::{Clock, Entropy};

/// How a sweep is paced.
#[derive(Clone, Copy, Debug)]
pub struct SweepSettings {
    pub workers: usize,
    pub timeout: Duration,
    /// Total attempts per market is this plus one.
    pub requeues: f64,
    pub quiet: bool,
    pub detail: bool,
}

/// One market's worth of results.
#[derive(Debug)]
pub struct MarketVisit {
    pub market_id: f64,
    pub name: String,
    pub status: Option<u16>,
    /// The parsed listing, kept whole so a snapshot can borrow from it at
    /// render time.
    ///
    /// `Some` exactly when the payload was a usable market listing, so
    /// `snapshot().is_some()` and `document.is_some()` never disagree — which
    /// matters because the retry decision turns on it.
    document: Option<JsValue>,
    pub eddn: Option<EddnResult>,
    pub attempts: u32,
    /// Populated by the original and then never rendered. R89.
    pub failure: Option<String>,
}

impl MarketVisit {
    #[must_use]
    pub fn snapshot(&self) -> Option<MarketSnapshot<'_>> {
        self.document.as_ref().and_then(parse_market_snapshot)
    }

    #[must_use]
    pub fn has_data(&self) -> bool {
        self.document.is_some()
    }
}

/// Everything a visit needs that a sweep cannot compute for itself.
///
/// `Debug` is hand-written because the transport, clock and entropy are generic
/// and none of them need to be printable for this to be useful in a panic.
pub struct Cx<'a, H, C, E> {
    pub http: &'a H,
    /// `EDM_ORIGIN_OVERRIDE`, or the game-internal API's own origin.
    pub origin: &'a str,
    pub clock: &'a C,
    pub entropy: &'a E,
    pub out: &'a Out,
    pub credentials: &'a Credentials,
    pub headers: &'a HeaderConfig,
    pub method_override: Option<&'a str>,
    pub dry_run: bool,
    /// Fixed overrides for the per-request stamp, when the flags pinned them.
    pub nonce_override: Option<edm_core::wire::Nonce>,
    pub frontier_time_override: Option<f64>,
    pub request_time_override: Option<u32>,
    /// Set when `--eddn` or `--eddn-test` was given.
    pub eddn: Option<&'a EddnPublish<'a>>,
    /// `--detail`: renders one market's full snapshot.
    ///
    /// Called from *inside* the worker, immediately after that market's
    /// progress line, because the original prints it there (ts:1546) — so the
    /// snapshots interleave with the progress lines in completion order rather
    /// than arriving in a block afterwards.
    pub detail: Option<&'a dyn Fn(&MarketVisit)>,
}

/// The EDDN side of a sweep.
#[derive(Debug)]
pub struct EddnPublish<'a> {
    pub options: &'a EddnOptions,
    pub url: &'a str,
    pub system_name: &'a str,
}

impl<H, C, E> std::fmt::Debug for Cx<'_, H, C, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cx")
            .field("dry_run", &self.dry_run)
            .field("method_override", &self.method_override)
            .field("eddn", &self.eddn.is_some())
            .finish_non_exhaustive()
    }
}

/// `nextStamp` (ts:100) — regenerated per request unless a flag pinned it.
pub fn next_stamp<C: Clock, E: Entropy>(
    clock: &C,
    entropy: &E,
    nonce: Option<edm_core::wire::Nonce>,
    frontier_time: Option<f64>,
    request_time: Option<u32>,
) -> Stamp {
    Stamp {
        nonce: nonce.unwrap_or_else(|| edm_core::wire::Nonce::from_entropy(entropy.nonce_bytes())),
        frontier_time: frontier_time.unwrap_or_else(|| clock.frontier_time()),
        // `>>> 0` wraps rather than saturating, and the uptime is whole seconds
        // so this is always a multiple of 1000. R15.
        request_time: request_time
            .unwrap_or_else(|| js::to_uint32((clock.uptime_seconds() * 1000.0).floor())),
    }
}

/// `isTransientStatus` (ts:1456).
///
/// A 4xx means the request itself is wrong, and retrying it three times just
/// repeats the mistake. Timeouts, rate limits and 5xx are the cases worth
/// requeueing — and so is *no* status at all, which is what a transport failure
/// or a timeout leaves behind.
#[must_use]
pub const fn is_transient_status(status: Option<u16>) -> bool {
    match status {
        None => true,
        Some(code) => code == 408 || code == 429 || code >= 500,
    }
}

/// The failure text for an attempt that ran out of time (ts:1442).
///
/// **R86 is oracle-pinned and not yet measured.** The original races the work
/// against a timer that both aborts the request and rejects with this message,
/// and which of the two rejections wins the race depends on whether Bun's
/// `fetch` rejects synchronously inside `abort()`. If the harness records
/// `aborted (timeout)` instead, this is the single line that changes.
#[must_use]
pub fn timeout_failure(millis: f64) -> String {
    format!("timed out after {} ms", js::format_integer(millis))
}

/// `[requeue n/total] Name (id): reason` (ts:1525).
#[must_use]
pub fn requeue_line(attempts: u32, requeues: f64, name: &str, id: f64, reason: &str) -> String {
    format!(
        "[requeue {}/{}] {name} ({}): {reason}",
        js::js_number(f64::from(attempts)),
        js::js_number(requeues),
        js::js_number(id),
    )
}

/// `[k/N] Name (id)  HTTP s  outcome` (ts:1540).
///
/// The counters interpolate through `String(n)` and so are **ungrouped**, while
/// the commodity count goes through `formatInteger` and so carries commas. That
/// asymmetry is in the original.
#[derive(Clone, Copy, Debug)]
pub struct Progress<'a> {
    pub completed: usize,
    pub total: usize,
    pub name: &'a str,
    pub market_id: f64,
    pub status: Option<u16>,
    pub outcome: &'a str,
    pub attempts: u32,
    pub eddn: Option<&'a EddnResult>,
}

#[must_use]
pub fn done_line(p: &Progress<'_>) -> String {
    use std::fmt::Write as _;
    let mut line = format!(
        "[{}/{}] {} ({})  HTTP {}  {}",
        js::js_number(p.completed as f64),
        js::js_number(p.total as f64),
        p.name,
        js::js_number(p.market_id),
        p.status
            .map_or_else(|| "-".to_owned(), |s| js::js_number(f64::from(s))),
        p.outcome,
    );
    if p.attempts > 1 {
        let _ = write!(
            line,
            "  after {} attempts",
            js::js_number(f64::from(p.attempts))
        );
    }
    if let Some(result) = p.eddn {
        line.push_str("  eddn ");
        line.push_str(if result.ok { "sent" } else { &result.detail });
    }
    line
}

/// Polls one market and optionally forwards it to EDDN (ts:1390).
pub async fn visit_market<H: HttpTransport, C: Clock, E: Entropy>(
    cx: &Cx<'_, H, C, E>,
    market_id: f64,
    name: &str,
    station: Option<&EddnStation>,
) -> MarketVisit {
    let stamp = next_stamp(
        cx.clock,
        cx.entropy,
        cx.nonce_override,
        cx.frontier_time_override,
        cx.request_time_override,
    );
    let request = game_api::prepare(
        cx.origin,
        MARKET_LIST,
        cx.method_override,
        game_api::list_fields(
            &js::js_number(market_id),
            cx.credentials,
            stamp.frontier_time,
        ),
        stamp,
        cx.headers,
    );

    // A sweep polls quietly, so the request table is suppressed — the
    // per-market tables would drown the progress lines. The *response* table is
    // not suppressed unconditionally: `send` prints it from a second site when
    // a quiet poll fails, because the headers are where the diagnosis is
    // \[R74\]. Passing a no-op here would silently drop the RESPONSE block for
    // every 405 and 500 in a sweep.
    let exchange = crate::exchange::send(
        cx.http,
        cx.out,
        &request,
        cx.dry_run,
        crate::exchange::SendOptions {
            quiet: true,
            ignore_dry_run: false,
            quiet_failure: false,
        },
        |_| {},
        |exchange| crate::cmd::emit_response(cx.out, exchange),
    )
    .await;

    let (status, document) = match exchange {
        Some(ref exchange) => (
            Some(exchange.status),
            exchange
                .decrypted
                .as_deref()
                .and_then(|text| JsValue::parse(text).ok())
                .filter(|doc| parse_market_snapshot(doc).is_some()),
        ),
        None => (None, None),
    };

    let mut eddn = None;
    if let (Some(publish), Some(station), Some(doc)) = (cx.eddn, station, document.as_ref()) {
        // The timestamp is the moment of publication, not of the poll.
        let timestamp = edm_core::js::time::iso8601_from_ms(cx.clock.now_ms()).unwrap_or_default();
        if let Some(snapshot) = parse_market_snapshot(doc) {
            let message = build_message(
                station,
                market_id,
                &snapshot.commodities,
                &timestamp,
                publish.options,
            );
            eddn = Some(if cx.dry_run {
                EddnResult {
                    ok: true,
                    status: None,
                    detail: format!(
                        "dry-run: {} commodities ready",
                        js::js_number(message.count as f64)
                    ),
                    commodities: message.count,
                }
            } else {
                let body = message.payload.stringify_compact();
                crate::eddn::submit(cx.http, publish.url, body.as_bytes(), message.count).await
            });
        }
    }

    MarketVisit {
        market_id,
        name: name.to_owned(),
        status,
        document,
        eddn,
        attempts: 1,
        failure: None,
    }
}

#[derive(Clone, Copy, Debug)]
struct Job {
    index: u32,
    attempts: u32,
}

// One function, because the invariant that makes the channel close soundly
// spans the whole loop and splitting it would hide the thing most worth reading.
#[allow(
    clippy::too_many_lines,
    reason = "the retirement invariant spans the loop"
)]
/// Sweeps every target with a pool of workers.
///
/// Results come back in the order the markets were *listed*, not the order they
/// happened to finish, and they are keyed by market id rather than by index —
/// so a starsystem payload that names the same market twice produces two rows
/// both holding the last writer's result, exactly as the original's `Map` does.
/// R88.
pub async fn sweep<H: HttpTransport, C: Clock, E: Entropy>(
    cx: &Cx<'_, H, C, E>,
    targets: &[MarketPoint<'_>],
    settings: &SweepSettings,
) -> Vec<MarketVisit> {
    if targets.is_empty() {
        return Vec::new();
    }

    let (tx, rx) = async_channel::unbounded::<Job>();
    for index in 0..targets.len() {
        let _ = tx.try_send(Job {
            index: index as u32,
            attempts: 0,
        });
    }

    // INVARIANT: `outstanding` == queued + held-by-a-worker. It is decremented
    // exactly once per target, at the single retirement site below. A worker
    // may only send a job it is already holding, which moves one unit from
    // held to queued without changing the total — so `outstanding == 0` implies
    // no further send is possible, which is precisely when closing the channel
    // is sound. On a current-thread runtime this cell only mutates between
    // await points, so decrement-and-test is atomic by construction.
    let outstanding = Cell::new(targets.len());
    let completed = Cell::new(0usize);
    let max_attempts = settings.requeues + 1.0;

    let workers = (0..settings.workers).map(|_| {
        let (tx, rx) = (&tx, &rx);
        let outstanding = &outstanding;
        let completed = &completed;
        async move {
            let mut mine: Vec<(u32, MarketVisit)> = Vec::new();
            // Parked, not polling. When the last job retires the channel is
            // closed and every worker wakes once to find it empty.
            while let Ok(mut job) = rx.recv().await {
                job.attempts += 1;
                let target = &targets[job.index as usize];

                let station = cx.eddn.map(|publish| EddnStation {
                    system_name: publish.system_name.to_owned(),
                    station_name: target.name.to_string(),
                    station_type: target.is_carrier().then(|| "FleetCarrier".to_owned()),
                    economies: None,
                });

                let attempt = tokio::time::timeout(
                    settings.timeout,
                    visit_market(cx, target.market_id, &target.name, station.as_ref()),
                )
                .await;

                let (mut visit, failure) = match attempt {
                    Ok(visit) => {
                        // Under `--dry-run` nothing was sent, so there is no
                        // failure to report and nothing is ever requeued — every
                        // row ends up "no data". R87.
                        let failure = (!visit.has_data() && !cx.dry_run).then(|| {
                            visit.status.map_or_else(
                                || "no response".to_owned(),
                                |s| format!("HTTP {}", js::js_number(f64::from(s))),
                            )
                        });
                        (visit, failure)
                    }
                    Err(_) => (
                        MarketVisit {
                            market_id: target.market_id,
                            name: target.name.to_string(),
                            status: None,
                            document: None,
                            eddn: None,
                            attempts: job.attempts,
                            failure: None,
                        },
                        Some(timeout_failure(settings.timeout.as_millis() as f64)),
                    ),
                };

                // A 2xx that failed to decrypt carries a status of 200, which is
                // not transient — retrying it would just fail the same way. R84.
                let retry =
                    failure.is_some() && !visit.has_data() && is_transient_status(visit.status);
                if retry && f64::from(job.attempts) < max_attempts {
                    // To the BACK of the queue, keeping its attempt count, so
                    // one bad market cannot be retried head-first while others
                    // wait.
                    let _ = tx.try_send(job);
                    if !settings.quiet {
                        cx.out.progress(&requeue_line(
                            job.attempts,
                            settings.requeues,
                            &target.name,
                            target.market_id,
                            failure.as_deref().unwrap_or(""),
                        ));
                    }
                    continue;
                }

                let left = outstanding.get() - 1;
                outstanding.set(left);
                if left == 0 {
                    rx.close();
                }
                completed.set(completed.get() + 1);

                visit.attempts = job.attempts;
                visit.failure.clone_from(&failure);

                if !settings.quiet {
                    let outcome = visit.snapshot().map_or_else(
                        || failure.clone().unwrap_or_else(|| "no data".to_owned()),
                        |snapshot| {
                            format!(
                                "{} commodities",
                                js::format_integer(snapshot.commodities.len() as f64)
                            )
                        },
                    );
                    cx.out.progress(&done_line(&Progress {
                        completed: completed.get(),
                        total: targets.len(),
                        name: &target.name,
                        market_id: target.market_id,
                        status: visit.status,
                        outcome: &outcome,
                        attempts: job.attempts,
                        eddn: visit.eddn.as_ref(),
                    }));
                }

                if let Some(render) = cx.detail
                    && settings.detail
                    && !settings.quiet
                {
                    render(&visit);
                }

                mine.push((job.index, visit));
            }
            mine
        }
    });

    // `join_all` polls its children in order on the first poll and the channel
    // is pre-filled, so worker *i* takes `targets[i]` — which is what
    // `Array.from({length: N}, () => worker())` does, and what fixes the
    // progress-line order for a fast mock server. R83.
    let mut finished: Vec<(f64, MarketVisit)> = Vec::new();
    for batch in futures_util::future::join_all(workers).await {
        for (_, visit) in batch {
            // Last writer wins for a duplicated id, matching the original's Map.
            if let Some(slot) = finished.iter_mut().find(|(id, _)| *id == visit.market_id) {
                slot.1 = visit;
            } else {
                finished.push((visit.market_id, visit));
            }
        }
    }

    // Reported in listing order. A duplicate id yields two rows that both hold
    // the surviving result.
    let mut ordered = Vec::with_capacity(targets.len());
    for target in targets {
        if let Some((_, visit)) = finished.iter().find(|(id, _)| *id == target.market_id) {
            ordered.push(MarketVisit {
                market_id: visit.market_id,
                name: visit.name.clone(),
                status: visit.status,
                document: visit.document.clone(),
                eddn: visit.eddn.clone(),
                attempts: visit.attempts,
                failure: visit.failure.clone(),
            });
        }
    }
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_transient_statuses_are_retried() {
        // No status at all — a timeout or a dropped connection — is retryable.
        assert!(is_transient_status(None));
        for code in [408, 429, 500, 503, 599] {
            assert!(
                is_transient_status(Some(code)),
                "{code} should be transient"
            );
        }
        // A 4xx means the request was wrong; repeating it repeats the mistake.
        for code in [200, 400, 401, 403, 404, 405, 418] {
            assert!(
                !is_transient_status(Some(code)),
                "{code} should not be transient"
            );
        }
    }

    /// The counters are ungrouped and the commodity count is not — the original
    /// interpolates one through `String(n)` and the other through
    /// `formatInteger`.
    #[test]
    fn progress_lines_mix_grouped_and_ungrouped_numbers() {
        let line = done_line(&Progress {
            completed: 1000,
            total: 2000,
            name: "Jaques Station",
            market_id: 4_306_502_403.0,
            status: Some(200),
            outcome: &format!("{} commodities", js::format_integer(1234.0)),
            attempts: 1,
            eddn: None,
        });
        assert_eq!(
            line,
            "[1000/2000] Jaques Station (4306502403)  HTTP 200  1,234 commodities"
        );
    }

    #[test]
    fn the_attempt_suffix_appears_only_after_a_retry() {
        let one = Progress {
            completed: 1,
            total: 1,
            name: "X",
            market_id: 1.0,
            status: Some(200),
            outcome: "ok",
            attempts: 1,
            eddn: None,
        };
        assert!(!done_line(&one).contains("attempts"));
        let retried = done_line(&Progress { attempts: 3, ..one });
        assert!(retried.ends_with("  after 3 attempts"));
    }

    /// The denominator is the configured requeue budget, so the last line reads
    /// `3/3` and the fourth attempt retires the job. R85.
    #[test]
    fn requeue_lines_count_against_the_budget() {
        assert_eq!(
            requeue_line(3, 3.0, "Ohm City", 128_667_761.0, "HTTP 500"),
            "[requeue 3/3] Ohm City (128667761): HTTP 500"
        );
    }
}
