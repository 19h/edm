//! The waiting half of the pacing policy.
//!
//! [`edm_core::pace`] decides *when* the next request may go and is pure
//! arithmetic with no runtime; this is the part that actually waits, and it is
//! the only part that needs a clock and a timer. Keeping them apart is what
//! lets a test assert the **sequence of delays** a scenario produces instead of
//! sitting through them — a stronger statement than a wall-clock measurement,
//! and one that runs in microseconds.
//!
//! Shared by every worker through a `&Pacer`, with the mutable state behind a
//! `RefCell`. That is sound here rather than merely convenient: the runtime is
//! `current_thread`, and — more to the point — **no borrow is ever held across
//! an `.await`**. Each method takes the cell, computes, drops the guard, and
//! only then waits. The one place that is easy to get wrong is
//! [`Pacer::acquire`], which is written in two statements for exactly this
//! reason.

use std::cell::RefCell;

use edm_core::js;
use edm_core::pace::{
    Breaker, BreakerState, BreakerWindow, Bucket, BucketState, Budget, GiveUpReason, RetryVerdict,
    backoff_ms, retry_after_ms,
};

use crate::ports::{Clock, Entropy, Timer};

/// The tunables a run is built with.
#[derive(Clone, Copy, Debug)]
pub struct Pacing {
    pub bucket: Bucket,
    pub breaker: Breaker,
    pub budget: Budget,
    /// First backoff step; doubles per attempt, then jittered.
    pub backoff_base_ms: f64,
    pub backoff_cap_ms: f64,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            bucket: Bucket::default(),
            breaker: Breaker::default(),
            budget: Budget::default(),
            backoff_base_ms: 500.0,
            backoff_cap_ms: 30_000.0,
        }
    }
}

/// What a run spent, for the coverage table.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spent {
    pub requests: usize,
    pub throttled: usize,
    pub retries: usize,
    pub waited_ms: f64,
}

#[derive(Debug)]
struct Mutable {
    tokens: BucketState,
    window: BreakerWindow,
    spent: Spent,
    tripped: Option<edm_core::pace::TripReason>,
}

/// The shared pacer.
#[derive(Debug)]
pub struct Pacer<'a, C, T, E> {
    pacing: Pacing,
    clock: &'a C,
    timer: &'a T,
    entropy: &'a E,
    started_ms: f64,
    inner: RefCell<Mutable>,
}

impl<'a, C: Clock, T: Timer, E: Entropy> Pacer<'a, C, T, E> {
    pub fn new(pacing: Pacing, clock: &'a C, timer: &'a T, entropy: &'a E) -> Self {
        let now = clock.now_ms();
        Self {
            pacing,
            clock,
            timer,
            entropy,
            started_ms: now,
            inner: RefCell::new(Mutable {
                tokens: BucketState::new(pacing.bucket, now),
                window: BreakerWindow::default(),
                spent: Spent::default(),
                tripped: None,
            }),
        }
    }

    /// Reserve a slot and wait for it.
    ///
    /// Written as reserve-then-wait, in two statements, because the borrow of
    /// the cell must end before the `.await`. Collapsing this into
    /// `self.wait_until(self.inner.borrow_mut()...)` compiles and then panics
    /// at runtime the first time two workers overlap.
    pub async fn acquire(&self) {
        let now = self.clock.now_ms();
        let at_ms = {
            let mut inner = self.inner.borrow_mut();
            let at = self.pacing.bucket.reserve(&mut inner.tokens, now);
            inner.spent.requests += 1;
            at
        };
        self.wait_until(at_ms).await;
    }

    /// Sleep until an absolute instant on the pacing clock.
    async fn wait_until(&self, at_ms: f64) {
        let delay = js::js_max(at_ms - self.clock.now_ms(), 0.0);
        if delay > 0.0 {
            self.inner.borrow_mut().spent.waited_ms += delay;
            self.timer.sleep_ms(delay).await;
        }
    }

