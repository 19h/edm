//! Fleet-carrier docking access, from Spansh \[C36\].
//!
//! Frontier publishes docking access nowhere this program can otherwise reach.
//! The market payload's top-level keys are exactly `allowsDumping`,
//! `commodities` and `inventory`; Ardent's station record has thirty-five
//! fields and none of them is an access; the game-internal `starsystem`
//! document models none either. EDDN's `commodity/3` schema *does* carry
//! `carrierDockingAccess`, renamed from the journal's `CarrierDockingAccess`,
//! and Spansh is the index that keeps that field where Ardent drops it. Spansh
//! and Ardent were measured ingesting the same EDDN message one second apart,
//! so this is not a second opinion about a market — it is the same observation
//! with one more column.
//!
//! **Three states, not two.** A carrier is restricted, or open, or *unpublished*
//! — and the third is roughly a third of them. Folding unpublished into
//! "dockable" would be a filter that quietly stops filtering, and folding it
//! into "restricted" would drop a station on a missing field rather than on a
//! measured one, which is the rule [`crate::select`] already states for a
//! missing arrival distance. So [`Access::Unknown`] is a value here, it
//! survives the default policy, and it is counted on screen.
//!
//! **The response is not trusted.** Spansh answers HTTP 200 to several
//! malformed requests, and every one of those answers is indistinguishable from
//! a real one by its status alone:
//!
//! - a `size` above 500 is silently replaced by 25, so a batch would come back
//!   truncated and the missing rows would read as "not restricted";
//! - a *misspelled filter key* is ignored rather than refused, and the server
//!   echoes the misspelling back under `search`, so the echo cannot be used to
//!   validate anything — the reply is the whole unfiltered id set;
//! - a `market_id` filter that is ignored the same way returns rows for
//!   stations nobody asked about.
//!
//! Each has a guard below, and each guard refuses the run rather than returning
//! a filter that silently covers less than it claims.

use crate::js;
use crate::js::json::{JsObject, JsValue};

/// Spansh's spelling of every docking access a carrier can publish.
///
/// Exact and case-sensitive on the wire: `["SquadronFriends"]` and
/// `["squadron friends"]` both match nothing, and matching nothing is spelled
/// the same way as "nothing is restricted" — a silent no-op rather than an
/// error. They are `const` and pinned by a test for that reason.
pub const RESTRICTED_ACCESS: [&str; 4] = ["Squadron", "Friends", "Squadron Friends", "None"];

/// The one value that means anybody may dock.
pub const OPEN_ACCESS: &str = "All";

/// How many market ids go into one request.
///
/// A batch can return at most this many rows, so `size` equal to it means one
/// page always suffices and `from + size` can never approach Spansh's hard
/// 10,000 result wall (past which it answers HTTP 500).
pub const BATCH_IDS: usize = 500;

/// What Spansh publishes about one carrier's door.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// `All` — anybody may dock.
    Open,
    /// `Squadron`, `Friends`, `Squadron Friends` or `None`.
    ///
    /// The four are one state here on purpose. Nothing this program can read —
    /// the journal included — knows the commander's squadron or friend list, so
    /// it cannot tell a door that opens for *this* commander from one that does
    /// not, and reporting four flavours of "probably not you" would imply a
    /// discrimination it cannot make.
    Restricted,
    /// Spansh has a row for this carrier and no `carrier_docking_access` in it,
    /// or has no row at all. The two are the same fact — nobody has reported
    /// this door — and no caller has ever needed to tell them apart.
    Unknown,
}

/// Which carriers a run will keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Policy {
    /// Every carrier. Sends no Spansh request at all.
    Any,
    /// Drop the provably restricted; keep the unpublished.
    #[default]
    Open,
    /// Keep only what Spansh affirmatively calls `All`.
    Proven,
}

impl Policy {
    /// The spelling `--carrier-access` accepts, and the one a message prints.
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

    /// Parse one `--carrier-access` value.
    ///
    /// Case- and separator-insensitive, matching how [`crate::cli::flag`]
    /// treats flag *names*: a user who writes `--carrier-access Open` has not
    /// made a mistake worth an exit code.
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

