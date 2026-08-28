//! Provably optimal trade routes over a set of live market reads.
//!
//! This crate is pure: it takes markets in and returns data out. It never
//! renders, never formats, and holds no strings the caller did not give it.
//! It also has no clock — so a wall-clock budget and a progress report both
//! arrive from outside, through [`watch::Watch`], and that module explains why
//! a step counter would not have done instead.
//!
//! # What it computes
//!
//! Three shapes, and the claims about them differ:
//!
//! - **Single hop** — exact by bounded enumeration. It has no steady-state
//!   rate, because repeating it means flying back with an empty hold.
//! - **Round trip** — exact by enumeration. A round trip is a two-cycle, and
//!   the loop solver agrees with this one by construction.
//! - **The optimal repeatable loop** — the centrepiece. A route flown
//!   repeatedly earns `Σw / Σt`, so the best route is the **maximum ratio
//!   cycle**, which is exactly solvable in polynomial time.
//!
//! # Why the loop answer is stronger than it looks
//!
//! The optimum is always a *simple* cycle. A closed walk decomposes into simple
//! cycles, and the mediant inequality `(a+c)/(b+d) <= max(a/b, c/d)` holds for
//! positive denominators, so the best walk's ratio equals the best simple
//! cycle's: the unconstrained optimum never revisits a station on its own. The
//! same argument survives a length cap, because decomposing a bounded walk
//! yields a simple cycle that is no worse *and no longer*. That is why the
//! bounded-`k` search here is a polynomial dynamic program where the sibling
//! project needed an exponential depth-first search, and why "distinct
//! stations" costs nothing extra.
//!
//! # Exactness
//!
//! Everything is integer arithmetic. Rates are exact rationals compared by
//! `i128` cross-multiplication, never as floating-point quotients — an
//! approximate comparison would hide exactly the class of near-tie these
//! searches exist to resolve. Floating point appears in [`time`], which
//! evaluates a square root, and nowhere else in the solving path; a test in
//! this crate greps its own source to keep it that way.
//!
//! # Overflow
//!
//! At instance bounds — cargo ≤ 2^15, unit margin ≤ 2^20, leg time ≤ 2^23 ms,
//! markets ≤ 2^13 — a leg's profit reaches 2^35, a cycle's profit 2^48 and its
//! time 2^36. A reduced weight is `λ.millis · w − λ.credits · t`, so it reaches
//! 2^36 · 2^35 = 2^71, and a Bellman-Ford distance accumulating those over a
//! path reaches 2^84. **All reduced-weight and ratio arithmetic is `i128`,
//! unconditionally.** There is no narrow fast path, not even for a three-cycle:
//! the bound above does not shrink with the cycle, only with the instance.

#![forbid(unsafe_code)]

pub mod bounded;
pub mod distinct;
#[cfg(test)]
mod fixture;
pub mod graph;
pub mod json;
pub mod model;
pub mod num;
pub mod ratio;
pub mod report;
pub mod rescore;
pub mod round;
pub mod sell;
pub mod single;
pub mod thread;
pub mod time;
pub mod topn;
pub mod view;
pub mod watch;
pub mod weight;

use crate::graph::{Pools, TradeGraph};
use crate::model::{Limits, Market, ShipConfig};
use crate::report::{Caveat, Route, RouteKind};
use crate::time::{Geometry, TimeModel};
use crate::watch::Watch;

/// Everything the optimiser found, one list per shape.
#[derive(Clone, Debug, Default)]
pub struct Solution {
    /// Best single hops, best first. No steady rate; see [`report::Caveat`].
    pub single: Vec<Route>,
    /// Best round trips, best first.
    pub round_trip: Vec<Route>,
    /// Best repeatable loops, best first. The head carries the strongest claim
    /// the search could establish; the rest are labelled for what they are.
    pub loops: Vec<Route>,
    /// What the graph build cost and skipped.
    pub stats: graph::BuildStats,
}

/// Which searches to run.
///
/// **A search nobody asked for is the most expensive thing this crate can do.**
/// The loop search is a Dinkelbach iteration over Bellman-Ford probes, minutes
/// at region scale, and `solve` used to run it on every call — so a plain
/// `edm route Sol --radius 100`, whose default shape is a round trip, spent
/// tens of minutes computing a loop ranking that was then discarded unprinted.
/// Reported from a live run, 2026-08-05.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub single: bool,
    pub round_trip: bool,
    pub loops: bool,
}

