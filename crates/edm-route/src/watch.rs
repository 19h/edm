//! The caller's clock, and somewhere for a search to say what it is doing.
//!
//! This crate has no clock and no output, and both absences are enforced rather
//! than intended: the `purity` gate in `cargo xtask gates` fails the build if
//! `edm-route`'s normal dependency tree reaches tokio, reqwest, rustix or
//! getrandom, and a search whose answer depended on what time it was would stop
//! being reproducible — which is the whole value of the thing.
//!
//! # Why a wall-clock budget is a predicate the caller supplies
//!
//! A step counter is the obvious alternative, and this crate already has one:
//! [`crate::model::Limits::search_budget`] caps how many partial paths the
//! `min_distinct` branch and bound expands. That is the right shape for *that*
//! limit, because it is a statement about how much of an exponential tree to
//! explore, and it means the same thing on every machine.
//!
//! It is the wrong shape for a wall clock, and the numbers say by how much. One
//! Bellman-Ford sweep is 2 edge relaxations on the two-market fixture and
//! **24,292,232** on a radius-100 sweep (measured 2026-08-06 over 5,049 cached
//! Companion API markets). A probe is `n` sweeps, so one probe ranges from
//! microseconds to **205 seconds** on the same binary — and the graph build
//! ahead of it ranges from microseconds to two minutes. A step count calibrated
//! to "about thirty seconds" is therefore a number that is wrong everywhere
//! except the instance it was measured on, and wrong silently, which is the
//! failure mode this crate is arranged to avoid.
//!
//! So the clock stays where there is one. The caller passes a predicate that
//! answers "is the budget spent?", the search asks, and it never learns what
//! time it is: still no clock, and still reproducible given the same answers.
//! The crate keeps counting steps, but only to decide how *often* to ask — a
//! predicate consulted once per edge relaxation would cost more than the
//! relaxation does.
//!
//! # What an exhausted budget may claim
//!
//! Nothing. A search that stopped because it ran out of time has proved
//! nothing, so it returns the best route it holds under
//! [`crate::report::Guarantee::Heuristic`] with
//! [`crate::report::HeuristicReason::SearchBudgetExhausted`]. The one exception
//! is [`crate::distinct`], which may still say `BoundedGap` — but only when the
//! bound it quotes was itself proved before the clock ran out.

use core::fmt;

use crate::num::Ratio;

/// What a search says about itself while it runs.
///
/// Every variant is a fact about work already done, never a prediction: a
/// progress line that guesses is worse than no progress line, because it is
/// believed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The graph build has finished `done` of `total` commodity pools and has
    /// `edges` legs so far.
    ///
    /// Pools rather than markets, because that is what the build loops over —
    /// and they are visited in descending order of the best trade each could
    /// possibly hold, so the early ones are where the answer comes from and the
    /// later ones are where the time goes.
    Building {
        /// Commodity pools finished.
        done: usize,
        /// Commodity pools in the instance.
        total: usize,
        /// Legs found so far.
        edges: usize,
    },
    /// A Dinkelbach round is beginning.
    ///
    /// `rate` is the best **achievable** rate in hand — a real cycle of `stops`
    /// stations earns it — so a watcher sees the answer improve rather than a
    /// spinner. It never decreases.
    Round {
        /// Which round, counting from one.
        round: u32,
        /// The rate the witness in hand achieves.
        rate: Ratio,
        /// How many stations that witness visits, or zero before there is one.
        stops: usize,
    },
    /// A branch and bound has expanded `paths` of its `budget` partial paths.
    Expanded {
        /// Partial paths expanded so far.
        paths: u64,
        /// The cap from [`crate::model::Limits::search_budget`].
        budget: u64,
    },
    /// A search stopped on the caller's budget rather than on its own stopping
    /// condition, so whatever it returns is heuristic.
    Abandoned,
}

/// The caller's clock and the caller's ears, lent to a search.
///
/// Copy, and cheap: two optional references. The unlimited watch — no budget,
/// no sink — is what every exact claim in this crate's test suite is made
/// under, and it is what [`Default`] gives.
#[derive(Clone, Copy, Default)]
pub struct Watch<'a> {
    expired: Option<&'a dyn Fn() -> bool>,
    sink: Option<&'a dyn Fn(Event)>,
}

impl<'a> Watch<'a> {
    /// A search with no deadline and nobody listening.
    #[must_use]
    pub fn unlimited() -> Self {
        Self { expired: None, sink: None }
    }

    /// Stops the search the first time `expired` answers `true`.
    ///
    /// The predicate is the caller's, and it is the only thing anywhere in the
    /// search that knows what time it is. It is consulted at coarse boundaries
    /// — once per Bellman-Ford sweep, once per branch-and-bound origin, once
    /// every few thousand expansions — so it may be asked long after the
    /// deadline passed, and it must keep answering `true` once it has.
    #[must_use]
    pub fn until(self, expired: &'a dyn Fn() -> bool) -> Self {
        Self { expired: Some(expired), ..self }
    }

    /// Sends progress to `sink`.
    ///
    /// The sink does not print — this crate holds no strings and writes to no
    /// stream. It receives an [`Event`], and the caller decides whether that is
    /// worth a line. The throttling belongs there too: only the caller knows
    /// how long the run has been silent.
    #[must_use]
    pub fn reporting(self, sink: &'a dyn Fn(Event)) -> Self {
        Self { sink: Some(sink), ..self }
    }

    /// Whether the caller's budget is spent.
    #[must_use]
    pub fn expired(&self) -> bool {
        self.expired.is_some_and(|expired| expired())
    }

    /// Hands an event to the sink, if there is one.
    pub fn report(&self, event: Event) {
        if let Some(sink) = self.sink {
            sink(event);
        }
    }
}

impl fmt::Debug for Watch<'_> {
    /// Neither field can be formatted, and neither is worth naming beyond
    /// whether it is there.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watch")
            .field("deadline", &self.expired.is_some())
            .field("sink", &self.sink.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{Event, Watch};
    use crate::num::Ratio;

    #[test]
    fn an_unlimited_watch_never_expires_and_drops_every_event() {
        let watch = Watch::unlimited();
        assert!(!watch.expired());
        // No sink, so this is a no-op rather than a panic.
        watch.report(Event::Abandoned);
    }

    #[test]
    fn the_deadline_predicate_is_the_callers_and_is_asked_every_time() {
        let asked = std::cell::Cell::new(0u32);
        let expired = || {
            asked.set(asked.get() + 1);
            asked.get() > 2
        };
        let watch = Watch::unlimited().until(&expired);
        assert!(!watch.expired());
        assert!(!watch.expired());
        assert!(watch.expired());
        assert_eq!(asked.get(), 3);
    }

    #[test]
    fn every_event_reaches_the_sink_in_order() {
        let seen = std::cell::RefCell::new(Vec::new());
        let sink = |event: Event| seen.borrow_mut().push(event);
        let watch = Watch::unlimited().reporting(&sink);
        watch.report(Event::Building { done: 1, total: 2, edges: 3 });
        watch.report(Event::Round { round: 1, rate: Ratio::ZERO, stops: 0 });
        watch.report(Event::Abandoned);
        assert_eq!(
            seen.into_inner(),
            vec![
                Event::Building { done: 1, total: 2, edges: 3 },
                Event::Round { round: 1, rate: Ratio::ZERO, stops: 0 },
                Event::Abandoned,
            ]
        );
    }
}
