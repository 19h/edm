//! What a trade request is made of.
//!
//! Only the decisions live here — pricing, the black-market rule, the three
//! clamps and the resulting stack size. The *order* in which the command line
//! is consulted is observable (a bad `--cargo` surfaces only after the stock
//! clamp has had its chance to fail first), so that ordering lives with the
//! argument accessors rather than here. R94.
//!
//! Everything is `f64`. That is not laziness: the single-trade path never
//! floors, and `heldQuantity` never floors, so `qty`, `unitPrice` and
//! `finalQty` can all legitimately reach the wire fractional. R95.

use crate::js;

use super::Commodity;

/// Which direction a trade goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Buy,
    Sell,
}

impl Kind {
    /// The spelling used in error messages and progress lines.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }

    /// `--type` accepts exactly these two, lowercased.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "buy" => Ok(Self::Buy),
            "sell" => Ok(Self::Sell),
            other => Err(format!("--type must be buy or sell, not \"{other}\"")),
        }
    }
}

/// A fully resolved trade request, ready to become an envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct TradePlan {
    /// Deliberately a string, not a number.
    ///
    /// `trade` never parses `--market-id`; it passes the flag straight through
    /// to the envelope, so `0004306502403` reaches the wire with its leading
    /// zeros while `market --market-id 0004306502403` would send
    /// `4306502403`. R53.
    pub market_id: String,
    pub kind: Kind,
    pub commodity_id: f64,
    pub commodity_name: String,
    pub black_market: bool,
    pub stolen: bool,
    pub unit_price: f64,
    pub qty: f64,
    pub final_qty: f64,
}

/// Free room in the hold.
///
/// A newtype over `f64` rather than an `Option`, because unbounded capacity is
/// `+Infinity` and `Math.min(free, available)` with an infinite `free` *is*
/// `available`. An enum would force a match arm at each of the five clamp
/// sites, and every one of them would be a chance to write the `None` case as
/// "no limit" in a way that silently overflows the hold.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Space(f64);

impl Space {
    /// No `--cargo` was given, so nothing constrains the purchase.
    pub const UNBOUNDED: Self = Self(f64::INFINITY);

    #[must_use]
    pub fn of(cargo: Option<f64>, used: f64) -> Self {
        cargo.map_or(Self::UNBOUNDED, |capacity| Self(capacity - used))
    }

    /// `free <= 0` — the test the fill loop stops on.
    #[must_use]
    pub fn exhausted(self) -> bool {
        self.0 <= 0.0
    }

    #[must_use]
    pub fn is_bounded(self) -> bool {
        self.0.is_finite()
    }

    #[must_use]
    pub fn units(self) -> f64 {
        self.0
    }
}

/// `deriveBlackMarket` (ts:1769) — illegal goods and anything stolen only move
/// through the black market, unless the flag says otherwise.
#[must_use]
pub fn derive_black_market(
    commodity: Option<&Commodity<'_>>,
    stolen: bool,
    explicit: Option<bool>,
) -> bool {
    explicit.unwrap_or_else(|| stolen || commodity.is_some_and(|c| c.illegal))
}

/// `derivePrice` (ts:1773).
///
/// A market that does not sell something prices it at zero, and sending a
/// zero-priced buy is rejected, so it fails here with a message naming the
/// commodity instead.
pub fn derive_price(
    commodity: &Commodity<'_>,
    kind: Kind,
    black_market: bool,
) -> Result<f64, String> {
    match kind {
        Kind::Buy => {
            if commodity.buy_price == 0.0 {
                Err(format!("{} is not sold at this market (buyPrice 0)", commodity.name))
            } else {
                Ok(commodity.buy_price)
            }
        }
        // A fence pays differently, and which price applies depends on the
        // black-market flag that was just derived.
        Kind::Sell => Ok(if black_market { commodity.fence_price } else { commodity.sell_price }),
    }
}

/// `resultingStack` (ts:1786) — what `finalQty` must be.
///
/// `finalQty` is the size the commodity's stack *ends up* at, not a copy of
/// `qty`. The game's own logs show `qty=13 finalQty=130` when 117 units were
/// already aboard, and sending `qty` in that field is rejected with HTTP 402.
/// Only buys appear in captured traffic; the sell direction is inferred.
#[must_use]
pub fn resulting_stack(held: f64, qty: f64, kind: Kind) -> f64 {
    match kind {
        Kind::Buy => held + qty,
        Kind::Sell => js::js_max(0.0, held - qty),
    }
}

/// How much of this commodity the market can supply, or the hold can give up.
#[must_use]
pub fn available(commodity: &Commodity<'_>, held: f64, kind: Kind) -> f64 {
    match kind {
        Kind::Buy => commodity.stock,
        Kind::Sell => held,
    }
}

/// The label a clamp message uses for whatever ran out.
#[must_use]
pub fn availability_label(kind: Kind, stolen: bool) -> &'static str {
    match (kind, stolen) {
        (Kind::Buy, _) => "stock",
        (Kind::Sell, true) => "stolen holdings",
        (Kind::Sell, false) => "holdings",
    }
}