impl Wanted {
    /// Every shape. What the tests and the oracles compare.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            single: true,
            round_trip: true,
            loops: true,
        }
    }

    /// Just one — what a command with a `--shape` should ask for.
    #[must_use]
    pub const fn only(kind: RouteKind) -> Self {
        match kind {
            RouteKind::SingleHop => Self {
                single: true,
                round_trip: false,
                loops: false,
            },
            RouteKind::RoundTrip => Self {
                single: false,
                round_trip: true,
                loops: false,
            },
            RouteKind::Loop { .. } => Self {
                single: false,
                round_trip: false,
                loops: true,
            },
        }
    }
}

impl Default for Wanted {
    fn default() -> Self {
        Self::all()
    }
}

/// Runs the requested solvers over one set of markets.
///
/// The shape of the loop search is taken from [`Limits::max_stops`] and
/// [`Limits::min_distinct`]: unconstrained, capped in length, or required to
/// visit a minimum number of distinct stations.
///
/// `watch` is where the caller lends its clock and its ears; see [`watch`] for
/// why a wall-clock budget has to arrive from outside a crate this pure.
/// [`Watch::unlimited`] is the shape every exact claim in this crate's tests is
/// made under, and it is what the search did before there was a budget at all.
#[must_use]
pub fn solve(
    markets: &[Market],
    time: TimeModel,
    ship: &ShipConfig,
    limits: &Limits,
    wanted: Wanted,
    watch: Watch<'_>,
) -> Solution {
    let geometry = Geometry::new(markets, time);
    let pools = Pools::from_markets(markets);
    // Built only for the shapes that read it. `single::solve` works off the
    // pools alone, so a one-way search used to pay for the whole trade graph
    // and then never look at it — and the graph is the expensive half:
    // quadratic in the market count, measured at 127 s and a 4.1 GiB peak over
    // 5,049 markets. `Wanted::only` already exists to stop a search nobody
    // asked for; this stops the *build* nobody asked for.
    let graph = if wanted.round_trip || wanted.loops {
        Some(TradeGraph::build(&pools, &geometry, ship, limits, watch))
    } else {
        None
    };

    let mut single = if wanted.single {
        single::solve(&pools, &geometry, ship, limits)
    } else {
        Vec::new()
    };
    // The round trip is also the loop search's warm start — an achievable
    // rational lower bound that Dinkelbach begins from — so it is computed
    // whenever either is wanted, and is cheap either way (exact enumeration
    // over 2-cycles).
    let mut round_trip = match &graph {
        Some(graph) => round::solve(graph, &geometry, limits),
        None => Vec::new(),
    };
    // Only the requested shape is searched, which is also what keeps the
    // budget's withdrawal notice honest: an `Abandoned` event can now only come
    // from the search whose table is about to be printed. Running all three and
    // printing one meant a loop search's "reporting the best route it had,
    // unproved" could be announced over an exhaustively proved round trip.
    let mut loops = match (wanted.loops, &graph) {
        (true, Some(graph)) => match (limits.min_distinct, limits.max_stops) {
            (Some(k_min), _) => distinct::solve(graph, &geometry, limits, k_min, watch),
            (None, Some(k)) => bounded::solve(graph, &geometry, limits, k, watch),
            (None, None) => ratio::solve(graph, &geometry, limits, watch),
        },
        _ => Vec::new(),
    };
    if !wanted.round_trip {
        round_trip.clear();
    }

    // `BuildStats::default()` is the honest description of a build that did not
    // happen, not a zeroed one that did.
    let stats = graph.map_or_else(graph::BuildStats::default, |graph| graph.stats);
    // Read from the limits, not from `stats.profit_floor_applied`. The floor is
    // a property of the request — `single::solve` applies it too — and reading
    // it off the graph meant a one-way search silently stopped declaring a
    // filter it was still performing the moment the graph became optional.
    if limits.min_profit.0 > 0 {
        for route in single.iter_mut().chain(&mut round_trip).chain(&mut loops) {
            route.add_caveat(Caveat::EdgesBelowFloorDropped);
        }
    }
    let threading = thread::Threading {
        markets,
        ship,
        limits,
    };
    Solution {
        single: threading.rethread(single),
        round_trip: threading.rethread(round_trip),
        loops: threading.rethread(loops),
        stats,
    }
}

#[cfg(test)]
mod exactness {
    //! The one rule this crate cannot check from outside.
    //!
    //! The `route-exactness` and `purity` gates in `cargo xtask gates` own the
    //! source scan and the dependency tree. What is left here is the manifest
    //! itself, which is worth failing in the same commit as the mistake rather
    //! than at the next gate run.

