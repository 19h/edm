//! Provably optimal trade routes over a set of live market reads.
//!
//! This crate is pure: it takes markets in and returns data out. It never
//! renders, never formats, and holds no strings the caller did not give it.
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
pub mod model;
pub mod num;
pub mod ratio;
pub mod report;
pub mod round;
pub mod single;
pub mod thread;
pub mod time;
pub mod topn;
pub mod weight;

use crate::graph::{Pools, TradeGraph};
use crate::model::{Limits, Market, ShipConfig};
use crate::report::{Caveat, Route};
use crate::time::{Geometry, TimeModel};

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

/// Runs every solver over one set of markets.
///
/// The shape of the loop search is taken from [`Limits::max_stops`] and
/// [`Limits::min_distinct`]: unconstrained, capped in length, or required to
/// visit a minimum number of distinct stations.
#[must_use]
pub fn solve(
    markets: &[Market],
    time: TimeModel,
    ship: &ShipConfig,
    limits: &Limits,
) -> Solution {
    let geometry = Geometry::new(markets, time);
    let pools = Pools::from_markets(markets);
    let graph = TradeGraph::build(&pools, &geometry, ship, limits);

    let mut single = single::solve(&pools, &geometry, ship, limits);
    let mut round_trip = round::solve(&graph, &geometry, limits);
    let mut loops = match (limits.min_distinct, limits.max_stops) {
        (Some(k_min), _) => distinct::solve(&graph, &geometry, limits, k_min),
        (None, Some(k)) => bounded::solve(&graph, &geometry, limits, k),
        (None, None) => ratio::solve(&graph, &geometry, limits),
    };

    if graph.stats.profit_floor_applied {
        for route in single.iter_mut().chain(&mut round_trip).chain(&mut loops) {
            route.add_caveat(Caveat::EdgesBelowFloorDropped);
        }
    }

    let stats = graph.stats;
    let threading = thread::Threading { markets, ship, limits };
    Solution {
        single: threading.rethread(single),
        round_trip: threading.rethread(round_trip),
        loops: threading.rethread(loops),
        stats,
    }
}

#[cfg(test)]
mod exactness {
    //! The exactness rule, enforced against this crate's own source.
    //!
    //! Every claim in this crate rests on the arithmetic being exact, and the
    //! way that stops being true is not a decision but a drift: one convenient
    //! `as f64` inside a comparison and the total order quietly becomes a
    //! partial one. So the rule is mechanical.
    //!
    //! This belongs in `cargo xtask gates` alongside the purity and
    //! parity-isolation checks. It lives here because that file has another
    //! owner while this crate is being written; moving it is a two-line change
    //! and the check itself does not need to move with it.

    /// The files where the search happens and floating point may not appear.
    const SOLVING_PATH: [(&str, &str); 7] = [
        ("num.rs", include_str!("num.rs")),
        ("weight.rs", include_str!("weight.rs")),
        ("single.rs", include_str!("single.rs")),
        ("round.rs", include_str!("round.rs")),
        ("ratio.rs", include_str!("ratio.rs")),
        ("bounded.rs", include_str!("bounded.rs")),
        ("distinct.rs", include_str!("distinct.rs")),
    ];

    #[test]
    fn the_solving_path_contains_no_floating_point() {
        for (name, source) in SOLVING_PATH {
            assert!(
                !source.contains("f64") && !source.contains("f32"),
                "{name} names a floating-point type; the exactness claims in this crate \
                 are only available over the integers"
            );
        }
    }

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

    #[test]
    fn the_solving_path_does_not_divide_its_way_out_of_a_ratio() {
        // A rate must never be collapsed to a quotient for comparison. The
        // sanctioned division is `Ratio::credits_per_hour_floor`, which is a
        // display path and lives in `num.rs`.
        for (name, source) in SOLVING_PATH {
            if name == "num.rs" {
                continue;
            }
            assert!(
                !source.contains("credits_per_hour_floor"),
                "{name} formats a rate; formatting belongs to the caller"
            );
        }
    }
}
