//! Request pacing, as arithmetic.
//!
//! `game-internal-api.ts:2988` has a `// Request pacing` section header with an
//! **empty body**. A 429 is classified transient and requeued immediately, with
//! no delay and no `Retry-After` read — so at concurrency 16 the client answers
//! rate limiting by retrying harder. That is affordable for a seven-market
//! system sweep and is not affordable for a thousand-market region, which is
//! why `edm route` paces and `edm market` continues not to \[C27\].
//!
//! Everything here is pure: times are `f64` milliseconds on one monotonic
//! scale, supplied by the caller. The sleeping happens elsewhere, which is what
//! lets a test assert the *sequence of delays* instead of waiting for them.
//!
//! One asymmetry is worth reading twice. `Retry-After` sets a **global**
//! hold-off, not a per-job one: one worker's 429 pauses all sixteen. A per-job
//! backoff would leave the other fifteen hammering a server that has just said
//! stop, which is the failure mode this module exists to prevent.

use crate::js::{js_max, js_min};

/// A token bucket with a minimum inter-request interval.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bucket {
    /// Steady-state rate, requests per second.
    pub rate: f64,
    /// How many requests may be issued back to back after an idle period.
    pub burst: f64,
    /// A floor the multiplicative decrease may not go below, so a run cannot
    /// throttle itself to a standstill.
    pub min_rate: f64,
}

impl Default for Bucket {
    fn default() -> Self {
        Self { rate: 4.0, burst: 8.0, min_rate: 0.5 }
    }
}

/// The mutable half, kept separate so [`Bucket`] stays `Copy` configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BucketState {
    pub tokens: f64,
    pub last_refill_ms: f64,
    /// Nothing may be sent before this instant. Set by `Retry-After` and by the
    /// breaker's cool-down, and **shared by every worker**.
    pub gate_ms: f64,
    /// The currently applied rate, after adaptation.
    pub rate: f64,
    pub consecutive_ok: u32,
}

impl BucketState {
    #[must_use]
    pub fn new(bucket: Bucket, now_ms: f64) -> Self {
        Self {
            tokens: bucket.burst,
            last_refill_ms: now_ms,
            gate_ms: now_ms,
            rate: bucket.rate,
            consecutive_ok: 0,
        }
    }
}

/// After this many clean responses, nudge the rate back up.
const RECOVERY_STREAK: u32 = 20;

impl Bucket {
    /// The instant the next request may be issued, having **reserved** it.
    ///
    /// Reservation is the whole point. Sixteen workers calling this in the same
    /// tick receive sixteen distinct, staggered instants; a check-then-sleep
    /// design would give them all the same one and the stagger would be lost
    /// the moment they woke.
    pub fn reserve(self, state: &mut BucketState, now_ms: f64) -> f64 {
        // The reservation clock only ever moves forward. `last_refill_ms` is a
        // floor as well as a refill point: without it, two callers at the same
        // `now_ms` would each compute the same wait and be handed the same
        // instant, which is precisely the stampede reservation exists to stop.
        let start = js_max(js_max(now_ms, state.gate_ms), state.last_refill_ms);

        let elapsed = js_max(start - state.last_refill_ms, 0.0);
        state.tokens = js_min(state.tokens + elapsed * state.rate / 1000.0, self.burst);
        state.last_refill_ms = start;

        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            return start;
        }
        // Not enough credit yet: wait exactly long enough to earn one token,
        // and consume it now so the next caller queues behind this one.
        let deficit = 1.0 - state.tokens;
        let wait_ms = deficit * 1000.0 / state.rate;
        state.tokens = 0.0;
        state.last_refill_ms = start + wait_ms;
        start + wait_ms
    }

    /// A throttle: halve the rate and, if the server named a time, hold off
    /// until then.
    pub fn on_throttled(self, state: &mut BucketState, now_ms: f64, hold_ms: Option<f64>) {
        state.rate = js_max(state.rate / 2.0, self.min_rate);
        state.consecutive_ok = 0;
        // Tokens are forfeit — a bucketful of credit accumulated before the
        // throttle would otherwise be spent immediately after it.
        state.tokens = 0.0;
        if let Some(hold) = hold_ms {
            state.gate_ms = js_max(state.gate_ms, now_ms + hold);
        }
    }

    /// A clean response. Recovery is additive against the multiplicative
    /// decrease, so the rate returns slowly and backs off fast.
    pub fn on_success(self, state: &mut BucketState) {
        state.consecutive_ok += 1;
        if state.consecutive_ok >= RECOVERY_STREAK && state.rate < self.rate {
            state.rate = js_min(state.rate + self.min_rate, self.rate);
            state.consecutive_ok = 0;
        }
    }
}

