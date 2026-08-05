//! The paced worker pool that fills a region's market table.
//!
//! Two stages, and they are genuinely two rather than one queue of mixed work:
//!
//! 1. **Systems.** One `starsystem` read per candidate system, which is what
//!    tells us authoritatively which markets are actually there. Ardent
//!    proposed them; this confirms.
//! 2. **Markets.** One `market` read per surviving market id.
//!
//! Stage 2 is *not* a barrier behind stage 1. A system's markets are queued the
//! instant that system's read lands, so the pool is saturated throughout
//! instead of idling while the last few systems trickle in. On a 1,300-market
//! sweep that is the difference between one long tail and two.
//!
//! Everything here is paced through a shared [`Pacer`], so the concurrency
//! number decides only how much latency is hidden — never how fast requests
//! leave. Sixteen workers behind a four-per-second bucket still issue four per
//! second, which is the property that makes a wide sweep safe to run at all.

use std::cell::{Cell, RefCell};

use edm_core::pace::GiveUpReason;
use edm_core::render::views::PaceEvent;

use crate::out::Out;
use crate::ports::{Clock, Entropy, Timer};
use crate::route::pacer::Pacer;

/// One unit of work, at either stage.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// Read a system's `starsystem` payload.
    System { name: String, address: f64 },
    /// Read one market's listing.
    Market { market_id: f64, station: String, system: String },
}

impl Job {
    /// The name to print for this job.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::System { name, .. } => name,
            Self::Market { station, .. } => station,
        }
    }
}

/// What one attempt produced, as the pool needs to see it.
///
/// Deliberately narrow: the pool decides only whether to retry, whether to
/// queue follow-on work, and what to count. Everything else — parsing,
/// rendering, EDDN — belongs to the caller's closure.
#[derive(Debug, Default)]
pub struct Outcome {
    pub status: Option<u16>,
    /// Present exactly when the server named one.
    pub retry_after: Option<String>,
    /// Whether the attempt produced usable data. A 200 that decrypts to
    /// something unparseable is *not* a success.
    pub ok: bool,
    /// Markets this system turned out to hold. Queued immediately.
    pub follow_on: Vec<Job>,
    /// The server answered definitively that this market has no commodity
    /// market at all.
    ///
    /// **HTTP 410 with `"commodities not currently available at this market"`.**
    /// Measured against a live radius-100 sweep, 2026-08-05: 40 of 5,089
    /// markets. It is a correct, permanent answer to a well-formed request —
    /// the station is real, it was reached, and it has nothing to trade. A
    /// failure it is not, and counting it as one made the coverage block report
    /// forty markets "not reached and absent from the ranking, not ranked low"
    /// when every one of them had been reached and had answered.
    pub absent: bool,
    /// Rows worth trading, for the progress line.
    ///
    /// Not the commodity count: **every Companion API market returns the same
    /// 391-entry map**, most of it priced but idle, so that number is identical
    /// for every market in the galaxy and tells a reader nothing.
    pub tradable: Option<usize>,
}

/// What a whole run produced.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tally {
    pub systems_read: usize,
    pub systems_failed: usize,
    pub markets_polled: usize,
    pub markets_failed: usize,
    /// Answered, and definitively empty. See [`Outcome::absent`].
    pub markets_absent: usize,
    /// Never attempted, because the run's `--deadline` had already passed.
    pub markets_out_of_time: usize,
    /// Set when the breaker stopped the run before the queue drained.
    pub abandoned: usize,
}

/// One line per job as it lands: the job, what it produced, how many attempts
/// it took, and how many jobs are now done.
pub type Report<'a> = &'a dyn Fn(&Job, &Outcome, u32, usize);

/// One line per pacing decision.
pub type Trace<'a> = &'a dyn Fn(&PaceEvent<'_>);

/// Everything the pool needs that it cannot decide for itself.
pub struct Pool<'a, C, T, E> {
    pub pacer: &'a Pacer<'a, C, T, E>,
    pub out: &'a Out,
    pub workers: usize,
    pub quiet: bool,
    /// Say what is happening while it happens.
    ///
    /// Called from *inside* the worker, immediately after the attempt it
    /// describes, so the lines arrive in completion order as the sweep
    /// progresses. Collecting them and printing afterwards would turn a
    /// five-minute progress report into a five-minute silence followed by a
    /// wall of text — which is the same as no progress report at all.
    pub report: Option<Report<'a>>,
    /// `--verbose`: the pacing decisions behind those lines.
    pub trace: Option<Trace<'a>>,
}

