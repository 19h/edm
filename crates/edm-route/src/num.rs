//! Exact integer money, time and mass, and the rational rate built from them.
//!
//! `edm-core` is floating-point everywhere because it mirrors JavaScript and
//! owes byte-level parity to it. This crate deliberately breaks with that.
//! Elite's ledger is integral — prices, stock, demand, cargo and the balance
//! are all whole numbers — and every exactness claim the optimiser makes is
//! only available over the integers. A rate compared as a floating-point
//! quotient would hide precisely the class of near-tie the search exists to
//! resolve.
//!
//! Time is quantised to one millisecond. That is three orders of magnitude
//! below the travel model's own error, and it buys the thing that matters:
//! every cycle's rate becomes an exact rational, which turns "binary search to
//! within epsilon" into "terminates on the exact optimum".
//!
//! **The rounding law.** Every quantisation rounds in the direction that
//! *lowers* the reported rate: units floor, time ceils, profit floors. So a
//! reported credits/hour is never an overstatement of what the model predicts.

use core::cmp::Ordering;
use core::iter::Sum;
use core::ops::{Add, AddAssign, Mul, Neg, Sub};

/// Milliseconds in an hour, the numerator of every credits-per-hour figure.
const MILLIS_PER_HOUR: i128 = 3_600_000;

/// A quantity of credits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Credits(pub i64);

impl Credits {
    /// No money.
    pub const ZERO: Self = Self(0);
}

impl Add for Credits {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for Credits {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sub for Credits {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl Neg for Credits {
    type Output = Self;
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

/// A unit margin times a number of units is a profit.
impl Mul<Tons> for Credits {
    type Output = Self;
    fn mul(self, rhs: Tons) -> Self {
        Self(self.0 * rhs.0)
    }
}

impl Sum for Credits {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

/// Wall-clock, quantised to one millisecond.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub i64);

impl Millis {
    /// No time.
    pub const ZERO: Self = Self(0);
}

impl Add for Millis {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl AddAssign for Millis {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl Sum for Millis {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

/// A quantity of cargo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tons(pub i64);

impl Tons {
    /// An empty hold.
    pub const ZERO: Self = Self(0);
}

impl Add for Tons {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub for Tons {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl Sum for Tons {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::ZERO, Add::add)
    }
}

/// Credits per millisecond, held as an exact rational.
///
/// This is the objective the loop solvers maximise. It is never evaluated as a
/// quotient during the search: two rates are compared by cross-multiplying into
/// [`i128`], which is exact at every magnitude this crate can reach, and the
/// only division that ever happens is the one that formats a number for a human.
///
/// The denominator is always strictly positive, so cross-multiplication
/// preserves the direction of the comparison without a sign analysis.
#[derive(Clone, Copy, Debug)]
pub struct Ratio {
    /// Credits earned.
    pub credits: i64,
    /// Wall-clock they were earned in. Strictly positive.
    pub millis: i64,
}

impl Ratio {
    /// Earning nothing, in one millisecond.
    pub const ZERO: Self = Self { credits: 0, millis: 1 };

    /// Builds a rate in lowest terms.
    ///
    /// Reduction is not cosmetic. Dinkelbach multiplies a rate's numerator and
    /// denominator into every reduced edge weight, and the branch-and-bound
    /// bound in `distinct` multiplies two rates' denominators together; leaving
    /// a common factor in would square the magnitude of intermediates for no
    /// reason.
    ///
    /// # Panics
    ///
    /// If `millis` is not strictly positive. A rate over zero time is not a
    /// slow route, it is a modelling error, and every construction site here
    /// has a positive floor on leg time.
    #[must_use]
    pub fn new(credits: Credits, millis: Millis) -> Self {
        assert!(millis.0 > 0, "a rate needs a strictly positive denominator");
        let divisor = gcd(credits.0.unsigned_abs(), millis.0.unsigned_abs());
        let divisor = if divisor == 0 { 1 } else { divisor as i64 };
        Self { credits: credits.0 / divisor, millis: millis.0 / divisor }
    }

    /// The rate as credits per hour, rounded **down**.
    ///
    /// Down, because the rounding law says every quantisation moves the
    /// reported rate toward the pessimistic side. `div_euclid` rather than `/`
    /// so a negative rate — which only a test constructs — also floors rather
    /// than truncating toward zero.
    #[must_use]
    pub fn credits_per_hour_floor(self) -> i64 {
        let scaled = i128::from(self.credits) * MILLIS_PER_HOUR;
        (scaled.div_euclid(i128::from(self.millis))) as i64
    }
}

impl PartialEq for Ratio {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ratio {}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ratio {
    /// Cross-multiplication in [`i128`].
    ///
    /// At instance bounds a numerator reaches 2^48 and a denominator 2^36, so
    /// the products here are at most 2^84 — far inside `i128` and far outside
    /// `i64`, which is why there is no narrow fast path.
    fn cmp(&self, other: &Self) -> Ordering {
        let lhs = i128::from(self.credits) * i128::from(other.millis);
        let rhs = i128::from(other.credits) * i128::from(self.millis);
        lhs.cmp(&rhs)
    }
}

/// Binary GCD would be faster; this runs once per rate and never in a loop.
const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::{Credits, Millis, Ratio, Tons};

    #[test]
    fn ratios_compare_exactly_where_a_quotient_would_tie() {
        // Two rates that differ in the 20th significant digit: a 64-bit float
        // has 53 bits of mantissa and would report these as equal.
        let a = Ratio { credits: 1_000_000_000_000_000_001, millis: 1_000_000_000_000_000_000 };
        let b = Ratio { credits: 1_000_000_000_000_000_002, millis: 1_000_000_000_000_000_000 };
        assert!(a < b);
    }

    #[test]
    fn equal_rates_in_different_terms_are_equal() {
        assert_eq!(Ratio { credits: 1, millis: 2 }, Ratio { credits: 50, millis: 100 });
        assert_eq!(Ratio::new(Credits(50), Millis(100)), Ratio { credits: 1, millis: 2 });
    }

    #[test]
    fn new_reduces_to_lowest_terms() {
        let r = Ratio::new(Credits(1_200_000), Millis(360_000));
        assert_eq!((r.credits, r.millis), (10, 3));
    }

    #[test]
    fn zero_credits_reduces_to_a_single_canonical_zero() {
        // gcd(0, n) is n, so every zero rate reduces to 0/1 whatever it was
        // earned over. That is the right answer: no two zero rates differ.
        assert_eq!(Ratio::new(Credits::ZERO, Millis(7)), Ratio::ZERO);
        assert_eq!(Ratio::new(Credits::ZERO, Millis(7)).millis, 1);
    }

    #[test]
    fn credits_per_hour_floors() {
        // 1000 credits in 3_600_001 ms is 999.999… per hour, and must print 999.
        let r = Ratio { credits: 1000, millis: 3_600_001 };
        assert_eq!(r.credits_per_hour_floor(), 999);
        let exact = Ratio { credits: 1000, millis: 3_600_000 };
        assert_eq!(exact.credits_per_hour_floor(), 1000);
    }

    #[test]
    fn a_margin_times_units_is_a_profit() {
        assert_eq!(Credits(1_500) * Tons(784), Credits(1_176_000));
    }
}
