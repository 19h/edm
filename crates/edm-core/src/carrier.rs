//! Fleet-carrier docking access, from Frontier \[C37\].
//!
//! Supersedes the Spansh reader C36 shipped. The defect that ended that design
//! is the one it was built to fix, arriving by a different road: a
//! crowd-sourced index is stale **by construction**, because the only thing
//! that ever republishes a carrier's access is somebody docking there and
//! opening the market screen. A carrier whose owner closed the door yesterday
//! reads as open until the next visitor discovers otherwise, and the commander
//! who discovers it is the one who flew there.
//!
//! `2.0/elite/fleetcarrier/info` answers `docking.accessLevel` live, and
//! answers it for a carrier the commander has never seen. Two measurements make
//! it affordable, and both were taken against the live endpoint rather than
//! reasoned about:
//!
//! - **The id is arithmetic.** `market_id = fleetCarrierId * 256 +
//!   3_290_400_000`, exact over all 157 id pairs in the 2026-08-26 capture and
//!   confirmed live. C36 priced this design partly on needing
//!   `fleetcarrier/system` first to map one id to the other; that request does
//!   not exist. A market id that is not congruent is not a carrier's, so the
//!   arithmetic is also its own filter and costs nothing to apply.
//! - **The reply echoes both ids back.** So every answer can be checked against
//!   the question, which `market/list` cannot offer — its payload carries no id
//!   at all.
//!
//! What is left is one metered request per carrier, which is why the caller
//! makes the probes a **priced phase** with a gate of its own rather than
//! slipping them in ahead of the plan.
//!
//! **Three states, still.** [`Access::Unknown`] survives the change and means
//! something sharper than it did: not "nobody has reported this door" — a third
//! of carriers, under Spansh — but "*this run* did not get an answer for this
//! door". Every instance has a cause the run can name.

use crate::js;
use crate::js::json::JsValue;

/// The offset between a carrier's market id and the `fleetCarrierId` that
/// `/info` takes.
pub const CARRIER_MARKET_BASE: f64 = 3_290_400_000.0;
/// Carrier market ids advance in steps of this size.
pub const CARRIER_MARKET_STRIDE: f64 = 256.0;

/// The `fleetCarrierId` for a market id, or `None` when it is not a carrier's.
///
/// Verified over 157 id pairs in the 2026-08-26 capture — 53 `/info` replies
/// and 104 `/system` records — with zero exceptions, and confirmed live against
/// `3711014400` → `1643025` → `T1N-W2F`.
///
/// The `None` case is not an error to swallow. Ardent's `stationType` is what
/// decides a station is a carrier; this arithmetic is Frontier's own opinion of
/// the same question, and a disagreement between them is exactly the kind of
/// silent wrongness this feature exists to surface. The caller reports it.
#[must_use]
pub fn carrier_id(market_id: f64) -> Option<f64> {
    if !market_id.is_finite() {
        return None;
    }
    let raw = market_id - CARRIER_MARKET_BASE;
    // Strict: the base itself is not a carrier, and neither is anything below
    // it — the whole pre-carrier market id space lives down there.
    if raw <= 0.0 || raw % CARRIER_MARKET_STRIDE != 0.0 {
        return None;
    }
    let id = raw / CARRIER_MARKET_STRIDE;
    js::safe_int(id).then_some(id)
}

/// The market id a `fleetCarrierId` belongs to.
///
/// The inverse is used, not merely documented: every reply is checked against
/// it, so the constant is exercised in both directions on live data.
#[must_use]
pub fn market_id(carrier_id: f64) -> f64 {
    carrier_id * CARRIER_MARKET_STRIDE + CARRIER_MARKET_BASE
}