impl<C, T, E> std::fmt::Debug for Pool<'_, C, T, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("workers", &self.workers)
            .field("quiet", &self.quiet)
            .field("verbose", &self.trace.is_some())
            .finish_non_exhaustive()
    }
}

/// A job that has been given up on, with the reason.
#[derive(Clone, Debug, PartialEq)]
pub struct Abandoned {
    pub job: Job,
    pub reason: GiveUpReason,
    pub attempts: u32,
    pub status: Option<u16>,
}

/// A job plus what has happened to it so far.
#[derive(Debug)]
struct Ticket {
    job: Job,
    attempts: u32,
    /// When the first attempt was actually issued. `NaN` until then, which is
    /// never read: the stamp is written before it is used, on attempt one.
    first_attempt_ms: f64,
}

/// Run the pool until the queue drains or the breaker trips.
///
/// One function, because the retirement invariant that makes closing the
/// channel sound spans the whole loop and splitting it would hide the thing
/// most worth reading.
///
/// `attempt` is called once per try. It receives the job and must not itself
/// pace or retry — both are this function's business, and doing either inside
/// the closure would put two independent limiters on the same wire.
///
/// The job arrives **by value**. A `Fn(&Job) -> Fut` cannot be written in
/// today's Rust without boxing: `Fut` would have to be generic over the borrow's
/// lifetime, which only a higher-ranked bound could say and only for a named
/// future type. Cloning two `String`s per attempt is nothing beside the HTTP
/// request the closure is about to make.
///
/// The return is the tally plus every job that was given up on. Those are the
/// markets the coverage table must name: absent from the ranking because they
/// were never read, not because they ranked low.
#[expect(clippy::too_many_lines, reason = "the retirement invariant spans the loop")]
pub async fn run<C, T, E, F, Fut>(
    pool: &Pool<'_, C, T, E>,
    seed: Vec<Job>,
    attempt: F,
) -> (Tally, Vec<Abandoned>)
where
    C: Clock,
    T: Timer,
    E: Entropy,
    F: Fn(Job) -> Fut,
    Fut: Future<Output = Outcome>,
{
    if seed.is_empty() {
        return (Tally::default(), Vec::new());
    }

    let queued = seed.len();
    let (tx, rx) = async_channel::unbounded::<Ticket>();
    for job in seed {
        let _ = tx.try_send(Ticket { job, attempts: 0, first_attempt_ms: f64::NAN });
    }

    // INVARIANT, as in `sweep`: `outstanding` counts queued plus held-by-a-
    // worker. It rises when follow-on work is queued and falls exactly once per
    // job retirement. A worker only ever sends while holding — a requeue moves
    // one unit from held to queued, a follow-on adds one before the holder
    // retires — so reaching zero proves no further send is possible, which is
    // when closing the channel is sound. Two-stage work makes this counter do
    // real work rather than merely mirroring a fixed length.
    let outstanding = Cell::new(queued);
    let completed = Cell::new(0usize);
    let tally = RefCell::new(Tally::default());
    let abandoned = RefCell::new(Vec::<Abandoned>::new());

    let workers = (0..pool.workers.max(1)).map(|_| {
        let (tx, rx) = (&tx, &rx);
        let (outstanding, completed) = (&outstanding, &completed);
        let (tally, abandoned) = (&tally, &abandoned);
        let attempt = &attempt;
        async move {
            while let Ok(mut ticket) = rx.recv().await {
                // The breaker is checked on the way *in*, not on the way out:
                // a tripped run must stop issuing requests, and every job still
                // in the queue is then abandoned rather than tried.
                // The run's own clock, checked where the time is actually
                // spent. See `Pacer::past_deadline`.
                if pool.pacer.past_deadline() {
                    tally.borrow_mut().markets_out_of_time += 1;
                    retire(outstanding, tx);
                    continue;
                }
                if let Some(reason) = pool.pacer.tripped() {
                    if let (Some(trace), 0) = (pool.trace, tally.borrow().abandoned) {
                        // Once, on the first job to find the breaker open.
                        trace(&PaceEvent::BreakerTripped { reason: &format!("{reason:?}") });
                    }
                    tally.borrow_mut().abandoned += 1;
                    retire(outstanding, tx);
                    continue;
                }

                pool.pacer.acquire().await;
                ticket.attempts += 1;
                if ticket.attempts == 1 {
                    // Stamped after the pacing wait, not before it: the retry
                    // budget bounds how long a job spends *failing*, and time
                    // spent queued behind the rate limit is not that.
                    ticket.first_attempt_ms = pool.pacer.now_ms();
                }
                let outcome = attempt(ticket.job.clone()).await;

                if outcome.ok {
                    pool.pacer.observe_ok();
                } else if is_throttle(outcome.status) {
                    pool.pacer.observe_throttled(outcome.retry_after.as_deref());
                } else {
                    pool.pacer.observe_failure();
                }

                if outcome.ok || is_throttle(outcome.status) {
                    // Reported before the retry decision, so a throttle is
                    // visible the moment it lands rather than after the wait.
                    if let (Some(trace), true) = (pool.trace, is_throttle(outcome.status)) {
                        trace(&PaceEvent::Throttled {
                            status: outcome.status.unwrap_or(429),
                            retry_after: outcome.retry_after.as_deref(),
                            new_rate: pool.pacer.rate(),
                        });
                    }
                }

                // A definitive "nothing here" retires quietly: it is neither a
                // success to rank nor a failure to report.
                if outcome.absent {
                    tally.borrow_mut().markets_absent += 1;
                    completed.set(completed.get() + 1);
                    if let Some(report) = pool.report {
                        report(&ticket.job, &outcome, ticket.attempts, completed.get());
                    }
                    retire(outstanding, tx);
                    continue;
                }

                if !outcome.ok {
                    let transient = crate::sweep::is_transient_status(outcome.status);
                    let before = pool.pacer.spent().waited_ms;
                    let give_up = pool
                        .pacer
                        .retry_after_failure(transient, ticket.attempts, ticket.first_attempt_ms)
                        .await;
                    match give_up {
                        None => {
                            if let Some(trace) = pool.trace {
                                trace(&PaceEvent::Retrying {
                                    station: ticket.job.label(),
                                    attempt: ticket.attempts,
                                    delay_ms: pool.pacer.spent().waited_ms - before,
                                    status: outcome.status,
                                });
                            }
                            // Back of the queue, not the front: a market that
                            // just failed is the least likely to succeed if
                            // tried again immediately, and going to the back
                            // gives the rest of the region a turn first.
                            let _ = tx.try_send(ticket);
                            continue;
                        }
                        Some(reason) => {
                            if let Some(trace) = pool.trace {
                                trace(&PaceEvent::GaveUp {
                                    station: ticket.job.label(),
                                    attempts: ticket.attempts,
                                    reason: &format!("{reason:?}"),
                                });
                            }
                            if let Some(report) = pool.report {
                                report(&ticket.job, &outcome, ticket.attempts, completed.get());
                            }
                            count_failure(tally, &ticket.job);
                            abandoned.borrow_mut().push(Abandoned {
                                job: ticket.job,
                                reason,
                                attempts: ticket.attempts,
                                status: outcome.status,
                            });
                            retire(outstanding, tx);
                            continue;
                        }
                    }
                }

                // Queue the follow-on *before* retiring this job, so the
                // counter never dips through zero between a system landing and
                // its markets being queued — which would close the channel with
                // work still to do.
                let Outcome { follow_on, status, retry_after, ok, tradable, absent } = outcome;
                let outcome =
                    Outcome { status, retry_after, ok, tradable, absent, follow_on: Vec::new() };
                for next in follow_on {
                    outstanding.set(outstanding.get() + 1);
                    let _ = tx.try_send(Ticket {
                        job: next,
                        attempts: 0,
                        first_attempt_ms: f64::NAN,
                    });
                }

                count_success(tally, &ticket.job);
                completed.set(completed.get() + 1);
                if let Some(report) = pool.report {
                    report(&ticket.job, &outcome, ticket.attempts, completed.get());
                }
                retire(outstanding, tx);
            }
        }
    });

    futures_util::future::join_all(workers).await;
    (tally.into_inner(), abandoned.into_inner())
}

