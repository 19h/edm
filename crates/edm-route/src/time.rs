//! The travel-time model, and the only floating point the search is allowed.
//!
//! This module exists as a boundary. Supercruise time is `sqrt`-shaped and
//! distance is a Euclidean norm, so something here must be floating point; the
//! job of this file is to make sure that is the *last* place it appears. Every
//! function that leaves this module returns [`Millis`], and every quantisation
//! **ceils**, because the rounding law says a quantisation must move the
//! reported rate toward the pessimistic side and time is the denominator.
//!
//! The constants are the sibling project's calibrated model, each one a
//! parameter rather than a literal so a different ship is a flag and not a
//! rebuild.
//!
//! Distance is always Euclidean from `systemX/Y/Z`, never an API `distance`
//! field. Those are integer-rounded *and* radial from a reference system, so
//! the separation between two rows is neither the difference nor the sum of
//! their radii — the triangle inequality forbids deriving it from them at all.

use edm_core::domain::id64::Coordinates;

use crate::model::Market;
use crate::num::Millis;

/// Every constant in the travel model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeModel {
    /// Ship jump range in light years; sets the jump count for a leg.
    pub jump_range_ly: f64,
    /// Wall-clock per jump: charge, witchspace, and align on arrival.
    pub sec_per_jump: f64,
    /// Docking plus undocking.
    pub dock_sec: f64,
    /// Time spent in the commodity market screen.
    pub market_sec: f64,
    /// Supercruise constant term.
    pub sc_base_sec: f64,
    /// Supercruise coefficient on the square root of the light-second distance.
    pub sc_coef: f64,
}

impl Default for TimeModel {
    fn default() -> Self {
        Self {
            jump_range_ly: 30.0,
            sec_per_jump: 45.0,
            dock_sec: 60.0,
            market_sec: 30.0,
            sc_base_sec: 20.0,
            sc_coef: 1.2,
        }
    }
}

/// Straight-line separation of two systems, in light years.
#[must_use]
pub fn distance_ly(a: Coordinates, b: Coordinates) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    let dz = a.z - b.z;
    (dx * dx + dy * dy + dz * dz).sqrt()
}

impl TimeModel {
    /// Supercruise time to a station `ls` light seconds from the arrival star.
    ///
    /// Square-root shaped because supercruise accelerates with distance: about
    /// 32 s at 100 ls, 58 s at 1 Kls, 140 s at 10 Kls and roughly twenty
    /// minutes at 1 Mls. Stations beyond a million light seconds exist, and a
    /// linear model would be absurd there.
    fn supercruise_sec(self, ls: f64) -> f64 {
        if ls.is_nan() || ls <= 0.0 {
            return self.sc_base_sec;
        }
        self.sc_base_sec + self.sc_coef * ls.sqrt()
    }

    fn jump_sec(self, ly: f64) -> f64 {
        if ly.is_nan() || ly <= 0.0 {
            return 0.0;
        }
        (ly / self.jump_range_ly).ceil() * self.sec_per_jump
    }

    /// Time for one laden leg: jump out, supercruise in, dock, trade.
    ///
    /// **Where the approach is charged decides whether the whole ratio
    /// formulation is clean.** Arrival, docking and the market screen are
    /// charged at the leg's *destination*. A cycle's last destination is its
    /// first origin, so summing this over a cycle counts every station's
    /// approach exactly once — no startup term, no double count, and a loop's
    /// steady-state rate is `sum(w) / sum(t)` with nothing added.
    #[must_use]
    pub fn leg_millis(self, ly: f64, arrival_ls: f64) -> Millis {
        ceil_millis(
            self.jump_sec(ly) + self.supercruise_sec(arrival_ls) + self.dock_sec + self.market_sec,
        )
    }

    /// One-off cost of reaching and loading at the first station of a route.
    ///
    /// An open route pays this; a cycle flown repeatedly pays it once, on the
    /// first lap only, which is why every result carries both a cycle time and
    /// a first-lap time.
    #[must_use]
    pub fn startup_millis(self, arrival_ls: f64) -> Millis {
        ceil_millis(self.supercruise_sec(arrival_ls) + self.dock_sec + self.market_sec)
    }

    /// The shortest leg the model admits: no distance, station at the star.
    ///
    /// This is what makes a *rate* usable as a branch-and-bound bound. The
    /// denominator has a positive lower bound, so dividing a profit bound by it
    /// can never underestimate the achievable rate, and so can never prune a
    /// winner.
    #[must_use]
    pub fn min_leg_millis(self) -> Millis {
        self.leg_millis(0.0, 0.0)
    }