/// Why a commodity could not be traded this round.
///
/// The order is the order the TypeScript tests in, and it is observable in the
/// skip messages: an empty market reports "no stock" even when the balance
/// would also have been too low. R91.
#[must_use]
pub fn zero_quantity_reason(
    kind: Kind,
    available: f64,
    credits: Option<f64>,
    unit_price: f64,
) -> &'static str {
    if available == 0.0 {
        match kind {
            Kind::Buy => "no stock",
            Kind::Sell => "nothing held",
        }
    } else if credits.is_some_and(|c| c < unit_price) {
        "not enough credits"
    } else {
        "no cargo space"
    }
}

/// The batch loop's per-commodity sizing (ts:2156).
///
/// Four clamps in a fixed order — the requested amount, what is actually
/// available, the room left in the hold, and what the balance can cover — then
/// a floor. Each is `Math.min`, which propagates NaN and treats `+Infinity` as
/// "no limit"; using Rust's `f64::min` here would differ on NaN.
#[must_use]
pub fn plan_quantity(
    kind: Kind,
    fill: bool,
    per_item_qty: Option<f64>,
    available: f64,
    free: Space,
    credits: Option<f64>,
    unit_price: f64,
) -> f64 {
    let mut qty = if fill {
        js::js_min(free.units(), available)
    } else {
        js::js_min(per_item_qty.unwrap_or(f64::INFINITY), available)
    };

    if kind == Kind::Buy {
        if free.is_bounded() {
            qty = js::js_min(qty, free.units());
        }
        // Never queue a purchase the balance cannot cover, once it is known.
        // The `unit_price > 0` guard keeps a free commodity from dividing by
        // zero into an infinite quantity.
        if let Some(credits) = credits
            && unit_price > 0.0
        {
            qty = js::js_min(qty, (credits / unit_price).floor());
        }
    }

    js::js_max(0.0, qty.floor())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commodity(buy: f64, sell: f64, fence: f64, stock: f64) -> Commodity<'static> {
        Commodity {
            id: 1.0,
            name: "Silver",
            category: "Metals",
            stock,
            stock_bracket: 0.0,
            buy_price: buy,
            sell_price: sell,
            fence_price: fence,
            demand: 0.0,
            demand_bracket: 0.0,
            mean_price: 0.0,
            consumer: false,
            producer: false,
            rare: false,
            illegal: false,
        }
    }

    #[test]
    fn a_fence_pays_a_different_price() {
        let c = commodity(500.0, 480.0, 300.0, 10.0);
        assert_eq!(derive_price(&c, Kind::Sell, false).unwrap(), 480.0);
        assert_eq!(derive_price(&c, Kind::Sell, true).unwrap(), 300.0);
        assert_eq!(derive_price(&c, Kind::Buy, false).unwrap(), 500.0);
    }

    #[test]
    fn an_unsold_commodity_names_itself() {
        let c = commodity(0.0, 480.0, 0.0, 10.0);
        assert_eq!(
            derive_price(&c, Kind::Buy, false).unwrap_err(),
            "Silver is not sold at this market (buyPrice 0)"
        );
    }

    /// Stolen goods and illegal goods route through the black market unless the
    /// flag overrides it — including overriding it *off*.
    #[test]
    fn black_market_is_inferred_but_overridable() {
        let mut illegal = commodity(500.0, 480.0, 300.0, 10.0);
        illegal.illegal = true;
        assert!(derive_black_market(Some(&illegal), false, None));
        assert!(derive_black_market(None, true, None));
        assert!(!derive_black_market(None, false, None));
        assert!(!derive_black_market(Some(&illegal), true, Some(false)));
    }

    /// `finalQty` is where the stack ends up, and a sale cannot take it below
    /// zero.
    #[test]
    fn final_qty_is_the_resulting_stack() {
        assert_eq!(resulting_stack(117.0, 13.0, Kind::Buy), 130.0);
        assert_eq!(resulting_stack(10.0, 4.0, Kind::Sell), 6.0);
        assert_eq!(resulting_stack(3.0, 10.0, Kind::Sell), 0.0);
    }

    /// An unbounded hold must not become a zero-sized one.
    #[test]
    fn unbounded_space_does_not_clamp() {
        let qty = plan_quantity(
            Kind::Buy,
            false,
            Some(50.0),
            1000.0,
            Space::UNBOUNDED,
            None,
            10.0,
        );
        assert_eq!(qty, 50.0);
    }

    /// The affordability clamp is the one that spends real money if it is wrong.
    #[test]
    fn a_purchase_is_clamped_to_the_balance() {
        let qty = plan_quantity(
            Kind::Buy,
            false,
            Some(100.0),
            1000.0,
            Space::of(Some(500.0), 0.0),
            Some(1050.0),
            100.0,
        );
        assert_eq!(qty, 10.0, "1050 credits at 100 each buys ten, not eleven");
    }

    /// A present-but-null `credits` reads as zero, and zero credits must buy
    /// nothing rather than everything. R18.
    #[test]
    fn zero_credits_buys_nothing() {
        let qty = plan_quantity(
            Kind::Buy,
            true,
            None,
            1000.0,
            Space::of(Some(500.0), 0.0),
            Some(0.0),
            100.0,
        );
        assert_eq!(qty, 0.0);
    }

    /// Selling is never constrained by credits or by cargo space.
    #[test]
    fn selling_ignores_the_balance() {
        let qty =
            plan_quantity(Kind::Sell, false, Some(40.0), 25.0, Space::of(Some(0.0), 0.0), Some(0.0), 10.0);
        assert_eq!(qty, 25.0);
    }
}