    #[test]
    fn this_crate_takes_exactly_one_thing_from_the_port() {
        // The optimiser needs a point in galactic space and nothing else. Every
        // other dependency would be a way for something impure — a clock, a
        // socket, an entropy source — to reach a search whose whole value is
        // that it is reproducible. The workspace's `cargo tree` purity gate is
        // the authoritative check and should be extended to `-p edm-route`;
        // this is the cheap version that fails in the same commit as the
        // mistake.
        let manifest = include_str!("../Cargo.toml");
        let dependencies = manifest
            .split("[dependencies]")
            .nth(1)
            .expect("a dependencies section")
            .split('[')
            .next()
            .expect("a section body");
        let named: Vec<&str> = dependencies
            .lines()
            .map(str::trim_ascii)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        assert_eq!(named, vec!["edm-core.workspace = true"]);
    }
}

#[cfg(test)]
mod budget {
    //! The one rule that has to hold across every solver at once.

    use crate::fixture::{limits, market};
    use crate::model::{Limits, ShipConfig};
    use crate::num::{Credits, Tons};
    use crate::report::{Guarantee, HeuristicReason};
    use crate::time::TimeModel;
    use crate::watch::Watch;

    /// Three stations whose best loop is the triangle, plus a ship rich enough
    /// that the credit cap provably never binds — which is the condition
    /// [`crate::thread`] upgrades `OptimalForStartingCredits` to
    /// `ProvedOptimal` on. Every route in this instance is therefore one step
    /// away from the strongest claim the crate can make.
    fn instance() -> Vec<crate::model::Market> {
        vec![
            market(1, 0.0, &[(0, 100, 500)], &[(2, 400, 500)]),
            market(2, 1.0, &[(1, 100, 500), (2, 100, 500)], &[(0, 900, 500)]),
            market(3, 2.0, &[(2, 100, 500)], &[(1, 900, 500)]),
        ]
    }

    fn solved(limits: &Limits, watch: Watch<'_>) -> crate::Solution {
        let ship = ShipConfig {
            cargo: Tons(500),
            credits: Credits(1_000_000_000),
        };
        crate::solve(
            &instance(),
            TimeModel::default(),
            &ship,
            limits,
            crate::Wanted::all(),
            watch,
        )
    }

    #[test]
    fn an_unlimited_search_still_proves_this_instance_optimal() {
        // The control. Without it the test below could pass because nothing
        // was ever provable here.
        let solution = solved(&limits(), Watch::unlimited());
        assert_eq!(solution.loops[0].guarantee, Guarantee::ProvedOptimal);
        assert_eq!(solution.loops[0].legs.len(), 3);
    }

    #[test]
    fn an_exhausted_budget_never_yields_proved_optimal() {
        // Every loop shape, because the three solvers reach the claim by three
        // different routes and each one has to withdraw it separately.
        let spent = || true;
        let watch = Watch::unlimited().until(&spent);
        for (limits, answers) in [
            (limits(), true),
            (
                Limits {
                    max_stops: Some(3),
                    ..limits()
                },
                true,
            ),
            // A floor of three stops is a different question, and the warm
            // start is a two-cycle: it does not answer that question, so an
            // abandoned `min_distinct` search reports nothing rather than
            // reporting a route that breaks the constraint it was given.
            (
                Limits {
                    min_distinct: Some(3),
                    ..limits()
                },
                false,
            ),
        ] {
            let solution = solved(&limits, watch);
            let claims: Vec<Guarantee> =
                solution.loops.iter().map(|route| route.guarantee).collect();
            // The abandoned search's own answer is in the list and says so.
            assert_eq!(
                claims.contains(&Guarantee::Heuristic {
                    reason: HeuristicReason::SearchBudgetExhausted
                }),
                answers,
                "{limits:?}: {claims:?}"
            );
            // And nothing in the list claims optimality — not the head, and not
            // a runner-up that out-ranked it once threading re-sorted them.
            // `rethread` upgrades `OptimalForStartingCredits` to
            // `ProvedOptimal` whenever the credit cap cannot bind, which it
            // cannot here, so a solver that returned the wrong guarantee would
            // arrive at the strongest claim in the crate rather than at a
            // slightly-too-strong one.
            for guarantee in &claims {
                assert!(
                    !matches!(
                        guarantee,
                        Guarantee::ProvedOptimal | Guarantee::OptimalForStartingCredits
                    ),
                    "{limits:?}: {claims:?}"
                );
            }
            // A rate is unreadable except through `rate()`, so the claim has to
            // survive that accessor too.
            for route in &solution.loops {
                assert_eq!(route.rate().guarantee, route.guarantee);
            }
        }
    }
}