    /// Whether this policy needs Spansh consulted at all.
    #[must_use]
    pub const fn queries_spansh(self) -> bool {
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

/// `POST {base}/stations/search`.
#[must_use]
pub fn search_url(base: &str) -> String {
    format!("{base}/stations/search")
}

/// The request body for one id batch, filtered server-side to one access set.
///
/// Ids are sent as strings formatted through [`js::js_number`] — the same
/// canonical decimal text the cache filename uses, for the reason
/// `route::cache` already gives: an id written by one path and looked up by
/// another is a silent permanent miss, and this program has two paths.
///
/// `size` is always the id count, never a round number: the echo of it is the
/// first guard, and a guard that compares against a constant cannot catch a
/// batch that was short for some other reason.
#[must_use]
pub fn search_body(market_ids: &[f64], access_values: &[&str]) -> Vec<u8> {
    let ids: Vec<JsValue> = market_ids
        .iter()
        .map(|id| JsValue::Str(js::js_number(*id).into_boxed_str()))
        .collect();
    let accesses: Vec<JsValue> = access_values
        .iter()
        .map(|value| JsValue::Str((*value).into()))
        .collect();

    let filters = object([
        ("market_id", object([("value", JsValue::Arr(ids))])),
        (
            "carrier_docking_access",
            object([("value", JsValue::Arr(accesses))]),
        ),
    ]);

    object([
        ("filters", filters),
        ("size", JsValue::Num(market_ids.len() as f64)),
        ("page", JsValue::Num(0.0)),
    ])
    .stringify_compact()
    .into_bytes()
}

/// Why a reply was refused.
///
/// Every variant is a case Spansh answers with HTTP 200, which is what makes
/// them worth naming: none of them is visible to a caller that only checks the
/// status, and each one would read downstream as "fewer carriers are
/// restricted than really are".
// No `Eq`: `echoed` is whatever number the wire carried, and a float has no
// total equality. Nothing compares refusals except the tests.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// The body was not an object with a `results` array.
    Shape,
    /// The echoed `size` is not the one that was sent — Spansh clamped it, and
    /// the page is therefore short by an unknown amount.
    SizeClamped { asked: usize, echoed: f64 },
    /// A row came back for a market that was not in this batch, so a filter was
    /// ignored rather than applied.
    ForeignMarket { market_id: f64 },
    /// More rows than ids were asked about.
    Overlong { rows: usize, asked: usize },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape => write!(f, "the reply had no results array"),
            Self::SizeClamped { asked, echoed } => write!(
                f,
                "asked for {asked} results and it echoed {}, so the page is short",
                js::js_number(*echoed)
            ),
            Self::ForeignMarket { market_id } => write!(
                f,
                "it returned market {} which was not in the batch, so a filter was ignored",
                js::js_number(*market_id)
            ),
            Self::Overlong { rows, asked } => {
                write!(f, "it returned {rows} rows for {asked} markets")
            }
        }
    }
}

/// The market ids in one filtered reply.
///
/// Only `market_id` is read. Spansh's `system_name` disagrees with Ardent's for
/// nearly a fifth of carriers — they jump — and its `distance` is measured from
/// Spansh's own default reference rather than this run's centre, so admitting
/// either into the route would silently mix two reference frames against
/// [`crate::ardent::separation_ly`]. The parser does not expose them.
pub fn parse_search(
    document: &JsValue,
    batch: &[f64],
    asked: usize,
) -> Result<Vec<f64>, Refusal> {
    let root = document.as_record().ok_or(Refusal::Shape)?;
    let rows = root
        .get("results")
        .and_then(JsValue::as_array)
        .ok_or(Refusal::Shape)?;

    // Checked before the rows are read: a clamped `size` means the page is
    // short, and a short page's *contents* are all perfectly valid. There is
    // nothing in the rows themselves to notice.
    match root.get("size").and_then(JsValue::as_f64) {
        Some(echoed) if echoed == asked as f64 => {}
        Some(echoed) => return Err(Refusal::SizeClamped { asked, echoed }),
        None => return Err(Refusal::Shape),
    }
    if rows.len() > asked {
        return Err(Refusal::Overlong {
            rows: rows.len(),
            asked,
        });
    }

    let mut found = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(market_id) = row.as_record().and_then(|r| r.get("market_id")).and_then(JsValue::as_f64) else {
            continue;
        };
        if !batch.contains(&market_id) {
            return Err(Refusal::ForeignMarket { market_id });
        }
        found.push(market_id);
    }
    Ok(found)
}

