//! What a sweep will cost, decided before any of it is spent.
//!
//! `edm market <system>` polls seven markets and asks nobody's permission,
//! which is proportionate. A radius sweep is a different animal: 20 Ly of Sol
//! is around 1,100 markets before filtering, 50 Ly is 10,000, and every one is
//! an authenticated request against one commander's session on a service whose
//! rate limits nobody has documented.
//!
//! So the cost is computed from free Ardent data, printed, and *then* either
//! refused, confirmed, or sent. This module is the arithmetic and the decision;
//! the printing and the requests live in the binary.
//!
//! The estimate is deliberately a **range**. Fitting one linear model to the
//! measured transfers does not work — Sol at 20 Ly implies about 20 KB per
//! market while Sol at 50 Ly and Shinrarta imply nearly none, because the
//! ~500 KB starsystem reads dominate and their size varies with how populated a
//! system is. A point estimate presented as fact when the underlying data
//! scatters by a third is worse than no estimate, so the prior is published
//! alongside the number.

use crate::js;

/// Requests above which `--yes` is required.
pub const CONFIRM_THRESHOLD: f64 = 250.0;
/// Requests above which nothing is sent at all, absent `--max-requests`.
pub const DEFAULT_MAX_REQUESTS: f64 = 2_000.0;
/// The widest radius accepted.
///
/// **Ardent's own clamp**, and therefore the only number here that is true
/// rather than chosen: ask for more and the server silently narrows the answer,
/// so a completeness claim past it could not be honest.
///
/// It used to be 100, picked as a proxy for "this will be too big". That proxy
/// was redundant — `--max-requests` measures the size directly and refuses on
/// it — and it was wrong about what a wide radius costs, because `--radius`
/// bounds how far each *market* sits from the reference, not how long a route's
/// legs may be. Two markets each within 40 Ly can be 80 Ly apart, so a long-leg
/// route never needed a wide radius. What a wide radius buys is *more markets*,
/// which is exactly what the request ceiling is for.
pub const MAX_RADIUS_LY: f64 = ARDENT_MAX_RADIUS_LY;
/// Ardent's own clamp, which it applies silently.
pub const ARDENT_MAX_RADIUS_LY: f64 = 500.0;
/// Ardent's `/nearby` row cap.
pub const ARDENT_NEARBY_CAP: usize = 1_000;

/// Payload sizes, as measured rather than guessed.
///
/// The starsystem figure is pinned by the sparse case: 662 systems within 50 Ly
/// of Skaudai hold zero markets, and reading them all still moves ~330 MB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SizePrior {
    pub system_bytes: f64,
    pub market_bytes: f64,
    /// Fractional spread either side, so the report can show a range.
    pub spread: f64,
}

impl Default for SizePrior {
    fn default() -> Self {
        Self { system_bytes: 500.0 * 1024.0, market_bytes: 20.0 * 1024.0, spread: 0.3 }
    }
}

/// One line of the plan table: a filter, and what it removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exclusion {
    pub label: &'static str,
    pub removed: usize,
    /// The flag that would keep them.
    pub keep_with: &'static str,
}

/// What the free Ardent pre-count established.
///
/// A struct rather than five positional arguments: they are all `usize` and a
/// transposition would be silent, wrong, and invisible at the call site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub systems: usize,
    /// Systems still holding a candidate market, so worth a starsystem read.
    pub systems_to_read: usize,
    pub stations_known: usize,
    pub markets_to_poll: usize,
    pub cached_fresh: usize,
}

/// Everything the plan table needs, and everything the gate decides on.
#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    pub systems: usize,
    /// Systems that still hold a candidate market, so are worth a starsystem
    /// read at all.
    pub systems_to_read: usize,
    pub stations_known: usize,
    pub exclusions: Vec<Exclusion>,
    pub markets_to_poll: usize,
    pub cached_fresh: usize,
    pub requests: f64,
    pub bytes_low: f64,
    pub bytes_high: f64,
    pub seconds: f64,
}

impl Estimate {
    /// Requests, transfer and wall clock, from counts that cost nothing to
    /// obtain.
    #[must_use]
    pub fn build(counts: Counts, exclusions: Vec<Exclusion>, rate_per_second: f64, prior: &SizePrior) -> Self {
        let Counts { systems, systems_to_read, stations_known, markets_to_poll, cached_fresh } =
            counts;
        let to_poll = markets_to_poll.saturating_sub(cached_fresh);
        let requests = systems_to_read as f64 + to_poll as f64;
        let bytes =
            systems_to_read as f64 * prior.system_bytes + to_poll as f64 * prior.market_bytes;

        Self {
            systems,
            systems_to_read,
            stations_known,
            exclusions,
            markets_to_poll: to_poll,
            cached_fresh,
            requests,
            bytes_low: bytes * (1.0 - prior.spread),
            bytes_high: bytes * (1.0 + prior.spread),
            // Pacing, not concurrency, is what bounds a paced sweep: sixteen
            // workers behind a four-per-second bucket still issue four per
            // second. Concurrency only decides how much of the latency hides.
            seconds: if rate_per_second > 0.0 { requests / rate_per_second } else { 0.0 },
        }
    }
}

