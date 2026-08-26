//! Which fleet carriers a commander can actually enter \[C36\].
//!
//! `edm route --carriers` used to rank every carrier in the region, and a
//! carrier that limits docking to its owner's squadron ranks exactly as well as
//! one that does not — better, often, because a private carrier's prices are
//! not being arbitraged by anybody. The result was a top-twenty built entirely
//! out of one carrier nobody could dock at.
//!
//! Nothing Frontier publishes says which is which. This module asks Spansh,
//! folds the answer into a per-market verdict, and applies it to the selection
//! **before the spend gate** — so a door that will not open never costs a
//! market read, and the plan the user is asked to approve is a plan they can
//! fly.
//!
//! Three things here are load-bearing and none is obvious:
//!
//! - **Two queries per batch, not one.** Spansh is asked for the restricted
//!   carriers and, separately, for the open ones. A carrier in neither reply is
//!   [`Access::Unknown`] — a real third state, not a default. Asking only for
//!   the restricted set would make "Spansh has never heard of this carrier"
//!   indistinguishable from "Spansh says it is open", which is the conflation
//!   this whole feature exists to prevent.
//! - **The two replies must not overlap.** A misspelled filter key is *ignored*
//!   by Spansh rather than refused, and the reply is then the whole unfiltered
//!   batch — with HTTP 200. Under two queries that shows up as a carrier
//!   reported both restricted and open, which cannot happen and is therefore a
//!   perfect detector.
//! - **`Unknown` is cached like any other verdict.** It is a recorded
//!   measurement — "asked, and nobody has reported this door" — not an absent
//!   file. Treating it as a miss would re-query a third of every region on
//!   every run, forever.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use edm_core::js;
use edm_core::js::json::{JsObject, JsValue};
use edm_core::domain::commander::{CarrierDoor, CommanderState};
use edm_core::select::Selection;
use edm_core::spansh::{self, Access, Policy};
use edm_core::spend::Exclusion;

use crate::net::HttpTransport;
use crate::ports::Fs;
use crate::spansh::SpanshClient;

/// Cache namespace. Provider *and* fact, following `frontier-market-list`, so a
/// later Spansh feature gets its own root and cannot read this one's entries.
const PROVIDER_NAMESPACE: &str = "spansh-carrier-access";

/// Bumped whenever the stored shape changes; an older entry is a miss.
const FORMAT_VERSION: u32 = 1;

/// How long a verdict is reused, in minutes.
///
/// Six hours: a quarter of the market-list lifetime, and far longer than a
/// price. Docking access is an owner-mutable setting so it ages faster than a
/// station list — but it is only ever *republished* when somebody docks and
/// opens the market screen, so our view can never be fresher than that anyway.
/// Six hours makes repeated runs inside one play session free and self-corrects
/// a flipped setting within one.
pub const LIFETIME_MINUTES: f64 = 360.0;

/// How many Spansh requests are in flight at once.
///
/// Four, not the sixteen the Ardent gather uses: Spansh rows are two orders of
/// magnitude larger, the server closes the connection after every reply so each
/// request pays a fresh handshake, and four matches the auxiliary client's own
/// idle-connection pool. No throttling was observable in measurement, but the
/// entire realistic worst case is a handful of requests, so there is nothing to
/// buy by pressing harder.
const CONCURRENCY: usize = 4;

/// The label the plan table uses for a carrier that will not admit us.
const RESTRICTED_LABEL: &str = "carriers that restrict docking";
/// The label for a carrier nobody has published an access for.
const UNPROVEN_LABEL: &str = "carriers with no published access";

/// Every carrier's verdict, by market id.
#[derive(Clone, Debug, Default)]
pub struct AccessIndex {
    verdicts: HashMap<u64, Access>,
}

impl AccessIndex {
    /// The verdict for one market.
    ///
    /// A market this index never looked up reads as [`Access::Unknown`], which
    /// is the same answer as a market Spansh has never heard of — and the same
    /// treatment. There is no way to ask this index a question it answers
    /// wrongly by omission.
    #[must_use]
    pub fn get(&self, market_id: f64) -> Access {
        self.verdicts
            .get(&market_id.to_bits())
            .copied()
            .unwrap_or(Access::Unknown)
    }