/// What Frontier says a carrier's door does.
///
/// Lowercase single tokens on the wire — `squadronfriends`, not Spansh's
/// `Squadron Friends`. Matched exactly: a token this program does not know is
/// [`Access::Unknown`] and a warning, never a guess in either direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessLevel {
    All,
    Friends,
    Squadron,
    SquadronFriends,
    /// Nobody but the owner.
    ///
    /// Unobserved in 53 replies, and modelled anyway because the journal enum
    /// has it. It must never collapse into [`Access::Unknown`]: `none` is the
    /// strictest carrier in the game and unknown is the least informative state
    /// there is, so conflating them would keep the one carrier that admits
    /// nobody at all.
    None,
}

impl AccessLevel {
    /// Parse one wire token.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "all" => Some(Self::All),
            "friends" => Some(Self::Friends),
            "squadron" => Some(Self::Squadron),
            "squadronfriends" => Some(Self::SquadronFriends),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Friends => "friends",
            Self::Squadron => "squadron",
            Self::SquadronFriends => "squadronfriends",
            Self::None => "none",
        }
    }
}

/// The `docking` object, as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Docking {
    pub level: AccessLevel,
    /// Whether a commander carrying notoriety may dock.
    ///
    /// A separate gate, and not a redundant one: **11 of the 31 carriers
    /// Frontier itself calls `all` set this false.** For a notorious commander
    /// `all` is therefore wrong 35% of the time — the same wasted flight, and a
    /// failure mode that Spansh, EDDN and the journal's `CarrierDockingAccess`
    /// all miss identically, because none of them carries the field.
    pub notorious_ok: bool,
}

/// What a carrier's door means for *this* commander.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Open,
    Restricted,
    /// This run did not get an answer.
    Unknown,
}

/// Why a carrier is closed, for the plan's ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    /// The published access does not admit the general public.
    Level,
    /// The door is open, but not to a commander carrying notoriety.
    Notoriety,
}

/// The verdict for one carrier, given this commander's notoriety.
///
/// Derived at read time and never cached: the cache stores what Frontier said
/// about the *carrier*, and folding in a fact about the *commander* would mean
/// that clearing notoriety left a stale verdict on disk for the rest of the
/// TTL.
#[must_use]
pub fn verdict(docking: Docking, notoriety: f64) -> (Access, Option<Closed>) {
    if notoriety > 0.0 && !docking.notorious_ok {
        return (Access::Restricted, Some(Closed::Notoriety));
    }
    match docking.level {
        AccessLevel::All => (Access::Open, None),
        AccessLevel::Friends
        | AccessLevel::Squadron
        | AccessLevel::SquadronFriends
        | AccessLevel::None => (Access::Restricted, Some(Closed::Level)),
    }
}

/// Which carriers a run will keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Policy {
    /// Every carrier. Probes nothing.
    Any,
    /// Drop the provably closed; keep the ones this run could not read.
    #[default]
    Open,
    /// Keep only carriers Frontier affirmatively confirmed open this run.
    ///
    /// The claim it buys is new: under Spansh `proven` meant "somebody once
    /// reported this open"; here it means *every carrier in this plan was
    /// confirmed dockable by Frontier minutes ago*.
    Proven,
}

impl Policy {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Open => "open",
            Self::Proven => "proven",
        }
    }

    /// Every accepted spelling, for the unknown-value error.
    pub const NAMES: [&'static str; 3] = ["any", "open", "proven"];

    /// Parse one `--carrier-access` value, case- and separator-insensitively.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let folded: String = raw
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-' && *c != '_')
            .collect::<String>()
            .to_lowercase();
        match folded.as_str() {
            "any" => Some(Self::Any),
            "open" => Some(Self::Open),
            "proven" => Some(Self::Proven),
            _ => None,
        }
    }

    /// Whether this policy costs anything to enforce.
    #[must_use]
    pub const fn filters(self) -> bool {
        !matches!(self, Self::Any)
    }

    /// Whether a carrier with this access survives.
    #[must_use]
    pub const fn admits(self, access: Access) -> bool {
        match self {
            Self::Any => true,
            Self::Open => !matches!(access, Access::Restricted),
            Self::Proven => matches!(access, Access::Open),
        }
    }
}