/// What the gate decided, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Send it.
    Proceed,
    /// Within the ceiling but large enough to want saying so out loud.
    NeedsConfirmation,
    /// Over the ceiling. Nothing is sent.
    Refused(Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    RadiusTooWide,
    TooManyRequests,
}

/// The ceiling test.
///
/// Radius is checked before requests, because a mistyped radius is the likelier
/// mistake and naming it directly is more use than reporting the forty thousand
/// requests it implies.
#[must_use]
pub fn verdict(
    estimate: &Estimate,
    radius_ly: f64,
    max_requests: f64,
    confirmed: bool,
) -> Verdict {
    if radius_ly > MAX_RADIUS_LY {
        return Verdict::Refused(Refusal::RadiusTooWide);
    }
    if estimate.requests > max_requests {
        return Verdict::Refused(Refusal::TooManyRequests);
    }
    if estimate.requests > CONFIRM_THRESHOLD && !confirmed {
        return Verdict::NeedsConfirmation;
    }
    Verdict::Proceed
}

/// The message a refusal prints. It names both numbers and the flag that would
/// change the answer, because a refusal that does not say how to proceed is
/// just an obstacle.
#[must_use]
pub fn refusal_message(refusal: &Refusal, estimate: &Estimate, radius_ly: f64, max_requests: f64) -> String {
    match refusal {
        Refusal::RadiusTooWide => format!(
            "--radius {} exceeds the {} Ly ceiling. Nothing has been sent.",
            js::js_number(radius_ly),
            js::js_number(MAX_RADIUS_LY),
        ),
        Refusal::TooManyRequests => format!(
            "Estimated {} Companion API requests, above the {} ceiling.\n\
             Narrow the sweep (--radius, --pad, --max-star-distance) or raise it with\n\
             --max-requests {}. Nothing has been sent.",
            js::format_integer(estimate.requests),
            js::format_integer(max_requests),
            js::format_integer((estimate.requests * 1.2).ceil()),
        ),
    }
}

/// The prompt shown when a sweep is large but permitted.
#[must_use]
pub fn confirmation_message(estimate: &Estimate) -> String {
    format!(
        "pass --yes to send {} requests to the Companion API",
        js::format_integer(estimate.requests)
    )
}

/// A transfer range, rendered the way the plan table shows it.
#[must_use]
pub fn transfer_range(estimate: &Estimate) -> String {
    // Kilobytes below a megabyte, because "0-0 MB" for a forty-kilobyte sweep
    // is not a rounding artefact the reader should have to decode — and the
    // small end of the range is exactly where a user is deciding whether the
    // sweep is worth running at all.
    const MB: f64 = 1024.0 * 1024.0;
    if estimate.bytes_high < MB {
        let kb = |bytes: f64| js::format_integer(js::js_round(bytes / 1024.0));
        return format!("{}-{} KB", kb(estimate.bytes_low), kb(estimate.bytes_high));
    }
    let mb = |bytes: f64| js::format_integer(js::js_round(bytes / MB));
    format!("{}-{} MB", mb(estimate.bytes_low), mb(estimate.bytes_high))
}