    /// The shortest wall-clock an open route of `legs` legs can take.
    #[must_use]
    pub fn min_lap_millis(self, legs: usize) -> Millis {
        let per_leg = self.min_leg_millis().0;
        let startup = self.startup_millis(0.0).0;
        Millis(startup + per_leg * legs as i64)
    }
}

/// Quantises seconds to milliseconds, rounding up, with a floor of one.
///
/// The floor is not defensive padding: a zero denominator is not a fast route
/// but an undefined rate, and a caller who sets every constant to zero should
/// get an absurdly high finite number rather than a panic in `Ratio::new`.
fn ceil_millis(sec: f64) -> Millis {
    if sec.is_nan() || sec <= 0.0 {
        return Millis(1);
    }
    let ms = (sec * 1000.0).ceil();
    // 2^53 keeps the value inside the range where every integer is exactly
    // representable, so the conversion below is not lossy.
    if ms >= 9_007_199_254_740_992.0 {
        return Millis(9_007_199_254_740_992);
    }
    Millis(ms as i64)
}

/// Distances and times over a fixed set of markets.
///
/// Bundled so the solvers can build a route without ever naming a floating
/// point value: they hand indices to a route constructor and get integers back.
#[derive(Clone, Copy, Debug)]
pub struct Geometry<'a> {
    /// The markets, indexed by node.
    pub markets: &'a [Market],
    /// The model in force.
    pub time: TimeModel,
}

impl<'a> Geometry<'a> {
    /// Binds a model to a market set.
    #[must_use]
    pub fn new(markets: &'a [Market], time: TimeModel) -> Self {
        Self { markets, time }
    }

    /// Separation of two markets' systems, in light years.
    #[must_use]
    pub fn leg_ly(&self, from: u32, to: u32) -> f64 {
        distance_ly(self.markets[from as usize].coords, self.markets[to as usize].coords)
    }

    /// Wall-clock for the laden leg `from` to `to`.
    #[must_use]
    pub fn leg_millis(&self, from: u32, to: u32) -> Millis {
        self.time.leg_millis(self.leg_ly(from, to), self.markets[to as usize].arrival_ls)
    }

    /// Wall-clock to reach and load at `at` before the first leg.
    #[must_use]
    pub fn startup_millis(&self, at: u32) -> Millis {
        self.time.startup_millis(self.markets[at as usize].arrival_ls)
    }
}

#[cfg(test)]
mod tests {
    use super::{TimeModel, distance_ly};
    use edm_core::domain::id64::Coordinates;

    fn at(x: f64, y: f64, z: f64) -> Coordinates {
        Coordinates { x, y, z }
    }

    #[test]
    fn distance_is_euclidean() {
        assert!((distance_ly(at(0.0, 0.0, 0.0), at(3.0, 4.0, 0.0)) - 5.0).abs() < 1e-12);
    }

    #[test]
    fn time_is_symmetric_exactly_when_the_star_distances_match() {
        let m = TimeModel::default();
        let ly = 12.0;
        assert_eq!(m.leg_millis(ly, 500.0), m.leg_millis(ly, 500.0));
        assert_ne!(m.leg_millis(ly, 500.0), m.leg_millis(ly, 100_000.0));
    }

    #[test]
    fn quantisation_ceils() {
        // 20 + 60 + 30 = 110 s exactly, plus a supercruise term that is not a
        // whole number of milliseconds: 1.2 * sqrt(2) = 1.697056… s.
        let m = TimeModel::default();
        let with = m.leg_millis(0.0, 2.0);
        let without = m.leg_millis(0.0, 0.0);
        assert_eq!(without, crate::num::Millis(110_000));
        assert_eq!(with, crate::num::Millis(111_698));
    }

    #[test]
    fn a_zero_length_jump_costs_nothing_but_the_approach() {
        let m = TimeModel::default();
        assert_eq!(m.leg_millis(0.0, 0.0), m.min_leg_millis());
        // One light year still costs a full jump: the count ceils.
        assert_eq!(m.leg_millis(1.0, 0.0), crate::num::Millis(155_000));
        assert_eq!(m.leg_millis(30.0, 0.0), crate::num::Millis(155_000));
        assert_eq!(m.leg_millis(30.5, 0.0), crate::num::Millis(200_000));
    }

    #[test]
    fn every_time_is_strictly_positive_even_with_a_zeroed_model() {
        let m = TimeModel {
            jump_range_ly: 30.0,
            sec_per_jump: 0.0,
            dock_sec: 0.0,
            market_sec: 0.0,
            sc_base_sec: 0.0,
            sc_coef: 0.0,
        };
        assert_eq!(m.leg_millis(0.0, 0.0).0, 1);
        assert_eq!(m.startup_millis(0.0).0, 1);
    }
}