/// Why a reply could not be turned into a verdict.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// No `fleetCarrier`, or no `docking` object inside it.
    ///
    /// Read through an explicit `Option` rather than a string accessor that
    /// answers `""` for a missing key: an absent field and an unrecognised
    /// token are different facts with different fixes.
    Shape,
    /// A token this program has never seen. Reported, never guessed at.
    UnknownLevel(String),
    /// The reply is about a different carrier than the one asked about.
    Identity { expected: f64, echoed: Option<f64> },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape => write!(f, "the reply carried no docking object"),
            Self::UnknownLevel(token) => {
                write!(f, "it published an access level this program does not know: {token:?}")
            }
            Self::Identity { expected, echoed } => write!(
                f,
                "it answered for market {} when asked about {}",
                echoed.map_or_else(|| "nothing".to_owned(), js::js_number),
                js::js_number(*expected)
            ),
        }
    }
}

/// What one `/info` reply says, checked against the carrier it was asked about.
///
/// Only the fields that are used are read. The reply also carries that
/// commander's `finance`, `inventory` and `crewRoster` — around 11 KB of
/// somebody else's business per request — and none of it is parsed, kept or
/// written anywhere.
pub fn parse_info(document: &JsValue, expected_market_id: f64) -> Result<Owned, Refusal> {
    let carrier = document
        .as_record()
        .and_then(|root| root.get("fleetCarrier"))
        .and_then(JsValue::as_record)
        .ok_or(Refusal::Shape)?;

    // Identity before content: a reply about the wrong carrier is not a reply
    // whose contents are worth reading.
    let echoed = carrier.get("market_id").and_then(JsValue::as_f64);
    if echoed != Some(expected_market_id) {
        return Err(Refusal::Identity {
            expected: expected_market_id,
            echoed,
        });
    }

    let docking = carrier
        .get("docking")
        .and_then(JsValue::as_record)
        .ok_or(Refusal::Shape)?;
    let raw = docking
        .get("accessLevel")
        .and_then(JsValue::as_str)
        .ok_or(Refusal::Shape)?;
    let level = AccessLevel::parse(raw).ok_or_else(|| Refusal::UnknownLevel(raw.to_owned()))?;
    // Absent reads as permissive, which is the direction that keeps a carrier
    // rather than dropping one on a field Frontier stopped sending.
    let notorious_ok = docking
        .get("notoriousAccess")
        .and_then(|value| match value {
            JsValue::Bool(flag) => Some(*flag),
            _ => None,
        })
        .unwrap_or(true);

    let owner = carrier.get("owner").and_then(JsValue::as_record);
    Ok(Owned {
        docking: Docking {
            level,
            notorious_ok,
        },
        // Carried for a squadron or friends match that is not implemented yet
        // \[C37\]. Eight bytes of cache each, and the alternative is re-probing
        // every carrier on the day it lands.
        owner_squadron_id: owner
            .and_then(|o| o.get("squadron_id"))
            .and_then(JsValue::as_f64),
        owner_user_id: owner
            .and_then(|o| o.get("user_id"))
            .and_then(JsValue::as_f64),
    })
}