    /// Whether this index has a verdict for this market at all.
    ///
    /// The journal overlay uses it so a door the commander happens to know
    /// about, but which is not a candidate on this run, does not silently
    /// enlarge the index and inflate the counts.
    #[must_use]
    pub fn knows(&self, market_id: f64) -> bool {
        self.verdicts.contains_key(&market_id.to_bits())
    }

    fn set(&mut self, market_id: f64, access: Access) {
        self.verdicts.insert(market_id.to_bits(), access);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.verdicts.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.verdicts.is_empty()
    }
}

/// What resolving the index cost, for the note and the JSON document.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub carriers: usize,
    pub requests: usize,
    pub cache_hits: usize,
    pub restricted: usize,
    pub unknown: usize,
    /// Verdicts this commander's own journal set, overriding Spansh.
    pub from_journal: usize,
    /// Of those, the ones where the journal and Spansh disagreed. Worth its own
    /// counter: a non-zero value is the crowd index being measurably wrong
    /// about a door this ship has stood in front of.
    pub journal_corrections: usize,
}

/// What the filter removed, for the plan table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub restricted: usize,
    /// Non-zero only under [`Policy::Proven`].
    pub unproven: usize,
    /// Kept, but unproven. Not a removal — the number the user is owed anyway,
    /// because it is the size of the claim this filter is *not* making.
    pub unproven_kept: usize,
}

impl Removed {
    #[must_use]
    pub const fn total(self) -> usize {
        self.restricted + self.unproven
    }
}

/// What one docking-access pass cost and what it did, together.
///
/// Carried to the `--json` document so a filtered run's output states the
/// filter: a consumer that cannot tell a region with no restricted carriers
/// from a region nobody checked is back where this feature started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub cost: Cost,
    pub removed: Removed,
}

/// One batch's two filtered replies: which index it came from, the restricted
/// ids and the open ones.
type BatchAnswer = (usize, Vec<f64>, Vec<f64>);

/// Where one market's verdict is cached.
fn path(root: &Path, market_id: f64) -> PathBuf {
    root.join(PROVIDER_NAMESPACE)
        .join(format!("{}.json", js::js_number(market_id)))
}

fn encode(access: Access) -> &'static str {
    match access {
        Access::Open => "open",
        Access::Restricted => "restricted",
        Access::Unknown => "unknown",
    }
}

fn decode(name: &str) -> Option<Access> {
    match name {
        "open" => Some(Access::Open),
        "restricted" => Some(Access::Restricted),
        "unknown" => Some(Access::Unknown),
        _ => None,
    }
}

/// How the cache is allowed to be used on this run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CachePolicy {
    /// `--no-cache` clears this.
    pub enabled: bool,
    /// `--refresh` sets this: write, but never read.
    pub refresh: bool,
}

fn cached<F: Fs>(
    fs: &F,
    root: &Path,
    market_id: f64,
    now_ms: f64,
    policy: CachePolicy,
) -> Option<Access> {
    if !policy.enabled || policy.refresh {
        return None;
    }
    let text = fs.read_to_string(&path(root, market_id)).ok()?;
    let document = JsValue::parse(&text).ok()?;
    let record = document.as_record()?;
    if record.get("version").and_then(JsValue::as_f64)? != f64::from(FORMAT_VERSION) {
        return None;
    }
    let read_at = record.get("readAt").and_then(JsValue::as_f64)?;
    let age_ms = now_ms - read_at;
    // A future timestamp is not a fresh observation: either the entry is
    // corrupt or a clock moved, and honouring it would extend the lifetime
    // past the bound the flag set. `contains` is false for `NaN` too, which is
    // the answer a corrupt `readAt` deserves.
    if !(0.0..=LIFETIME_MINUTES * 60_000.0).contains(&age_ms) {
        return None;
    }
    decode(record.get("access").and_then(JsValue::as_str)?)
}