    /// A clean response.
    pub fn observe_ok(&self) {
        let mut inner = self.inner.borrow_mut();
        self.pacing.bucket.on_success(&mut inner.tokens);
        self.record(&mut inner, true, false);
    }

    /// A throttle — 429, or a 503 that named a `Retry-After`.
    ///
    /// The hold-off is **global**: it moves the shared bucket's gate, so one
    /// worker's 429 pauses every other worker too. A per-job backoff would
    /// leave fifteen of sixteen workers hammering a server that just said stop,
    /// which is the failure the original has (`game-internal-api.ts` requeues a
    /// 429 immediately, with no delay and no header read).
    pub fn observe_throttled(&self, retry_after: Option<&str>) {
        let now = self.clock.now_ms();
        let hold = retry_after.and_then(|header| retry_after_ms(header, now));
        let mut inner = self.inner.borrow_mut();
        self.pacing.bucket.on_throttled(&mut inner.tokens, now, hold);
        inner.spent.throttled += 1;
        self.record(&mut inner, false, true);
    }

    /// A failure that was neither success nor throttle.
    pub fn observe_failure(&self) {
        let mut inner = self.inner.borrow_mut();
        self.record(&mut inner, false, false);
    }

    fn record(&self, inner: &mut Mutable, ok: bool, throttled: bool) {
        if let BreakerState::Tripped(reason) = self.pacing.breaker.observe(&mut inner.window, ok, throttled)
        {
            // First trip wins: the reason a run stopped is the one that stopped
            // it, not whichever failure happened to arrive last.
            inner.tripped.get_or_insert(reason);
        }
    }

    /// Whether the run's own wall-clock budget is spent.
    ///
    /// `--deadline` is documented as how long the whole sweep may take, and the
    /// retry budget already carries it — but only [`Budget::verdict`] consulted
    /// it, which is reached solely from a *failed* attempt. A sweep that simply
    /// took a long time and succeeded at everything ran past the limit
    /// untouched, and then handed the optimiser a deadline that had already
    /// expired: every route would come back marked `SearchBudgetExhausted`
    /// because the search never got a millisecond, which reads as the search
    /// having been too slow.
    #[must_use]
    pub fn past_deadline(&self) -> bool {
        self.clock.now_ms() - self.started_ms >= self.pacing.budget.run_deadline_ms
    }

    /// Whether the run should keep going at all.
    #[must_use]
    pub fn tripped(&self) -> Option<edm_core::pace::TripReason> {
        self.inner.borrow().tripped
    }

    /// Decide whether to retry this job, and if so wait out the backoff.
    ///
    /// Returns the reason for giving up, or `None` to mean *go again*. The wait
    /// happens here rather than at the call site so that the delay is always
    /// paid — a caller that forgot would turn the backoff into a busy loop,
    /// which is precisely the bug this module exists to fix.
    pub async fn retry_after_failure(
        &self,
        transient: bool,
        attempts: u32,
        first_attempt_ms: f64,
    ) -> Option<GiveUpReason> {
        let now = self.clock.now_ms();
        let verdict =
            self.pacing.budget.verdict(transient, attempts, first_attempt_ms, now, self.started_ms);
        if let RetryVerdict::GiveUp(reason) = verdict {
            return Some(reason);
        }

        let jitter = self.entropy.jitter_unit();
        let delay = backoff_ms(attempts, self.pacing.backoff_base_ms, self.pacing.backoff_cap_ms, jitter);
        // The bucket's own gate may already be further out than the backoff —
        // if a peer's 429 set a hold-off, this job waits for the later of the
        // two rather than jumping the queue.
        let gate = self.inner.borrow().tokens.gate_ms;
        {
            self.inner.borrow_mut().spent.retries += 1;
        }
        self.wait_until(js::js_max(now + delay, gate)).await;
        None
    }

    /// The pacing clock, for callers that need to stamp a job's first attempt.
    #[must_use]
    pub fn now_ms(&self) -> f64 {
        self.clock.now_ms()
    }

