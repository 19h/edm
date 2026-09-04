//! What every `--follow` loop has in common, kept apart from any one of them
//! \[C43\], \[C52\], \[C53\].
//!
//! A follow session is rounds on a timer, and three things end one whatever
//! it re-reads: `--follow-rounds`, the session request ceiling, and a run of
//! rounds that produced nothing. Route's loop and sell's loop each carried
//! their own copy of those rules, and a full-screen UI that refreshes on the
//! same rules would have been a third. The rules live here; the words each
//! command prints around them stay with the command, because they describe
//! what that command re-reads.

use std::collections::HashSet;

use edm_core::js;
use edm_route::report::Route;

use crate::cmd::route::Ranked;

/// How many consecutive rounds may produce nothing before a session stops
/// \[C46\].
///
/// A follow round re-prices a fixed set and never re-nominates, so when the
/// whole set dies at once no later round can recover it, and the loop would
/// otherwise re-read the same dead markets forever. One empty round is a
/// transient read failure; three is the set genuinely gone.
pub(crate) const BARREN_ROUNDS_BEFORE_STOPPING: usize = 3;

/// The round counter and the stop rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FollowState {
    /// Rounds begun so far.
    pub(crate) round: usize,
    /// Consecutive rounds that produced nothing.
    pub(crate) barren_rounds: usize,
}

impl FollowState {
    /// The note to print when `--follow-rounds` has been reached.
    #[must_use]
    pub(crate) fn round_cap(&self, limit: Option<usize>) -> Option<String> {
        let limit = limit?;
        (self.round >= limit).then(|| {
            format!(
                "--follow-rounds {} reached",
                js::format_integer(limit as f64)
            )
        })
    }

    /// The note to print when the session request ceiling has been reached.
    ///
    /// The only live ceiling this program has: `--max-requests` is otherwise
    /// checked at the gate against an *estimate* and never against what was
    /// actually sent, so an indefinite loop would be bounded by nothing.
    #[must_use]
    pub(crate) fn ceiling(&self, spent_requests: usize, max_requests: f64) -> Option<String> {
        (spent_requests as f64 >= max_requests).then(|| {
            format!(
                "--max-requests {} reached after {} {}",
                js::format_integer(max_requests),
                js::format_integer(self.round as f64),
                if self.round == 1 { "round" } else { "rounds" },
            )
        })
    }

    pub(crate) fn begin_round(&mut self) {
        self.round += 1;
    }

    /// Record whether the round produced anything.
    ///
    /// `Some(n)` when it did not and that makes `n` consecutive empty rounds,
    /// enough to stop on.
    pub(crate) fn record(&mut self, produced: bool) -> Option<usize> {
        if produced {
            self.barren_rounds = 0;
            return None;
        }
        self.barren_rounds += 1;
        (self.barren_rounds >= BARREN_ROUNDS_BEFORE_STOPPING).then_some(self.barren_rounds)
    }
}

/// The routes a follow session re-prices, and where the ship was when they
/// were found.
#[derive(Clone, Debug)]
pub(crate) struct Shortlist {
    /// The shortlist as first solved. Every round is re-evaluated against
    /// *this*, not against what survived the previous round, because
    /// `rescore` only ever filters: without the restore, a carrier offline for
    /// one poll would be deleted from the ranking permanently and could never
    /// come back, so a long session would erode to nothing.
    baseline: Vec<Route>,
    /// The berth the ship was docked at when the shortlist was built \[C49\].
    pub(crate) pinned_at: Option<u64>,
}

impl Shortlist {
    #[must_use]
    pub(crate) fn new(ranked: &Ranked, pinned_at: Option<u64>) -> Self {
        Self {
            baseline: ranked.routes().to_vec(),
            pinned_at,
        }
    }

    /// How many routes the session started with.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.baseline.len()
    }

    /// Put the original shortlist back and forget what was read, so the round
    /// about to start re-prices all of it.
    pub(crate) fn restore(&self, ranked: &mut Ranked) -> HashSet<u64> {
        ranked.routes_mut().clone_from(&self.baseline);
        HashSet::new()
    }

    /// Note where the ship is docked now; true when that changed.
    pub(crate) fn moved(&mut self, now_at: Option<u64>) -> bool {
        let moved = now_at != self.pinned_at;
        self.pinned_at = now_at;
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_round_cap_and_the_ceiling_say_when_they_stop_a_session() {
        let mut follow = FollowState::default();
        assert_eq!(follow.round_cap(None), None);
        assert_eq!(follow.round_cap(Some(1)), None);
        assert_eq!(follow.ceiling(0, 10.0), None);
        follow.begin_round();
        assert_eq!(
            follow.round_cap(Some(1)).as_deref(),
            Some("--follow-rounds 1 reached")
        );
        assert_eq!(
            follow.ceiling(10, 10.0).as_deref(),
            Some("--max-requests 10 reached after 1 round")
        );
        follow.begin_round();
        assert_eq!(
            follow.ceiling(2_000, 2_000.0).as_deref(),
            Some("--max-requests 2,000 reached after 2 rounds")
        );
    }

    /// One empty round is a transient; three in a row is the set gone. A
    /// productive round in between resets the count.
    #[test]
    fn three_consecutive_empty_rounds_stop_a_session() {
        let mut follow = FollowState::default();
        assert_eq!(follow.record(false), None);
        assert_eq!(follow.record(false), None);
        assert_eq!(follow.record(true), None);
        assert_eq!(follow.barren_rounds, 0);
        assert_eq!(follow.record(false), None);
        assert_eq!(follow.record(false), None);
        assert_eq!(follow.record(false), Some(3));
    }

    #[test]
    fn moving_is_a_change_of_berth_including_undocking() {
        let ranked = crate::cmd::route::Ranked::empty();
        let mut shortlist = Shortlist::new(&ranked, Some(7));
        assert!(!shortlist.moved(Some(7)));
        assert!(shortlist.moved(None));
        assert!(shortlist.moved(Some(8)));
        assert!(!shortlist.moved(Some(8)));
    }
}