/// One carrier's answer, as stored.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Owned {
    pub docking: Docking,
    /// `0` means "no squadron" and must never be matched against itself.
    pub owner_squadron_id: Option<f64>,
    pub owner_user_id: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real pairs, both directions. The constant is load-bearing: get it wrong
    /// and every probe asks about a different carrier than the one being
    /// filtered, and the identity guard turns the whole run into refusals.
    #[test]
    fn the_carrier_id_arithmetic_round_trips_on_measured_pairs() {
        for (market, carrier) in [
            (3_701_577_984.0, 1_606_164.0),
            (3_703_823_104.0, 1_614_934.0),
            (3_702_563_840.0, 1_610_015.0),
            (3_706_370_816.0, 1_624_886.0),
            (3_712_197_888.0, 1_647_648.0),
            (3_711_324_160.0, 1_644_235.0),
            // Confirmed live: T1N-W2F.
            (3_711_014_400.0, 1_643_025.0),
        ] {
            assert_eq!(carrier_id(market), Some(carrier), "{market} -> id");
            assert_eq!(market_id(carrier), market, "{carrier} -> market");
        }
    }

    #[test]
    fn a_market_id_below_the_base_is_not_a_carrier() {
        // Jaques Station, and an ordinary starport from a real sweep.
        assert_eq!(carrier_id(128_667_761.0), None);
        assert_eq!(carrier_id(128_016_384.0), None);
    }

    #[test]
    fn a_market_id_exactly_at_the_base_is_not_a_carrier() {
        assert_eq!(carrier_id(CARRIER_MARKET_BASE), None);
    }

    #[test]
    fn a_market_id_off_the_stride_is_not_a_carrier() {
        assert_eq!(carrier_id(CARRIER_MARKET_BASE + 1.0), None);
        assert_eq!(carrier_id(CARRIER_MARKET_BASE + 255.0), None);
        // Measured: a real non-carrier market id well above the base.
        assert_eq!(carrier_id(4_306_502_403.0), None);
    }

    #[test]
    fn a_non_finite_or_unsafe_market_id_is_not_a_carrier() {
        assert_eq!(carrier_id(f64::NAN), None);
        assert_eq!(carrier_id(f64::INFINITY), None);
        assert_eq!(carrier_id(f64::NEG_INFINITY), None);
        // Large enough that the quotient is past `Number.MAX_SAFE_INTEGER`, so
        // it would not survive a round trip through the wire's decimal text.
        assert_eq!(carrier_id(1e300), None);
    }

    /// The base is itself a whole number of strides
    /// (`3_290_400_000 = 256 * 12_853_125`), so congruence to the base and
    /// congruence to zero are the same test. Worth pinning: it is why a
    /// stride-aligned market id far above the base still yields a usable id
    /// rather than a fractional one.
    #[test]
    fn the_base_is_a_whole_number_of_strides() {
        assert_eq!(CARRIER_MARKET_BASE % CARRIER_MARKET_STRIDE, 0.0);
    }

    #[test]
    fn the_four_observed_tokens_and_none_all_parse() {
        for token in ["all", "friends", "squadron", "squadronfriends", "none"] {
            let level = AccessLevel::parse(token).expect(token);
            assert_eq!(level.name(), token);
        }
    }

    /// Frontier's spellings are not Spansh's, and the wrong one matches
    /// nothing — which is spelled the same way as "not restricted".
    #[test]
    fn spansh_spellings_do_not_parse() {
        assert_eq!(AccessLevel::parse("Squadron Friends"), None);
        assert_eq!(AccessLevel::parse("All"), None);
        assert_eq!(AccessLevel::parse(""), None);
    }

    /// The carrier that admits nobody must not be read as the carrier nobody
    /// has reported on.
    #[test]
    fn none_is_restricted_and_never_unknown() {
        let (access, why) = verdict(
            Docking {
                level: AccessLevel::None,
                notorious_ok: true,
            },
            0.0,
        );
        assert_eq!(access, Access::Restricted);
        assert_eq!(why, Some(Closed::Level));
    }

    #[test]
    fn the_verdict_truth_table() {
        let open = Docking {
            level: AccessLevel::All,
            notorious_ok: true,
        };
        let open_to_the_clean = Docking {
            level: AccessLevel::All,
            notorious_ok: false,
        };
        assert_eq!(verdict(open, 0.0), (Access::Open, None));
        assert_eq!(verdict(open, 3.0), (Access::Open, None));
        assert_eq!(verdict(open_to_the_clean, 0.0), (Access::Open, None));
        assert_eq!(
            verdict(open_to_the_clean, 1.0),
            (Access::Restricted, Some(Closed::Notoriety))
        );
        for level in [
            AccessLevel::Friends,
            AccessLevel::Squadron,
            AccessLevel::SquadronFriends,
        ] {
            assert_eq!(
                verdict(
                    Docking {
                        level,
                        notorious_ok: true
                    },
                    0.0
                ),
                (Access::Restricted, Some(Closed::Level))
            );
        }
    }

    /// Notoriety is checked first, so a closed door reports the reason that
    /// the commander can actually do something about.
    #[test]
    fn notoriety_outranks_the_level_in_the_reason() {
        let (_, why) = verdict(
            Docking {
                level: AccessLevel::Squadron,
                notorious_ok: false,
            },
            2.0,
        );
        assert_eq!(why, Some(Closed::Notoriety));
    }

    fn reply(market_id: f64, level: &str, notorious: bool) -> JsValue {
        JsValue::parse(&format!(
            r#"{{"fleetCarrier":{{"market_id":{market_id},"body_site_id":1,
               "docking":{{"accessLevel":"{level}","notoriousAccess":{notorious}}},
               "owner":{{"user_id":909522,"squadron_id":82472}}}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_clean_reply_yields_its_docking_and_owner_ids() {
        let parsed = parse_info(&reply(3_711_014_400.0, "squadronfriends", false), 3_711_014_400.0)
            .unwrap();
        assert_eq!(parsed.docking.level, AccessLevel::SquadronFriends);
        assert!(!parsed.docking.notorious_ok);
        assert_eq!(parsed.owner_squadron_id, Some(82472.0));
        assert_eq!(parsed.owner_user_id, Some(909_522.0));
    }

    /// The check `market/list` cannot offer: every reply names the carrier it
    /// is about, so a mis-addressed answer is catchable rather than silently
    /// filtering the wrong station.
    #[test]
    fn a_reply_about_another_carrier_is_refused() {
        let error = parse_info(&reply(1.0, "all", true), 3_711_014_400.0).unwrap_err();
        assert_eq!(
            error,
            Refusal::Identity {
                expected: 3_711_014_400.0,
                echoed: Some(1.0)
            }
        );
    }

    #[test]
    fn an_unrecognised_level_is_a_question_not_a_guess() {
        let error = parse_info(&reply(1.0, "squadronsonly", true), 1.0).unwrap_err();
        assert_eq!(error, Refusal::UnknownLevel("squadronsonly".to_owned()));
    }

    #[test]
    fn a_reply_without_a_docking_object_is_refused() {
        let document = JsValue::parse(r#"{"fleetCarrier":{"market_id":1}}"#).unwrap();
        assert_eq!(parse_info(&document, 1.0), Err(Refusal::Shape));
        assert_eq!(parse_info(&JsValue::parse("{}").unwrap(), 1.0), Err(Refusal::Shape));
    }

    #[test]
    fn an_absent_notorious_flag_keeps_the_carrier() {
        let document =
            JsValue::parse(r#"{"fleetCarrier":{"market_id":1,"docking":{"accessLevel":"all"}}}"#)
                .unwrap();
        assert!(parse_info(&document, 1.0).unwrap().docking.notorious_ok);
    }

    #[test]
    fn only_any_skips_the_probes() {
        assert!(!Policy::Any.filters());
        assert!(Policy::Open.filters());
        assert!(Policy::Proven.filters());
    }

    #[test]
    fn the_policies_differ_only_on_what_this_run_could_not_read() {
        for policy in [Policy::Any, Policy::Open, Policy::Proven] {
            assert!(policy.admits(Access::Open), "{}", policy.name());
        }
        assert!(Policy::Any.admits(Access::Restricted));
        assert!(!Policy::Open.admits(Access::Restricted));
        assert!(!Policy::Proven.admits(Access::Restricted));

        assert!(Policy::Any.admits(Access::Unknown));
        assert!(Policy::Open.admits(Access::Unknown));
        assert!(!Policy::Proven.admits(Access::Unknown));
    }

    #[test]
    fn policy_parses_case_and_separator_insensitively() {
        assert_eq!(Policy::parse("open"), Some(Policy::Open));
        assert_eq!(Policy::parse("PROVEN"), Some(Policy::Proven));
        assert_eq!(Policy::parse("Any"), Some(Policy::Any));
        assert_eq!(Policy::parse("friendly"), None);
    }
}