fn bank<F: Fs>(
    fs: &F,
    root: &Path,
    market_id: f64,
    access: Access,
    now_ms: f64,
    policy: CachePolicy,
) {
    if !policy.enabled {
        return;
    }
    let provider_root = root.join(PROVIDER_NAMESPACE);
    if fs.create_dir_all(&provider_root).is_err() {
        return;
    }
    let entry = JsObject::from_document_order(vec![
        ("provider".into(), JsValue::Str(PROVIDER_NAMESPACE.into())),
        ("marketId".into(), JsValue::Num(market_id)),
        ("readAt".into(), JsValue::Num(now_ms)),
        ("version".into(), JsValue::Num(f64::from(FORMAT_VERSION))),
        ("access".into(), JsValue::Str(encode(access).into())),
    ]);
    // Silent on failure, like every other cache here: a cache that cannot be
    // written is a lost optimisation, not a lost run.
    let _ = fs.write(
        &path(root, market_id),
        &JsValue::Obj(entry).stringify_compact(),
    );
}

/// Overlay what this commander's own ship has learned onto a resolved index.
///
/// **The journal always wins, and it is not a tie-break.** Spansh reported
/// market 3712438528 ("1GOT", Nessa) as `All`, having last heard from it the
/// previous day; this commander's ship was answered `DockingDenied` /
/// `RestrictedAccess` by that carrier the next morning. A crowd-sourced index
/// cannot be better than its last reporter, and it cannot know which squadron
/// the reader belongs to at all — so where the two disagree, the one that was
/// actually there is right.
///
/// It cuts both ways, and the `Admitted` direction is the more valuable half:
/// `Policy::Open` drops every squadron- and friends-only carrier because
/// nothing else this program reads knows the commander's squadron or friend
/// list — but a `Docked` this ship completed is proof of membership, and
/// restores a carrier the published policy would have thrown away.
fn overlay_journal(index: &mut AccessIndex, commander: Option<&CommanderState>, cost: &mut Cost) {
    let Some(state) = commander else {
        return;
    };
    for (market_id, observation) in &state.carrier_doors {
        let id = *market_id as f64;
        if !index.knows(id) {
            continue;
        }
        let door = match observation.door {
            CarrierDoor::Admitted => Access::Open,
            CarrierDoor::Refused => Access::Restricted,
        };
        let published = index.get(id);
        if published != door {
            cost.journal_corrections += 1;
        }
        cost.from_journal += 1;
        index.set(id, door);
    }
}