/// Wall clock, rendered coarsely because the estimate does not deserve
/// precision.
#[must_use]
pub fn duration_estimate(seconds: f64) -> String {
    if seconds < 90.0 {
        return format!("{}s", js::format_integer(seconds.ceil()));
    }
    let minutes = (seconds / 60.0).floor();
    if minutes < 90.0 {
        return format!("{}m {}s", js::format_integer(minutes), js::format_integer(js::js_round(seconds % 60.0)));
    }
    format!(
        "{}h {}m",
        js::format_integer((minutes / 60.0).floor()),
        js::format_integer(minutes % 60.0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sol_20_ly() -> Estimate {
        // The measured shape of Sol at 20 Ly once the defaults have run:
        // 119 systems, ~1,000 stations Ardent knows, and 157 starports.
        Estimate::build(
            Counts {
                systems: 119,
                systems_to_read: 119,
                stations_known: 1_004,
                markets_to_poll: 157,
                cached_fresh: 0,
            },
            vec![
                Exclusion { label: "fleet carriers", removed: 488, keep_with: "--include-carriers" },
                Exclusion { label: "settlements", removed: 313, keep_with: "--settlements" },
                Exclusion { label: "pad below L", removed: 46, keep_with: "--pad M" },
            ],
            4.0,
            &SizePrior::default(),
        )
    }

    #[test]
    fn the_defaults_keep_a_nearby_sweep_inside_the_ceiling() {
        let estimate = sol_20_ly();
        assert_eq!(estimate.requests, 276.0, "119 starsystem reads plus 157 market polls");
        assert_eq!(verdict(&estimate, 20.0, DEFAULT_MAX_REQUESTS, true), Verdict::Proceed);
        // Large enough to be worth saying out loud, though.
        assert_eq!(
            verdict(&estimate, 20.0, DEFAULT_MAX_REQUESTS, false),
            Verdict::NeedsConfirmation
        );
    }

    /// The point of the whole filter: without it the same sweep is four times
    /// the size and the ceiling would refuse it.
    #[test]
    fn without_the_station_filter_the_ceiling_bites() {
        let unfiltered =
            Estimate::build(Counts { systems: 119, systems_to_read: 119, stations_known: 1_004, markets_to_poll: 1_004, cached_fresh: 0 }, Vec::new(), 4.0, &SizePrior::default());
        assert_eq!(unfiltered.requests, 1_123.0);
        assert_eq!(verdict(&unfiltered, 20.0, DEFAULT_MAX_REQUESTS, true), Verdict::Proceed);

        // And at 50 Ly it is refused outright rather than quietly attempted.
        let wide = Estimate::build(Counts { systems: 1_245, systems_to_read: 1_245, stations_known: 21_900, markets_to_poll: 10_333, cached_fresh: 0 }, Vec::new(), 4.0, &SizePrior::default());
        assert_eq!(
            verdict(&wide, 50.0, DEFAULT_MAX_REQUESTS, true),
            Verdict::Refused(Refusal::TooManyRequests)
        );
    }

    #[test]
    fn a_mistyped_radius_is_named_directly() {
        let estimate = sol_20_ly();
        // Not "40,000 requests" — the radius is the mistake and the message
        // says so.
        assert_eq!(
            verdict(&estimate, 5_000.0, 1e9, true),
            Verdict::Refused(Refusal::RadiusTooWide)
        );
        // And the ceiling itself is accepted: it is Ardent's clamp, not a
        // judgement about size, and `--max-requests` is what refuses a sweep
        // for being too big.
        assert_eq!(verdict(&estimate, MAX_RADIUS_LY, 1e9, true), Verdict::Proceed);
        let message = refusal_message(&Refusal::RadiusTooWide, &estimate, 5_000.0, 1e9);
        // Ungrouped: the message quotes back what was typed, so it can be edited
        // and re-run rather than retyped.
        assert!(message.contains("--radius 5000"), "{message}");
        assert!(message.contains("Nothing has been sent"), "{message}");
    }

    #[test]
    fn a_refusal_says_how_to_proceed() {
        let wide = Estimate::build(Counts { systems: 1_245, systems_to_read: 1_245, stations_known: 21_900, markets_to_poll: 10_333, cached_fresh: 0 }, Vec::new(), 4.0, &SizePrior::default());
        let message = refusal_message(&Refusal::TooManyRequests, &wide, 50.0, DEFAULT_MAX_REQUESTS);
        assert!(message.contains("11,578"), "the actual count: {message}");
        assert!(message.contains("2,000"), "the ceiling: {message}");
        assert!(message.contains("--max-requests"), "the way out: {message}");
    }

    /// Cached markets are not polled, so they must not be paid for either.
    #[test]
    fn a_warm_cache_lowers_the_estimate() {
        let cold = Estimate::build(Counts { systems: 10, systems_to_read: 10, stations_known: 100, markets_to_poll: 100, cached_fresh: 0 }, Vec::new(), 4.0, &SizePrior::default());
        let warm = Estimate::build(Counts { systems: 10, systems_to_read: 10, stations_known: 100, markets_to_poll: 100, cached_fresh: 90 }, Vec::new(), 4.0, &SizePrior::default());
        assert_eq!(cold.requests, 110.0);
        assert_eq!(warm.requests, 20.0);
        assert!(warm.bytes_high < cold.bytes_high);
    }

    /// Only systems that might hold something are read, which is what makes a
    /// sparse region nearly free.
    #[test]
    fn a_sparse_region_costs_almost_nothing() {
        // 662 systems within 50 Ly of Skaudai, not one with a market.
        let sparse = Estimate::build(Counts { systems: 662, ..Counts::default() }, Vec::new(), 4.0, &SizePrior::default());
        assert_eq!(sparse.requests, 0.0);
        assert_eq!(verdict(&sparse, 50.0, DEFAULT_MAX_REQUESTS, false), Verdict::Proceed);
    }

    #[test]
    fn durations_read_as_durations() {
        assert_eq!(duration_estimate(45.0), "45s");
        assert_eq!(duration_estimate(90.0), "1m 30s");
        assert_eq!(duration_estimate(3_600.0), "60m 0s");
        assert_eq!(duration_estimate(7_200.0), "2h 0m");
    }
}