/// One job is done with, whatever the outcome. Closes the channel when the last
/// one retires.
fn retire<T>(outstanding: &Cell<usize>, tx: &async_channel::Sender<T>) {
    let left = outstanding.get() - 1;
    outstanding.set(left);
    if left == 0 {
        tx.close();
    }
}

fn count_success(tally: &RefCell<Tally>, job: &Job) {
    let mut tally = tally.borrow_mut();
    match job {
        Job::System { .. } => tally.systems_read += 1,
        Job::Market { .. } => tally.markets_polled += 1,
    }
}

fn count_failure(tally: &RefCell<Tally>, job: &Job) {
    let mut tally = tally.borrow_mut();
    match job {
        Job::System { .. } => tally.systems_failed += 1,
        Job::Market { .. } => tally.markets_failed += 1,
    }
}

/// A throttle, as distinct from any other failure.
///
/// 503 counts only when it carried a `Retry-After`; a bare 503 is an outage and
/// halving the rate for it would be reading a server crash as backpressure.
/// The caller supplies the header, so this test is on status alone and the
/// pacer decides what to do with the header it was handed.
const fn is_throttle(status: Option<u16>) -> bool {
    matches!(status, Some(429))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{CountingEntropy, FixedClock, RecordingTimer};
    use crate::route::pacer::Pacing;
    use edm_core::js::text::Metric;
    use edm_core::pace::{Breaker, Bucket};

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a current-thread runtime")
            .block_on(future)
    }

    fn systems(names: &[&str]) -> Vec<Job> {
        names
            .iter()
            .enumerate()
            .map(|(n, name)| Job::System { name: (*name).to_owned(), address: n as f64 })
            .collect()
    }

    fn market(id: f64, station: &str, system: &str) -> Job {
        Job::Market { market_id: id, station: station.to_owned(), system: system.to_owned() }
    }

    struct Bed {
        clock: FixedClock,
        timer: RecordingTimer,
        entropy: CountingEntropy,
        out: Out,
    }

    impl Default for Bed {
        fn default() -> Self {
            Self {
                clock: FixedClock { now_ms: 0.0, uptime_seconds: 0.0 },
                timer: RecordingTimer::default(),
                entropy: CountingEntropy::default(),
                out: Out::capturing(200, Metric::Utf16, false),
            }
        }
    }

    /// A system read enqueues its markets, and the run does not finish until
    /// they have been polled too. This is the invariant that a naive
    /// "drop the senders" pool gets wrong: the queue is momentarily empty
    /// between the last system landing and its markets arriving.
    #[test]
    fn follow_on_work_keeps_the_queue_open() {
        let bed = Bed::default();
        let pacer = Pacer::new(Pacing::default(), &bed.clock, &bed.timer, &bed.entropy);
        let pool = Pool { pacer: &pacer, out: &bed.out, workers: 4, quiet: true, report: None, trace: None };
        let seen = RefCell::new(Vec::<String>::new());

        let (tally, abandoned) = block_on(run(&pool, systems(&["Sol"]), |job| {
            seen.borrow_mut().push(job.label().to_owned());
            let follow_on = match job {
                Job::System { .. } => {
                    vec![market(1.0, "Abraham Lincoln", "Sol"), market(2.0, "Galileo", "Sol")]
                }
                Job::Market { .. } => Vec::new(),
            };
            async move { Outcome { status: Some(200), ok: true, follow_on, ..Outcome::default() } }
        }));

        assert_eq!(tally, Tally { systems_read: 1, markets_polled: 2, ..Tally::default() });
        assert!(abandoned.is_empty());
        assert_eq!(seen.borrow().len(), 3);
    }

    /// A market that never answers retires, and is reported — the coverage
    /// table needs to name it, because a route through an unread market must
    /// never be silently omitted as though it had ranked low.
    #[test]
    fn a_permanently_failing_market_is_reported_not_dropped() {
        let bed = Bed::default();
        let pacing = Pacing {
            budget: edm_core::pace::Budget {
                per_job_ms: 1e9,
                hard_attempts: 3,
                run_deadline_ms: 1e9,
            },
            breaker: Breaker { window: 100, threshold: 1.1, ..Breaker::default() },
            ..Pacing::default()
        };
        let pacer = Pacer::new(pacing, &bed.clock, &bed.timer, &bed.entropy);
        let pool = Pool { pacer: &pacer, out: &bed.out, workers: 2, quiet: true, report: None, trace: None };
        let tries = Cell::new(0usize);

        let (tally, abandoned) = block_on(run(
            &pool,
            vec![market(7.0, "Sisyphus Dock", "Nowhere")],
            |_| {
                tries.set(tries.get() + 1);
                async { Outcome { status: Some(503), ok: false, ..Outcome::default() } }
            },
        ));

        assert_eq!(tally.markets_failed, 1);
        assert_eq!(tally.markets_polled, 0);
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].reason, GiveUpReason::AttemptCap);
        assert_eq!(abandoned[0].attempts, 3);
        assert_eq!(tries.get(), 3, "and it stopped trying");
    }

    /// A 404 is the request being wrong, not the server being busy. Retrying it
    /// repeats the mistake, so it retires on the first attempt.
    #[test]
    fn a_non_transient_failure_is_not_retried() {
        let bed = Bed::default();
        let pacer = Pacer::new(Pacing::default(), &bed.clock, &bed.timer, &bed.entropy);
        let pool = Pool { pacer: &pacer, out: &bed.out, workers: 1, quiet: true, report: None, trace: None };
        let tries = Cell::new(0usize);

        let (_, abandoned) = block_on(run(&pool, systems(&["Nowhere"]), |_| {
            tries.set(tries.get() + 1);
            async { Outcome { status: Some(404), ok: false, ..Outcome::default() } }
        }));

        assert_eq!(tries.get(), 1);
        assert_eq!(abandoned[0].reason, GiveUpReason::NotTransient);
    }

    /// The pacer bounds the *run*, not each worker: eight workers behind a
    /// two-per-second bucket still issue two per second.
    #[test]
    fn concurrency_does_not_raise_the_rate() {
        let bed = Bed::default();
        let pacing =
            Pacing { bucket: Bucket { rate: 2.0, burst: 1.0, min_rate: 0.5 }, ..Pacing::default() };
        let pacer = Pacer::new(pacing, &bed.clock, &bed.timer, &bed.entropy);
        let pool = Pool { pacer: &pacer, out: &bed.out, workers: 8, quiet: true, report: None, trace: None };

        block_on(run(&pool, systems(&["A", "B", "C", "D"]), |_| async {
            Outcome { status: Some(200), ok: true, ..Outcome::default() }
        }));

        // One free from the burst, then 500 ms apart. The clock is frozen, so
        // these are absolute instants rather than gaps.
        assert_eq!(bed.timer.delays(), vec![500.0, 1_000.0, 1_500.0]);
    }

    /// A tripped breaker stops the run without draining the queue one request
    /// at a time. What is left is abandoned, and counted as abandoned.
    #[test]
    fn a_tripped_breaker_stops_issuing_requests() {
        let bed = Bed::default();
        let pacing = Pacing {
            breaker: Breaker { window: 4, threshold: 0.5, ..Breaker::default() },
            budget: edm_core::pace::Budget {
                per_job_ms: 1e9,
                hard_attempts: 1,
                run_deadline_ms: 1e9,
            },
            ..Pacing::default()
        };
        let pacer = Pacer::new(pacing, &bed.clock, &bed.timer, &bed.entropy);
        let pool = Pool { pacer: &pacer, out: &bed.out, workers: 1, quiet: true, report: None, trace: None };
        let tries = Cell::new(0usize);

        let jobs: Vec<Job> = (0..40).map(|n| market(f64::from(n), "X", "Y")).collect();
        let (tally, _) = block_on(run(&pool, jobs, |_| {
            tries.set(tries.get() + 1);
            async { Outcome { status: Some(500), ok: false, ..Outcome::default() } }
        }));

        assert!(pacer.tripped().is_some(), "the breaker must have tripped");
        assert!(tries.get() < 40, "it stopped early: {} of 40", tries.get());
        assert!(tally.abandoned > 0);
        assert_eq!(tally.markets_failed + tally.abandoned, 40, "every job accounted for");
    }

    /// An empty region is not an error and costs nothing.
    #[test]
    fn an_empty_seed_sends_nothing() {
        let bed = Bed::default();
        let pacer = Pacer::new(Pacing::default(), &bed.clock, &bed.timer, &bed.entropy);
        let pool = Pool { pacer: &pacer, out: &bed.out, workers: 4, quiet: true, report: None, trace: None };

        let (tally, abandoned) = block_on(run(&pool, Vec::new(), |_| async {
            unreachable!("nothing to do")
        }));

        assert_eq!(tally, Tally::default());
        assert!(abandoned.is_empty());
        assert_eq!(pacer.spent().requests, 0);
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use crate::ports::CountingEntropy;
    use crate::route::pacer::Pacing;
    use edm_core::js::text::Metric;
    use edm_core::pace::{Breaker, Budget, Bucket};

    /// A clock that advances only when the timer sleeps, so the run's own
    /// deadline is reached by pacing rather than by wall time.
    #[derive(Debug, Default)]
    struct Bed {
        now: std::cell::Cell<f64>,
    }

    impl Clock for Bed {
        fn now_ms(&self) -> f64 {
            self.now.get()
        }
        fn uptime_seconds(&self) -> f64 {
            0.0
        }
    }

    impl Timer for Bed {
        async fn sleep_ms(&self, millis: f64) {
            self.now.set(self.now.get() + millis);
        }
    }

    /// `--deadline` is documented as how long the whole sweep may take, and
    /// only the *retry* budget consulted it — which is reached solely from a
    /// failed attempt. A sweep that simply took a long time and succeeded at
    /// everything ran past its limit untouched, and then handed the optimiser a
    /// deadline that had already expired, so every route came back marked
    /// `SearchBudgetExhausted` as though the search had been the slow part.
    #[test]
    fn a_sweep_stops_at_the_run_deadline_even_when_nothing_fails() {
        let bed = Bed::default();
        let entropy = CountingEntropy::default();
        let out = Out::capturing(200, Metric::Utf16, false);
        let pacing = Pacing {
            // One request per second, and two seconds to spend.
            bucket: Bucket { rate: 1.0, burst: 1.0, min_rate: 0.5 },
            budget: Budget { per_job_ms: 1e9, hard_attempts: 8, run_deadline_ms: 2_000.0 },
            breaker: Breaker { window: 100, threshold: 1.1, ..Breaker::default() },
            ..Pacing::default()
        };
        let pacer = Pacer::new(pacing, &bed, &bed, &entropy);
        let pool =
            Pool { pacer: &pacer, out: &out, workers: 1, quiet: true, report: None, trace: None };

        let jobs: Vec<Job> = (0..20)
            .map(|n| Job::Market {
                market_id: f64::from(n),
                station: format!("S{n}"),
                system: "Sol".to_owned(),
            })
            .collect();

        let (tally, abandoned) = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a current-thread runtime")
            .block_on(run(&pool, jobs, |_| async {
                Outcome { status: Some(200), ok: true, ..Outcome::default() }
            }));

        assert!(tally.markets_out_of_time > 0, "the deadline must cut the run short: {tally:?}");
        assert!(tally.markets_polled < 20, "and some markets must be left unpolled: {tally:?}");
        assert_eq!(
            tally.markets_polled + tally.markets_out_of_time,
            20,
            "every job is accounted for: {tally:?}"
        );
        assert!(abandoned.is_empty(), "nothing failed, so nothing was given up on");
    }
}