/// Resolve every carrier's docking access.
///
/// `market_ids` is the carriers only — a non-carrier has no access to publish
/// and asking about one would spend a slot in a batch to be told nothing.
///
/// Fails loudly. A partial answer is not accepted either: a filter enforced
/// over some of the candidates is a filter that lies about the rest, and the
/// lie is in the safe-looking direction.
pub async fn resolve<H: HttpTransport, F: Fs>(
    client: &SpanshClient<'_, H>,
    fs: &F,
    cache_root: &Path,
    market_ids: &[f64],
    now_ms: f64,
    cache_policy: CachePolicy,
    commander: Option<&CommanderState>,
    report: Option<&dyn Fn(usize, usize)>,
) -> Result<(AccessIndex, Cost), String> {
    use futures_util::StreamExt as _;

    let mut index = AccessIndex::default();
    let mut cost = Cost {
        carriers: market_ids.len(),
        ..Cost::default()
    };

    // Deduplicated, and the cache drained first so a warm session sends
    // nothing at all.
    let mut wanted: Vec<f64> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    for id in market_ids {
        if !seen.insert(id.to_bits()) {
            continue;
        }
        match cached(fs, cache_root, *id, now_ms, cache_policy) {
            Some(access) => {
                cost.cache_hits += 1;
                index.set(*id, access);
            }
            None => wanted.push(*id),
        }
    }

    let batches: Vec<&[f64]> = wanted.chunks(spansh::BATCH_IDS).collect();
    let total_requests = batches.len() * 2;
    let done = std::cell::Cell::new(0usize);

    let answers: Vec<Result<BatchAnswer, String>> =
        futures_util::stream::iter(batches.iter().enumerate().map(|(nth, batch)| {
            let done = &done;
            async move {
                let restricted = client
                    .carriers_with_access(batch, &spansh::RESTRICTED_ACCESS)
                    .await?;
                done.set(done.get() + 1);
                if let Some(report) = report {
                    report(done.get(), total_requests);
                }
                let open = client
                    .carriers_with_access(batch, &[spansh::OPEN_ACCESS])
                    .await?;
                done.set(done.get() + 1);
                if let Some(report) = report {
                    report(done.get(), total_requests);
                }
                Ok((nth, restricted, open))
            }
        }))
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    for answer in answers {
        let (nth, restricted, open) = answer?;
        let batch = batches[nth];
        cost.requests += 2;

        // The overlap guard. A filter key Spansh does not recognise is ignored
        // rather than refused, and the reply is the whole unfiltered batch with
        // HTTP 200 — which lands here as a carrier that is both restricted and
        // open. Nothing else in the protocol can distinguish that reply from a
        // real one.
        if let Some(both) = restricted.iter().find(|id| open.contains(id)) {
            return Err(format!(
                "Spansh reported market {} as both restricted and open, so a filter was ignored",
                js::js_number(*both)
            ));
        }
        if restricted.len() + open.len() > batch.len() {
            return Err(format!(
                "Spansh reported {} restricted and {} open carriers out of a batch of {}",
                restricted.len(),
                open.len(),
                batch.len()
            ));
        }

        for id in batch {
            let access = if restricted.contains(id) {
                Access::Restricted
            } else if open.contains(id) {
                Access::Open
            } else {
                Access::Unknown
            };
            index.set(*id, access);
            bank(fs, cache_root, *id, access, now_ms, cache_policy);
        }
    }

    // After Spansh and after the cache, because it overrides both — and before
    // the tally, so the counts the user reads describe the verdicts actually
    // used.
    overlay_journal(&mut index, commander, &mut cost);

    for id in market_ids {
        match index.get(*id) {
            Access::Restricted => cost.restricted += 1,
            Access::Unknown => cost.unknown += 1,
            Access::Open => {}
        }
    }

    Ok((index, cost))
}