fn object<'k>(entries: impl IntoIterator<Item = (&'k str, JsValue)>) -> JsValue {
    JsValue::Obj(JsObject::from_document_order(
        entries.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_is_the_search_endpoint() {
        assert_eq!(
            search_url("https://spansh.co.uk/api"),
            "https://spansh.co.uk/api/stations/search"
        );
    }

    #[test]
    fn the_body_sends_ids_as_canonical_decimal_strings() {
        let body = search_body(&[3_711_014_400.0, 128_000_000.0], &[OPEN_ACCESS]);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            r#"{"filters":{"market_id":{"value":["3711014400","128000000"]},"carrier_docking_access":{"value":["All"]}},"size":2,"page":0}"#
        );
    }

    /// The four restricted spellings are exact. A test rather than a comment
    /// because a typo here does not fail — it matches nothing, which is the
    /// same reply as "nothing is restricted".
    #[test]
    fn the_restricted_spellings_are_pinned() {
        assert_eq!(
            RESTRICTED_ACCESS,
            ["Squadron", "Friends", "Squadron Friends", "None"]
        );
        assert_eq!(OPEN_ACCESS, "All");
    }

    fn reply(size: f64, ids: &[f64]) -> JsValue {
        let rows: Vec<JsValue> = ids
            .iter()
            .map(|id| object([("market_id", JsValue::Num(*id))]))
            .collect();
        object([("size", JsValue::Num(size)), ("results", JsValue::Arr(rows))])
    }

    #[test]
    fn a_clean_reply_yields_its_market_ids() {
        let batch = [1.0, 2.0, 3.0];
        let found = parse_search(&reply(3.0, &[1.0, 3.0]), &batch, 3).unwrap();
        assert_eq!(found, vec![1.0, 3.0]);
    }

    /// Spansh answers a `size` above 500 with HTTP 200, `"size":25` and 25
    /// rows. Unguarded, the 475 rows it did not send read as "not restricted".
    #[test]
    fn a_clamped_size_is_refused() {
        let batch: Vec<f64> = (0..501).map(f64::from).collect();
        assert_eq!(
            parse_search(&reply(25.0, &[1.0]), &batch, 501),
            Err(Refusal::SizeClamped {
                asked: 501,
                echoed: 25.0
            })
        );
    }

    /// A misspelled `market_id` key is ignored rather than refused, and the
    /// reply is then about stations nobody asked about.
    #[test]
    fn a_row_outside_the_batch_is_refused() {
        assert_eq!(
            parse_search(&reply(2.0, &[1.0, 99.0]), &[1.0, 2.0], 2),
            Err(Refusal::ForeignMarket { market_id: 99.0 })
        );
    }

    #[test]
    fn more_rows_than_ids_is_refused() {
        assert_eq!(
            parse_search(&reply(1.0, &[1.0, 2.0]), &[1.0, 2.0], 1),
            Err(Refusal::Overlong { rows: 2, asked: 1 })
        );
    }

    #[test]
    fn a_reply_without_results_is_refused() {
        assert_eq!(
            parse_search(&object([("size", JsValue::Num(1.0))]), &[1.0], 1),
            Err(Refusal::Shape)
        );
    }

    #[test]
    fn policy_parses_case_and_separator_insensitively() {
        assert_eq!(Policy::parse("open"), Some(Policy::Open));
        assert_eq!(Policy::parse("OPEN"), Some(Policy::Open));
        assert_eq!(Policy::parse("Proven"), Some(Policy::Proven));
        assert_eq!(Policy::parse("any"), Some(Policy::Any));
        assert_eq!(Policy::parse("friendly"), None);
    }

    #[test]
    fn only_any_skips_spansh() {
        assert!(!Policy::Any.queries_spansh());
        assert!(Policy::Open.queries_spansh());
        assert!(Policy::Proven.queries_spansh());
    }

    /// The whole point of the three states: `open` keeps what nobody has
    /// reported, `proven` does not, and neither keeps a restricted door.
    #[test]
    fn the_policies_differ_only_on_the_unpublished() {
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
}