    #[must_use]
    pub fn spent(&self) -> Spent {
        self.inner.borrow().spent
    }

    /// The rate the bucket has adapted to, for the coverage table.
    #[must_use]
    pub fn rate(&self) -> f64 {
        self.inner.borrow().tokens.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{CountingEntropy, FixedClock, RecordingTimer};

    /// A clock that advances by exactly what it is told to sleep, so the pacer
    /// sees time pass the way it would in a real run.
    #[derive(Debug, Default)]
    struct TestBed {
        now: std::cell::Cell<f64>,
        delays: std::cell::RefCell<Vec<f64>>,
    }

    impl Clock for TestBed {
        fn now_ms(&self) -> f64 {
            self.now.get()
        }
        fn uptime_seconds(&self) -> f64 {
            0.0
        }
    }

    impl Timer for TestBed {
        async fn sleep_ms(&self, millis: f64) {
            self.delays.borrow_mut().push(millis);
            self.now.set(self.now.get() + millis);
        }
    }

    /// Entropy with a chosen jitter fraction.
    ///
    /// [`CountingEntropy`] counts up from zero, so its `jitter_unit` is
    /// effectively zero for any run of realistic length — which is exactly what
    /// the parity harness wants (C29 pins jitter to zero so a recorded run
    /// replays), and exactly wrong for a test about backoff *growing*.
    #[derive(Debug)]
    struct FixedJitter(f64);

    impl Entropy for FixedJitter {
        fn nonce_bytes(&self) -> [u8; 6] {
            [0; 6]
        }
        fn jitter_unit(&self) -> f64 {
            self.0
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a current-thread runtime")
            .block_on(future)
    }

    /// The property the whole design turns on: N workers acquiring in the same
    /// tick get N staggered instants rather than one shared one.
    #[test]
    fn concurrent_acquires_are_staggered_not_stampeded() {
        let bed = TestBed::default();
        let entropy = CountingEntropy::default();
        let pacing = Pacing {
            bucket: Bucket { rate: 4.0, burst: 1.0, min_rate: 0.5 },
            ..Pacing::default()
        };
        let pacer = Pacer::new(pacing, &bed, &bed, &entropy);

        block_on(async {
            for _ in 0..4 {
                pacer.acquire().await;
            }
        });

        // One free (the burst), then 250 ms apart at 4 per second. The delays
        // are cumulative against a clock that advances, so each observed sleep
        // is the same quarter second rather than a growing absolute.
        assert_eq!(*bed.delays.borrow(), vec![250.0, 250.0, 250.0]);
        assert_eq!(pacer.spent().requests, 4);
    }

    /// One worker's 429 must pause all of them. A `Retry-After` moves the
    /// shared gate, so the *next* acquire by any worker waits it out.
    #[test]
    fn a_retry_after_holds_off_every_worker() {
        let bed = TestBed::default();
        let entropy = CountingEntropy::default();
        let pacer = Pacer::new(Pacing::default(), &bed, &bed, &entropy);

        pacer.observe_throttled(Some("30"));
        block_on(pacer.acquire());

        assert_eq!(*bed.delays.borrow(), vec![30_000.0]);
        // And the rate halved, so the run does not walk straight back into it.
        assert!(pacer.rate() < Pacing::default().bucket.rate, "{}", pacer.rate());
    }

    /// A `Retry-After` in the past is a real thing servers send, usually a
    /// stale date. It must clamp to zero, not to a negative sleep.
    #[test]
    fn a_retry_after_in_the_past_does_not_wait() {
        let bed = TestBed::default();
        bed.now.set(1_700_000_000_000.0);
        let entropy = CountingEntropy::default();
        let pacer = Pacer::new(Pacing::default(), &bed, &bed, &entropy);

        pacer.observe_throttled(Some("Wed, 21 Oct 2015 07:28:00 GMT"));
        block_on(pacer.acquire());

        // Half a second, not thirty: the stale date sets no hold-off at all,
        // and what remains is the bucket earning back the token the throttle
        // forfeited, at the halved rate. Compare the live `Retry-After` above,
        // where the same acquire waits the full 30 s the server asked for.
        assert_eq!(*bed.delays.borrow(), vec![500.0]);
    }

    /// A permanently failing job retires on the wall-clock budget rather than
    /// spinning, and the reason names the budget rather than the attempt cap.
    #[test]
    fn a_job_retires_on_wall_clock_not_on_attempts() {
        let bed = TestBed::default();
        let entropy = FixedJitter(1.0);
        let pacing = Pacing {
            budget: Budget { per_job_ms: 5_000.0, hard_attempts: 1_000, run_deadline_ms: 1e9 },
            backoff_base_ms: 1_000.0,
            ..Pacing::default()
        };
        let pacer = Pacer::new(pacing, &bed, &bed, &entropy);

        let first = bed.now.get();
        let mut attempts = 0;
        let reason = block_on(async {
            loop {
                attempts += 1;
                if let Some(reason) = pacer.retry_after_failure(true, attempts, first).await {
                    break reason;
                }
                assert!(attempts < 50, "the budget must bound this");
            }
        });

        assert_eq!(reason, GiveUpReason::BudgetExhausted);
        assert!(bed.now.get() - first >= 5_000.0);
    }

    /// Full jitter can legitimately draw zero, and does so on every draw under
    /// the harness's pinned entropy. A wall-clock budget alone would then never
    /// expire, because nothing advances the clock — which is the whole reason
    /// the budget carries a hard attempt cap beside it. Neither bound is
    /// redundant.
    #[test]
    fn zero_jitter_retires_on_the_attempt_cap_instead() {
        let bed = TestBed::default();
        let entropy = FixedJitter(0.0);
        let pacing = Pacing {
            budget: Budget { per_job_ms: 5_000.0, hard_attempts: 6, run_deadline_ms: 1e9 },
            ..Pacing::default()
        };
        let pacer = Pacer::new(pacing, &bed, &bed, &entropy);

        let mut attempts = 0;
        let reason = block_on(async {
            loop {
                attempts += 1;
                if let Some(reason) = pacer.retry_after_failure(true, attempts, 0.0).await {
                    break reason;
                }
                assert!(attempts < 50, "the attempt cap must bound this");
            }
        });

        assert_eq!(reason, GiveUpReason::AttemptCap);
        assert_eq!(attempts, 6);
        assert!(bed.delays.borrow().is_empty(), "and it never slept, hence the cap");
    }

    /// A 4xx that is not 408 or 429 is not retried at all: repeating a wrong
    /// request repeats the mistake.
    #[test]
    fn a_non_transient_failure_is_not_retried() {
        let bed = TestBed::default();
        let entropy = CountingEntropy::default();
        let pacer = Pacer::new(Pacing::default(), &bed, &bed, &entropy);

        let reason = block_on(pacer.retry_after_failure(false, 1, 0.0));

        assert_eq!(reason, Some(GiveUpReason::NotTransient));
        assert!(bed.delays.borrow().is_empty(), "and it does not sleep first");
    }

    /// The recorded timer is the one the harness uses; assert it composes.
    #[test]
    fn the_recording_timer_captures_the_sequence() {
        let clock = FixedClock { now_ms: 0.0, uptime_seconds: 0.0 };
        let timer = RecordingTimer::default();
        let entropy = CountingEntropy::default();
        let pacing =
            Pacing { bucket: Bucket { rate: 2.0, burst: 1.0, min_rate: 0.5 }, ..Pacing::default() };
        let pacer = Pacer::new(pacing, &clock, &timer, &entropy);

        block_on(async {
            pacer.acquire().await;
            pacer.acquire().await;
            pacer.acquire().await;
        });

        // The clock is frozen, so these are absolute offsets rather than gaps.
        assert_eq!(timer.delays(), vec![500.0, 1_000.0]);
    }
}