/// Drop the carriers this policy will not admit, and record why.
///
/// Only carriers are touched. `considered` is deliberately left alone — it is
/// the size of what Ardent offered, and the exclusions are a ledger against it,
/// so moving both would make the plan's arithmetic stop closing.
pub fn apply(selection: &mut Selection, index: &AccessIndex, policy: Policy) -> Removed {
    let mut removed = Removed::default();
    if !policy.queries_spansh() {
        return removed;
    }

    let kept = std::mem::take(&mut selection.keep);
    selection.keep = kept
        .into_iter()
        .filter(|station| {
            if !edm_core::ardent::is_carrier(station.station_type.as_deref()) {
                return true;
            }
            let access = index.get(station.market_id);
            if matches!(access, Access::Unknown) && policy.admits(access) {
                removed.unproven_kept += 1;
            }
            if policy.admits(access) {
                return true;
            }
            match access {
                Access::Restricted => removed.restricted += 1,
                Access::Unknown => removed.unproven += 1,
                Access::Open => {}
            }
            false
        })
        .collect();

    // Zero-count rows are not pushed: the plan lists filters that removed
    // something, and a line saying nothing happened is noise in a table whose
    // whole job is to explain a number.
    if removed.restricted > 0 {
        selection.exclusions.push(Exclusion {
            label: RESTRICTED_LABEL,
            removed: removed.restricted,
            keep_with: "--carrier-access any",
        });
    }
    if removed.unproven > 0 {
        selection.exclusions.push(Exclusion {
            label: UNPROVEN_LABEL,
            removed: removed.unproven,
            keep_with: "--carrier-access open",
        });
    }
    removed
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use edm_core::ardent::ArdentStation;
    use edm_core::domain::id64::Coordinates;

    #[derive(Debug, Default)]
    struct MemFs {
        files: RefCell<HashMap<PathBuf, String>>,
    }

    impl Fs for MemFs {
        fn write(&self, path: &Path, contents: &str) -> std::io::Result<()> {
            self.files
                .borrow_mut()
                .insert(path.to_path_buf(), contents.to_owned());
            Ok(())
        }
        fn read_to_string(&self, path: &Path) -> std::io::Result<String> {
            self.files.borrow().get(path).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no such entry")
            })
        }
        fn create_dir_all(&self, _path: &Path) -> std::io::Result<()> {
            Ok(())
        }
        fn read_dir(&self, _path: &Path) -> std::io::Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.borrow().contains_key(path)
        }
    }

    const LIVE: CachePolicy = CachePolicy {
        enabled: true,
        refresh: false,
    };

    fn station(market_id: f64, kind: &str) -> ArdentStation {
        ArdentStation {
            market_id,
            station_name: format!("S{market_id}"),
            system_name: "Sol".to_owned(),
            system_address: 1.0,
            station_type: Some(kind.to_owned()),
            max_landing_pad_size: Some(3.0),
            distance_to_arrival: Some(100.0),
            coordinates: Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        }
    }

    fn selection_of(stations: Vec<ArdentStation>) -> Selection {
        Selection {
            considered: stations.len(),
            keep: stations,
            exclusions: Vec::new(),
        }
    }

    fn index_of(entries: &[(f64, Access)]) -> AccessIndex {
        let mut index = AccessIndex::default();
        for (id, access) in entries {
            index.set(*id, *access);
        }
        index
    }

    #[test]
    fn a_verdict_survives_a_cache_round_trip_including_unknown() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        for (id, access) in [
            (1.0, Access::Open),
            (2.0, Access::Restricted),
            (3.0, Access::Unknown),
        ] {
            bank(&fs, root, id, access, 1_000.0, LIVE);
            assert_eq!(cached(&fs, root, id, 1_000.0, LIVE), Some(access));
        }
    }

    /// The reason `Unknown` is banked at all: otherwise a third of every region
    /// is re-queried on every run, forever.
    #[test]
    fn an_unknown_verdict_is_a_cache_hit_not_a_miss() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, 7.0, Access::Unknown, 0.0, LIVE);
        assert_eq!(cached(&fs, root, 7.0, 0.0, LIVE), Some(Access::Unknown));
    }

    #[test]
    fn a_verdict_older_than_the_lifetime_is_a_miss() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, 1.0, Access::Open, 0.0, LIVE);
        let just_inside = LIFETIME_MINUTES * 60_000.0;
        assert_eq!(
            cached(&fs, root, 1.0, just_inside, LIVE),
            Some(Access::Open)
        );
        assert_eq!(cached(&fs, root, 1.0, just_inside + 1.0, LIVE), None);
    }

    #[test]
    fn a_future_timestamp_is_a_miss_not_an_extended_lifetime() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, 1.0, Access::Open, 10_000.0, LIVE);
        assert_eq!(cached(&fs, root, 1.0, 0.0, LIVE), None);
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_not_a_crash() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        fs.write(&path(root, 1.0), "{not json").unwrap();
        assert_eq!(cached(&fs, root, 1.0, 0.0, LIVE), None);
        fs.write(&path(root, 2.0), r#"{"version":99,"access":"open"}"#)
            .unwrap();
        assert_eq!(cached(&fs, root, 2.0, 0.0, LIVE), None);
    }

    #[test]
    fn no_cache_neither_reads_nor_writes() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        let off = CachePolicy {
            enabled: false,
            refresh: false,
        };
        bank(&fs, root, 1.0, Access::Open, 0.0, off);
        assert!(fs.files.borrow().is_empty());
        bank(&fs, root, 1.0, Access::Open, 0.0, LIVE);
        assert_eq!(cached(&fs, root, 1.0, 0.0, off), None);
    }

    #[test]
    fn refresh_writes_but_never_reads() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        let refresh = CachePolicy {
            enabled: true,
            refresh: true,
        };
        bank(&fs, root, 1.0, Access::Open, 0.0, refresh);
        assert_eq!(cached(&fs, root, 1.0, 0.0, refresh), None);
        assert_eq!(cached(&fs, root, 1.0, 0.0, LIVE), Some(Access::Open));
    }

    #[test]
    fn open_drops_the_restricted_and_keeps_the_unproven() {
        let mut selection = selection_of(vec![
            station(1.0, "FleetCarrier"),
            station(2.0, "FleetCarrier"),
            station(3.0, "FleetCarrier"),
            station(4.0, "Coriolis"),
        ]);
        let index = index_of(&[
            (1.0, Access::Open),
            (2.0, Access::Restricted),
            (3.0, Access::Unknown),
        ]);
        let removed = apply(&mut selection, &index, Policy::Open);

        assert_eq!(removed.restricted, 1);
        assert_eq!(removed.unproven, 0);
        assert_eq!(removed.unproven_kept, 1);
        let kept: Vec<f64> = selection.keep.iter().map(|s| s.market_id).collect();
        assert_eq!(kept, vec![1.0, 3.0, 4.0]);
        assert_eq!(selection.exclusions.len(), 1);
        assert_eq!(selection.exclusions[0].label, RESTRICTED_LABEL);
        assert_eq!(selection.exclusions[0].removed, 1);
    }

    #[test]
    fn proven_drops_the_unproven_too_and_says_so_separately() {
        let mut selection = selection_of(vec![
            station(1.0, "FleetCarrier"),
            station(2.0, "FleetCarrier"),
            station(3.0, "FleetCarrier"),
        ]);
        let index = index_of(&[
            (1.0, Access::Open),
            (2.0, Access::Restricted),
            (3.0, Access::Unknown),
        ]);
        let removed = apply(&mut selection, &index, Policy::Proven);

        assert_eq!(removed.restricted, 1);
        assert_eq!(removed.unproven, 1);
        assert_eq!(removed.unproven_kept, 0);
        assert_eq!(removed.total(), 2);
        let labels: Vec<&str> = selection.exclusions.iter().map(|e| e.label).collect();
        assert_eq!(labels, vec![RESTRICTED_LABEL, UNPROVEN_LABEL]);
    }

    /// `any` is the escape hatch, and it must not merely admit everything — it
    /// must not touch the ledger either, or a run that asked no question would
    /// print an answer.
    #[test]
    fn any_removes_nothing_and_pushes_no_row() {
        let mut selection = selection_of(vec![station(2.0, "FleetCarrier")]);
        let index = index_of(&[(2.0, Access::Restricted)]);
        let removed = apply(&mut selection, &index, Policy::Any);
        assert_eq!(removed, Removed::default());
        assert_eq!(selection.keep.len(), 1);
        assert!(selection.exclusions.is_empty());
    }

    /// A station that is not a carrier has no access to publish, and must not
    /// be dropped for failing to publish one.
    #[test]
    fn a_starport_is_never_filtered_however_strict_the_policy() {
        let mut selection = selection_of(vec![station(4.0, "Coriolis"), station(5.0, "Orbis")]);
        let removed = apply(&mut selection, &AccessIndex::default(), Policy::Proven);
        assert_eq!(removed, Removed::default());
        assert_eq!(selection.keep.len(), 2);
    }

    /// The ledger has to close: what went in is what stayed plus what each row
    /// claims to have taken.
    #[test]
    fn the_exclusion_counts_sum_to_the_stations_removed() {
        let stations: Vec<ArdentStation> = (0..30)
            .map(|n| station(f64::from(n), "FleetCarrier"))
            .collect();
        let index = index_of(
            &(0..30)
                .map(|n| {
                    (
                        f64::from(n),
                        match n % 3 {
                            0 => Access::Open,
                            1 => Access::Restricted,
                            _ => Access::Unknown,
                        },
                    )
                })
                .collect::<Vec<_>>(),
        );
        let before = stations.len();
        let mut selection = selection_of(stations);
        let removed = apply(&mut selection, &index, Policy::Proven);
        let claimed: usize = selection.exclusions.iter().map(|e| e.removed).sum();
        assert_eq!(claimed, removed.total());
        assert_eq!(selection.keep.len() + claimed, before);
        assert_eq!(selection.considered, before, "considered is not moved");
    }

    fn commander_with(doors: &[(u64, CarrierDoor, &str)]) -> CommanderState {
        let mut state = CommanderState::default();
        for (id, door, at) in doors {
            state.carrier_doors.push((
                *id,
                edm_core::domain::commander::DoorObservation {
                    door: *door,
                    observed_at: Some((*at).to_owned()),
                },
            ));
        }
        state
    }

    /// The 1GOT case, exactly. Spansh said `All`, having last heard from the
    /// carrier the day before; this ship was refused by it the next morning.
    #[test]
    fn a_journal_refusal_overrides_a_published_all() {
        let mut index = index_of(&[(3_712_438_528.0, Access::Open)]);
        let mut cost = Cost::default();
        let state = commander_with(&[(
            3_712_438_528,
            CarrierDoor::Refused,
            "2026-08-26T07:18:31Z",
        )]);
        overlay_journal(&mut index, Some(&state), &mut cost);

        assert_eq!(index.get(3_712_438_528.0), Access::Restricted);
        assert_eq!(cost.from_journal, 1);
        assert_eq!(cost.journal_corrections, 1);
    }

    /// The other half, and the reason the overlay is not simply a denylist:
    /// nothing else this program reads knows the commander's squadron, but a
    /// `Docked` this ship completed proves the door opens for them.
    #[test]
    fn a_journal_docking_rescues_a_carrier_the_policy_would_drop() {
        let mut index = index_of(&[(7.0, Access::Restricted)]);
        let mut cost = Cost::default();
        let state = commander_with(&[(7, CarrierDoor::Admitted, "2026-08-26T07:00:00Z")]);
        overlay_journal(&mut index, Some(&state), &mut cost);

        assert_eq!(index.get(7.0), Access::Open);
        assert_eq!(cost.journal_corrections, 1);

        let mut selection = selection_of(vec![station(7.0, "FleetCarrier")]);
        let removed = apply(&mut selection, &index, Policy::Open);
        assert_eq!(removed.restricted, 0, "the commander is in that squadron");
        assert_eq!(selection.keep.len(), 1);
    }

    /// An agreement is still an override, and must not be counted as a
    /// correction — the counter exists to say how often the crowd index was
    /// measurably wrong.
    #[test]
    fn a_journal_observation_that_agrees_is_not_a_correction() {
        let mut index = index_of(&[(7.0, Access::Restricted)]);
        let mut cost = Cost::default();
        let state = commander_with(&[(7, CarrierDoor::Refused, "2026-08-26T07:00:00Z")]);
        overlay_journal(&mut index, Some(&state), &mut cost);
        assert_eq!(cost.from_journal, 1);
        assert_eq!(cost.journal_corrections, 0);
    }

    /// A door the commander knows about but which is not a candidate this run
    /// must not enlarge the index or inflate the counts.
    #[test]
    fn a_journal_door_outside_this_run_is_ignored() {
        let mut index = index_of(&[(7.0, Access::Open)]);
        let mut cost = Cost::default();
        let state = commander_with(&[(999, CarrierDoor::Refused, "2026-08-26T07:00:00Z")]);
        overlay_journal(&mut index, Some(&state), &mut cost);
        assert_eq!(index.len(), 1);
        assert_eq!(cost.from_journal, 0);
        assert!(!index.knows(999.0));
    }

    #[test]
    fn no_commander_state_changes_nothing() {
        let mut index = index_of(&[(7.0, Access::Open)]);
        let mut cost = Cost::default();
        overlay_journal(&mut index, None, &mut cost);
        assert_eq!(index.get(7.0), Access::Open);
        assert_eq!(cost, Cost::default());
    }

    #[test]
    fn an_unindexed_carrier_reads_as_unknown() {
        let index = AccessIndex::default();
        assert_eq!(index.get(12345.0), Access::Unknown);
    }
}