/// The largest hold-off that will be honoured, so an absurd or hostile
/// `Retry-After` cannot park a run for a day.
pub const MAX_RETRY_AFTER_MS: f64 = 300_000.0;

/// `Retry-After` — RFC 9110 §10.2.3, which permits **either** delta-seconds or
/// an IMF-fixdate.
///
/// Returns the hold-off in milliseconds from `now_ms`, clamped to
/// `0..=MAX_RETRY_AFTER_MS`. A date already in the past clamps to zero rather
/// than going negative — the same clamp Ardent's future-dated rows need in the
/// other direction.
#[must_use]
pub fn retry_after_ms(header: &str, now_ms: f64) -> Option<f64> {
    let header = crate::js::text::js_trim(header);
    if header.is_empty() {
        return None;
    }

    // Delta-seconds is the common form and is unambiguous: a bare integer.
    if header.bytes().all(|b| b.is_ascii_digit()) {
        let seconds = crate::js::to_number(header);
        if !seconds.is_finite() {
            return None;
        }
        return Some((seconds * 1000.0).clamp(0.0, MAX_RETRY_AFTER_MS));
    }

    let at_ms = imf_fixdate_ms(header)?;
    Some((at_ms - now_ms).clamp(0.0, MAX_RETRY_AFTER_MS))
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` — the only date form a sender is required to
/// produce, and the only one parsed here.
///
/// Hand-rolled because the workspace has no HTTP-date parser and pulling a date
/// crate in for one fixed-width format would be the larger change. The format
/// is rigid, which is what makes forty lines sufficient.
#[must_use]
pub fn imf_fixdate_ms(text: &str) -> Option<f64> {
    const MONTHS: [&str; 12] =
        ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

    // "Sun, 06 Nov 1994 08:49:37 GMT" — 29 characters, fixed width throughout.
    let bytes = text.as_bytes();
    if bytes.len() != 29 || bytes[3] != b',' || bytes[4] != b' ' || !text.ends_with(" GMT") {
        return None;
    }
    // The day-of-week is redundant with the date and RFC 9110 tells recipients
    // not to rely on it — but it is part of the grammar, and a header that does
    // not match the grammar is far more likely to be junk than to be a real
    // date with a typo. Rejecting is the safer reading of an unparseable
    // hold-off, since the fallback is ordinary exponential backoff.
    if !DAYS.contains(&text.get(0..3)?) {
        return None;
    }
    let field = |from: usize, to: usize| -> Option<i64> {
        let slice = text.get(from..to)?;
        if !slice.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        slice.parse().ok()
    };

    let day = field(5, 7)?;
    let month_name = text.get(8..11)?;
    let month = MONTHS.iter().position(|m| *m == month_name)? as i64 + 1;
    let year = field(12, 16)?;
    let hour = field(17, 19)?;
    let minute = field(20, 22)?;
    let second = field(23, 25)?;

    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // Howard Hinnant's `days_from_civil`, the inverse of the one `js::time`
    // already uses to format.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000) as f64)
}

/// Full-jitter exponential backoff: `random(0, min(cap, base * 2^attempt))`.
///
/// Full jitter rather than equal or decorrelated, because the failure being
/// defended against is a synchronised retry storm across sixteen workers, and
/// full jitter is the variant that spreads them most.
///
/// `unit` is a sample in `[0, 1)` supplied by the caller, which keeps this pure
/// and lets the harness pin it to zero \[C29\].
#[must_use]
pub fn backoff_ms(attempt: u32, base_ms: f64, cap_ms: f64, unit: f64) -> f64 {
    let ceiling = js_min(base_ms * 2f64.powi(attempt.min(30) as i32), cap_ms);
    (ceiling * unit.clamp(0.0, 1.0)).clamp(0.0, cap_ms)
}

/// A sliding window of outcomes, and the decision to stop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Breaker {
    pub window: usize,
    /// Failure fraction at which the run aborts.
    pub threshold: f64,
    /// Below this many samples the rate is not yet meaningful.
    pub min_samples: usize,
    /// A separate, faster trip: this many throttles in a row means the server
    /// is asking us to stop and the rate adaptation is not keeping up.
    pub consecutive_throttle_limit: u32,
}

impl Default for Breaker {
    fn default() -> Self {
        Self {
            window: 100,
            threshold: 0.25,
            min_samples: 20,
            consecutive_throttle_limit: 8,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BreakerWindow {
    outcomes: std::collections::VecDeque<bool>,
    failures: usize,
    consecutive_throttles: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Tripped(TripReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TripReason {
    FailureRate,
    ConsecutiveThrottles,
}

impl Breaker {
    pub fn observe(self, state: &mut BreakerWindow, ok: bool, throttled: bool) -> BreakerState {
        if throttled {
            state.consecutive_throttles += 1;
        } else if ok {
            state.consecutive_throttles = 0;
        }

        state.outcomes.push_back(ok);
        if !ok {
            state.failures += 1;
        }
        if state.outcomes.len() > self.window
            && let Some(evicted) = state.outcomes.pop_front()
            && !evicted
        {
            state.failures -= 1;
        }

        if state.consecutive_throttles >= self.consecutive_throttle_limit {
            return BreakerState::Tripped(TripReason::ConsecutiveThrottles);
        }
        // Clamped to the window, which can only ever hold `window` samples.
        // A `min_samples` above it would mean the condition is never met and
        // the breaker never trips — a configuration that silently disables the
        // one safeguard standing between a wide sweep and a thousand doomed
        // requests. Degrading to "trip as soon as the window is full" is the
        // failure direction that keeps the safeguard alive.
        if state.outcomes.len() >= self.min_samples.min(self.window) {
            let rate = state.failures as f64 / state.outcomes.len() as f64;
            if rate >= self.threshold {
                return BreakerState::Tripped(TripReason::FailureRate);
            }
        }
        BreakerState::Closed
    }
}

#[cfg(test)]
mod breaker_tests {
    use super::*;

    /// A `min_samples` above the window would otherwise disable the breaker
    /// outright: the deque never grows past `window`, so the comparison never
    /// holds and no failure rate, however total, ever trips it.
    #[test]
    fn a_min_samples_above_the_window_still_trips() {
        let breaker = Breaker { window: 4, threshold: 0.5, min_samples: 20, ..Breaker::default() };
        let mut window = BreakerWindow::default();

        let mut tripped = None;
        for _ in 0..10 {
            if let BreakerState::Tripped(reason) = breaker.observe(&mut window, false, false) {
                tripped = Some(reason);
                break;
            }
        }

        assert_eq!(tripped, Some(TripReason::FailureRate));
    }

    /// And the ordinary case is unchanged: below `min_samples` a run of bad
    /// luck is not yet evidence.
    #[test]
    fn a_short_run_of_failures_is_not_yet_evidence() {
        let breaker = Breaker::default();
        let mut window = BreakerWindow::default();
        for _ in 0..(Breaker::default().min_samples - 1) {
            assert_eq!(breaker.observe(&mut window, false, false), BreakerState::Closed);
        }
        assert!(matches!(
            breaker.observe(&mut window, false, false),
            BreakerState::Tripped(TripReason::FailureRate)
        ));
    }
}

/// How long one job may keep being retried.
///
/// This replaces R98 — `--requeue` counts attempts and is unbounded, so a
/// permanently-500ing market can loop indefinitely. Counting wall clock instead
/// bounds the damage whatever the failure rate, and bounds the *run* as well as
/// the job.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub per_job_ms: f64,
    /// A backstop against a pathologically fast failure loop.
    pub hard_attempts: u32,
    pub run_deadline_ms: f64,
}

impl Default for Budget {
    fn default() -> Self {
        Self { per_job_ms: 120_000.0, hard_attempts: 8, run_deadline_ms: 3_600_000.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryVerdict {
    Retry,
    GiveUp(GiveUpReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GiveUpReason {
    /// A 4xx that is not 408 or 429: the request is wrong, and repeating it
    /// repeats the mistake.
    NotTransient,
    BudgetExhausted,
    AttemptCap,
    RunDeadline,
    /// The shared circuit breaker opened before this queued job could run.
    CircuitBreaker,
}

impl Budget {
    #[must_use]
    pub fn verdict(
        self,
        transient: bool,
        attempts: u32,
        first_attempt_ms: f64,
        now_ms: f64,
        run_started_ms: f64,
    ) -> RetryVerdict {
        if !transient {
            return RetryVerdict::GiveUp(GiveUpReason::NotTransient);
        }
        if now_ms - run_started_ms >= self.run_deadline_ms {
            return RetryVerdict::GiveUp(GiveUpReason::RunDeadline);
        }
        if now_ms - first_attempt_ms >= self.per_job_ms {
            return RetryVerdict::GiveUp(GiveUpReason::BudgetExhausted);
        }
        if attempts >= self.hard_attempts {
            return RetryVerdict::GiveUp(GiveUpReason::AttemptCap);
        }
        RetryVerdict::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_stagger_rather_than_stampede() {
        let bucket = Bucket { rate: 4.0, burst: 2.0, min_rate: 0.5 };
        let mut state = BucketState::new(bucket, 0.0);

        // The burst is spent first, at once.
        assert_eq!(bucket.reserve(&mut state, 0.0), 0.0);
        assert_eq!(bucket.reserve(&mut state, 0.0), 0.0);
        // Then every further caller in the same tick gets its own instant,
        // 250 ms apart at 4 req/s — not all the same one.
        assert_eq!(bucket.reserve(&mut state, 0.0), 250.0);
        assert_eq!(bucket.reserve(&mut state, 0.0), 500.0);
        assert_eq!(bucket.reserve(&mut state, 0.0), 750.0);
    }

    #[test]
    fn a_hold_off_applies_to_every_worker() {
        let bucket = Bucket::default();
        let mut state = BucketState::new(bucket, 0.0);
        bucket.on_throttled(&mut state, 1_000.0, Some(2_000.0));

        // The worker that was throttled and every one of its peers.
        for _ in 0..4 {
            assert!(bucket.reserve(&mut state, 1_000.0) >= 3_000.0);
        }
    }

    #[test]
    fn throttling_halves_and_recovery_creeps() {
        let bucket = Bucket { rate: 4.0, burst: 8.0, min_rate: 0.5 };
        let mut state = BucketState::new(bucket, 0.0);

        bucket.on_throttled(&mut state, 0.0, None);
        assert_eq!(state.rate, 2.0);
        bucket.on_throttled(&mut state, 0.0, None);
        assert_eq!(state.rate, 1.0);

        // The floor holds however many throttles arrive.
        for _ in 0..10 {
            bucket.on_throttled(&mut state, 0.0, None);
        }
        assert_eq!(state.rate, bucket.min_rate);

        // Recovery is additive and needs a streak, so one good reply does not
        // undo the backoff.
        bucket.on_success(&mut state);
        assert_eq!(state.rate, bucket.min_rate);
        for _ in 0..RECOVERY_STREAK {
            bucket.on_success(&mut state);
        }
        assert_eq!(state.rate, 1.0);
    }

    #[test]
    fn retry_after_reads_both_forms() {
        // Delta-seconds.
        assert_eq!(retry_after_ms("2", 0.0), Some(2_000.0));
        assert_eq!(retry_after_ms("  30  ", 0.0), Some(30_000.0));
        // Clamped rather than honoured.
        assert_eq!(retry_after_ms("86400", 0.0), Some(MAX_RETRY_AFTER_MS));

        // IMF-fixdate. 1994-11-06T08:49:37Z is 784,111,777 s.
        let epoch = 784_111_777_000.0;
        assert_eq!(imf_fixdate_ms("Sun, 06 Nov 1994 08:49:37 GMT"), Some(epoch));
        // A date in the past is a hold-off of zero, never a negative one.
        assert_eq!(retry_after_ms("Sun, 06 Nov 1994 08:49:37 GMT", epoch + 5_000.0), Some(0.0));
        assert_eq!(retry_after_ms("Sun, 06 Nov 1994 08:49:37 GMT", epoch - 3_000.0), Some(3_000.0));

        // Anything else is not a hold-off, and must not be read as one.
        for junk in ["", "soon", "-5", "2.5", "Sun 06 Nov 1994 08:49:37 GMT", "Xxx, 06 Nov 1994 08:49:37 GMT"] {
            assert_eq!(retry_after_ms(junk, 0.0), None, "{junk}");
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let (base, cap) = (100.0, 10_000.0);
        // With the jitter pinned to its maximum, growth is the ceiling itself.
        assert_eq!(backoff_ms(0, base, cap, 1.0), 100.0);
        assert_eq!(backoff_ms(3, base, cap, 1.0), 800.0);
        assert_eq!(backoff_ms(20, base, cap, 1.0), cap);
        // Full jitter means the floor is always zero.
        assert_eq!(backoff_ms(5, base, cap, 0.0), 0.0);
        // And nothing exceeds the cap at any attempt or sample.
        for attempt in 0..40 {
            for unit in [0.0, 0.5, 0.999] {
                assert!(backoff_ms(attempt, base, cap, unit) <= cap);
            }
        }
    }

    #[test]
    fn the_breaker_trips_on_a_streak_before_a_rate() {
        let breaker = Breaker::default();
        let mut window = BreakerWindow::default();

        // Seven throttles in a row is not yet enough, and the failure rate has
        // too few samples to speak.
        for _ in 0..7 {
            assert_eq!(breaker.observe(&mut window, false, true), BreakerState::Closed);
        }
        assert_eq!(
            breaker.observe(&mut window, false, true),
            BreakerState::Tripped(TripReason::ConsecutiveThrottles)
        );
    }

    #[test]
    fn the_breaker_needs_a_meaningful_sample() {
        let breaker = Breaker::default();
        let mut window = BreakerWindow::default();
        // Every one of the first few fails, which is a rate of 1.0 — but on too
        // little evidence to abort a run over.
        for _ in 0..(breaker.min_samples - 1) {
            assert_eq!(breaker.observe(&mut window, false, false), BreakerState::Closed);
        }
        assert_eq!(
            breaker.observe(&mut window, false, false),
            BreakerState::Tripped(TripReason::FailureRate)
        );
    }

    /// The direct contrast with R98: a job that keeps failing retires on wall
    /// clock, not on an attempt count that never runs out.
    #[test]
    fn a_job_retires_on_time_rather_than_on_attempts() {
        let budget = Budget { per_job_ms: 10_000.0, hard_attempts: 100, run_deadline_ms: 1e9 };

        assert_eq!(budget.verdict(true, 1, 0.0, 5_000.0, 0.0), RetryVerdict::Retry);
        assert_eq!(
            budget.verdict(true, 2, 0.0, 10_000.0, 0.0),
            RetryVerdict::GiveUp(GiveUpReason::BudgetExhausted)
        );
        // A 4xx is not retried at all, however much budget remains.
        assert_eq!(
            budget.verdict(false, 1, 0.0, 0.0, 0.0),
            RetryVerdict::GiveUp(GiveUpReason::NotTransient)
        );
        // And the run's own deadline outranks the job's: this job has barely
        // started, but the sweep as a whole is out of time.
        let short_run = Budget { run_deadline_ms: 5_000.0, ..budget };
        assert_eq!(
            short_run.verdict(true, 1, 9_000.0, 9_100.0, 0.0),
            RetryVerdict::GiveUp(GiveUpReason::RunDeadline)
        );
    }
}
