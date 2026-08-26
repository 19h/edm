//! Which fleet carriers a commander can actually enter \[C37\].
//!
//! `edm route --carriers` used to rank every carrier in the region, and a
//! carrier that limits docking to its owner's squadron ranks exactly as well as
//! one that does not — better, often, because a private carrier's prices are
//! not being arbitraged by anybody. The result was a top-twenty built entirely
//! out of one carrier nobody could dock at.
//!
//! C36 answered that with Spansh, and Spansh was wrong in the way a
//! crowd-sourced index is always wrong: it is only ever refreshed when somebody
//! docks and opens the market screen, so the carrier whose owner closed the
//! door yesterday still reads as open, and the commander who finds out is the
//! one who flew there. This module asks Frontier instead —
//! `2.0/elite/fleetcarrier/info`, one carrier per request, live.
//!
//! Four things here are load-bearing:
//!
//! - **The probes are priced and gated before any of them is sent.** They are
//!   metered requests out of the same budget as the prices, so the caller runs
//!   [`prepare`] — free, file reads only — takes its cold count through a gate
//!   of its own, and only then calls [`probe`]. Spansh was two free requests
//!   and could sit ahead of the plan; this cannot, and pretending otherwise
//!   would make `--max-requests` say "nothing has been sent" after two hundred
//!   requests had gone.
//! - **The cache stores what Frontier said, not what this program concluded.**
//!   A verdict folds in the commander's own notoriety, and caching that would
//!   mean clearing your notoriety left dropped carriers on disk for the rest of
//!   the TTL. The derivation is one function call at read time.
//! - **A failed probe is [`Access::Unknown`] and is not banked**, so the next
//!   run retries instead of caching a hole. The run only *ends* when nothing
//!   succeeded at all — which is indistinguishable from a broken endpoint, and
//!   ranking two hundred unread carriers under `open` would hand back exactly
//!   the unfiltered list the user asked not to have.
//! - **The journal still overrides, but narrowly.** Under Spansh it always won,
//!   because a crowd index cannot be fresher than its last reporter. Against a
//!   live read that argument is gone: see [`overlay_journal`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use edm_core::carrier::{self, Access, AccessLevel, Closed, Docking, Policy};
use edm_core::consts::FLEETCARRIER_INFO;
use edm_core::domain::commander::{CarrierDoor, CommanderState};
use edm_core::js;
use edm_core::js::json::{JsObject, JsValue};
use edm_core::select::Selection;
use edm_core::spend::Exclusion;

use crate::game_api::{self, Credentials, HeaderConfig, Stamp};
use crate::net::HttpTransport;
use crate::out::Out;
use crate::ports::{Clock, Entropy, Fs, Timer};
use crate::route::pacer::Pacer;

/// Cache namespace. Provider *and* fact, following `frontier-market-list`.
///
/// A new directory rather than a format-version bump, because the old one is
/// named after a provider that no longer answers here: leaving Frontier
/// verdicts in a directory called `spansh-carrier-access` would be a name that
/// lies. The old directory simply goes unread; nothing sweeps it.
const PROVIDER_NAMESPACE: &str = "frontier-carrier-access";

/// Bumped whenever the stored shape changes; an older entry is a miss.
const FORMAT_VERSION: u32 = 1;

/// How long a verdict is reused, in minutes.
///
/// **Fifteen, where the Spansh reader used three hundred and sixty.** That six
/// hours was justified entirely by a fact about Spansh — its view could never
/// be fresher than the last commander to dock — and against a live read the
/// reasoning is void. Fifteen minutes because:
///
/// - it is shorter than the flight it informs, so the answer is at least as
///   fresh as the decision it feeds, which six hours is not;
/// - it is long enough that the expensive thing is paid once across the four
///   or five runs a user makes while adjusting `--pad` and `--radius`;
/// - the game itself caches this **not at all** — it was measured re-fetching
///   the same carrier ninety-one seconds apart. Fifteen minutes is already
///   more conservative than the real client, and by one order of magnitude
///   rather than three.
pub const LIFETIME_MINUTES: f64 = 15.0;

/// How many probes are in flight at once.
///
/// The pacer, not this, is what bounds the request rate; this only decides how
/// much of the latency hides behind itself. Eight is the game's own order of
/// magnitude — it was observed firing 37 concurrent `/info` requests and 401 KB
/// in under three seconds, with no throttling and `Fdev-Retry: 0/2` throughout
/// — so anything here is quieter than the real client.
const CONCURRENCY: usize = 8;

/// The label the plan table uses for a carrier that will not admit us.
const RESTRICTED_LABEL: &str = "carriers that restrict docking";
/// The label for a carrier nobody has published an access for.
const UNPROVEN_LABEL: &str = "carriers with no published access";

/// Every carrier's verdict, by market id.
#[derive(Clone, Debug, Default)]
pub struct AccessIndex {
    verdicts: HashMap<u64, Access>,
    /// Ids whose verdict came from a probe issued during *this* run, as
    /// opposed to one read out of the cache. The journal overlay turns on the
    /// distinction and nothing else does.
    fresh: HashSet<u64>,
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

    fn set_fresh(&mut self, market_id: f64, access: Access) {
        self.set(market_id, access);
        self.fresh.insert(market_id.to_bits());
    }

    /// Whether this verdict was read from Frontier during this run.
    #[must_use]
    pub fn is_fresh(&self, market_id: f64) -> bool {
        self.fresh.contains(&market_id.to_bits())
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
    /// Verdicts this commander's own journal set, overriding Frontier.
    pub from_journal: usize,
    /// Of those, the ones where the two disagreed.
    ///
    /// No longer "the crowd index being measurably wrong". Against a live read
    /// it is the rarer and more interesting fact of Frontier and this ship's
    /// own experience not matching — most often a squadron carrier that admits
    /// us and could not have said so.
    pub journal_corrections: usize,
    /// Refusals this ship recorded that a *fresher* live answer overrode.
    pub journal_disagreements: usize,
    /// Carriers closed to this commander only because of notoriety.
    ///
    /// Its own counter and its own exclusion row: eleven of the thirty-one
    /// carriers Frontier calls `all` refuse a notorious commander, and burying
    /// those under "restrict docking" would hide a filter the commander can
    /// actually do something about.
    pub notoriety_blocked: usize,
    /// Carriers Frontier says no longer exist.
    pub gone: usize,
    /// Probes that were sent and did not produce a verdict.
    pub probe_failures: usize,
    /// Carriers whose market id is not congruent to a `fleetCarrierId`.
    ///
    /// Ardent called it a carrier and the arithmetic disagrees. Never silently
    /// kept or silently dropped — it means the two sources disagree about what
    /// a fleet carrier is, which is the class of quiet wrongness this feature
    /// exists to surface.
    pub unprobeable: usize,
    /// Carriers that reached the filter after the priced phase had run, so were
    /// never probed at all.
    pub unprobed: usize,
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

/// Where one market's verdict is cached.
fn path(root: &Path, market_id: f64) -> PathBuf {
    root.join(PROVIDER_NAMESPACE)
        .join(format!("{}.json", js::js_number(market_id)))
}

/// How the cache is allowed to be used on this run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CachePolicy {
    /// `--no-cache` clears this.
    pub enabled: bool,
    /// `--refresh` sets this: write, but never read.
    pub refresh: bool,
    /// `--max-age`, when it is tighter than [`LIFETIME_MINUTES`].
    ///
    /// It can only tighten. It is the flag a user reaches for when they suspect
    /// staleness, and given what this feature is about, leaving it unable to
    /// reach this cache would be a poor joke.
    pub max_age_minutes: Option<f64>,
}

impl CachePolicy {
    fn lifetime_ms(self) -> f64 {
        let minutes = self
            .max_age_minutes
            .map_or(LIFETIME_MINUTES, |asked| js::js_min(LIFETIME_MINUTES, asked));
        minutes * 60_000.0
    }
}

/// Read one banked answer, if there is a fresh one from *this* provider.
///
/// The `provider` check is not decoration. `bank` has always written the field
/// and the Spansh reader never read it back, which made it a comment with extra
/// steps. Reading it catches a hand-copied or symlinked entry that the
/// namespace alone does not — and a Spansh verdict served as a Frontier one is
/// exactly the failure this whole change exists to end.
fn cached<F: Fs>(
    fs: &F,
    root: &Path,
    market_id: f64,
    now_ms: f64,
    policy: CachePolicy,
) -> Option<carrier::Owned> {
    if !policy.enabled || policy.refresh {
        return None;
    }
    let text = fs.read_to_string(&path(root, market_id)).ok()?;
    let document = JsValue::parse(&text).ok()?;
    let record = document.as_record()?;
    if record.get("provider").and_then(JsValue::as_str)? != PROVIDER_NAMESPACE {
        return None;
    }
    if record.get("version").and_then(JsValue::as_f64)? != f64::from(FORMAT_VERSION) {
        return None;
    }
    let read_at = record.get("readAt").and_then(JsValue::as_f64)?;
    let age_ms = now_ms - read_at;
    // A future timestamp is not a fresh observation: either the entry is
    // corrupt or a clock moved, and honouring it would extend the lifetime
    // past the bound the flag set. `contains` is false for `NaN` too, which is
    // the answer a corrupt `readAt` deserves.
    if !(0.0..=policy.lifetime_ms()).contains(&age_ms) {
        return None;
    }
    let level = AccessLevel::parse(record.get("accessLevel").and_then(JsValue::as_str)?)?;
    let notorious_ok = match record.get("notoriousAccess") {
        Some(JsValue::Bool(flag)) => *flag,
        _ => return None,
    };
    Some(carrier::Owned {
        docking: Docking {
            level,
            notorious_ok,
        },
        owner_squadron_id: record.get("ownerSquadronId").and_then(JsValue::as_f64),
        owner_user_id: record.get("ownerUserId").and_then(JsValue::as_f64),
    })
}

/// Bank what Frontier said — never what this program concluded from it.
///
/// The distinction is the reason this function takes an [`carrier::Owned`] and
/// not an [`Access`]: a verdict folds in the commander's notoriety, and a
/// commander who pays off their notoriety would otherwise keep reading dropped
/// carriers out of cache until the entry aged out.
fn bank<F: Fs>(
    fs: &F,
    root: &Path,
    market_id: f64,
    owned: carrier::Owned,
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
    let optional = |value: Option<f64>| value.map_or(JsValue::Null, JsValue::Num);
    let entry = JsObject::from_document_order(vec![
        ("provider".into(), JsValue::Str(PROVIDER_NAMESPACE.into())),
        ("marketId".into(), JsValue::Num(market_id)),
        (
            "fleetCarrierId".into(),
            optional(carrier::carrier_id(market_id)),
        ),
        ("readAt".into(), JsValue::Num(now_ms)),
        ("version".into(), JsValue::Num(f64::from(FORMAT_VERSION))),
        (
            "accessLevel".into(),
            JsValue::Str(owned.docking.level.name().into()),
        ),
        (
            "notoriousAccess".into(),
            JsValue::Bool(owned.docking.notorious_ok),
        ),
        // Carried for a squadron or friends match that is not implemented yet.
        // Eight bytes each, against re-probing every carrier on the day it
        // lands.
        ("ownerSquadronId".into(), optional(owned.owner_squadron_id)),
        ("ownerUserId".into(), optional(owned.owner_user_id)),
    ]);
    // Silent on failure, like every other cache here: a cache that cannot be
    // written is a lost optimisation, not a lost run.
    let _ = fs.write(
        &path(root, market_id),
        &JsValue::Obj(entry).stringify_compact(),
    );
}

/// Overlay what this commander's own ship has learned.
///
/// Under Spansh the journal always won, and the argument was that a crowd index
/// cannot be better than its last reporter. Against a live read that argument
/// is gone — a `DockingDenied` from three days ago would otherwise override an
/// `accessLevel` fetched four seconds ago — so the rule is now split:
///
/// - **`Admitted` always wins.** It is the one question Frontier cannot answer.
///   `accessLevel: squadron` says the door opens for the owner's squadron; it
///   does not say whether *this* commander is in it, and nothing else this
///   program reads knows either. A `Docked` this ship completed is proof of
///   membership, and it restores a carrier the published policy would throw
///   away.
/// - **`Refused` beats a *cached* verdict and loses to a *fresh* one.** A probe
///   issued during this run is by construction newer than a journal line read
///   from disk at startup; a cached entry may not be. That resolves the contest
///   on a fact the resolver already knows, with no date parsing — the
///   observation deliberately keeps its timestamp exactly as written.
///
/// Where the fresh answer wins, the disagreement is still counted and still
/// worth a line: an `all` carrier that refused this ship recently is either an
/// owner who changed their mind back, or a notoriety refusal.
fn overlay_journal(
    index: &mut AccessIndex,
    commander: Option<&CommanderState>,
    cost: &mut Cost,
) {
    let Some(state) = commander else {
        return;
    };
    for (market_id, observation) in &state.carrier_doors {
        let id = *market_id as f64;
        if !index.knows(id) {
            continue;
        }
        let published = index.get(id);
        match observation.door {
            CarrierDoor::Admitted => {
                if published != Access::Open {
                    cost.journal_corrections += 1;
                }
                cost.from_journal += 1;
                index.set(id, Access::Open);
            }
            CarrierDoor::Refused => {
                if index.is_fresh(id) {
                    // The live answer is newer than the journal line. Keep it,
                    // and say that the two disagree.
                    if published != Access::Restricted {
                        cost.journal_disagreements += 1;
                    }
                    continue;
                }
                if published != Access::Restricted {
                    cost.journal_corrections += 1;
                }
                cost.from_journal += 1;
                index.set(id, Access::Restricted);
            }
        }
    }
}

/// What the free pass established, and what it costs to finish.
#[derive(Clone, Debug, Default)]
pub struct Prepared {
    /// Verdicts already known, from the cache.
    pub index: AccessIndex,
    /// Market ids that still need a live read. **This is the priced number.**
    pub cold: Vec<f64>,
    pub cost: Cost,
}

/// Drain the cache and apply the id arithmetic. Costs file reads and nothing
/// else.
///
/// Split out of the probe so the caller can price [`Prepared::cold`] and put it
/// through a gate before a single request is built. `acquire::prepare` gives
/// the reason in as many words: a plan that priced twenty-two requests and then
/// sent none is a plan nobody can check — and its inverse, a plan that priced
/// none and then sent two hundred, is worse.
pub fn prepare<F: Fs>(
    fs: &F,
    cache_root: &Path,
    market_ids: &[f64],
    now_ms: f64,
    cache_policy: CachePolicy,
    notoriety: f64,
) -> Prepared {
    let mut prepared = Prepared {
        cost: Cost {
            carriers: market_ids.len(),
            ..Cost::default()
        },
        ..Prepared::default()
    };

    let mut seen: HashSet<u64> = HashSet::new();
    for market_id in market_ids {
        if !seen.insert(market_id.to_bits()) {
            continue;
        }
        // Ardent says this is a carrier and Frontier's own id space says it is
        // not. Neither keep it quietly nor drop it quietly.
        if carrier::carrier_id(*market_id).is_none() {
            prepared.cost.unprobeable += 1;
            prepared.index.set(*market_id, Access::Unknown);
            continue;
        }
        match cached(fs, cache_root, *market_id, now_ms, cache_policy) {
            Some(owned) => {
                prepared.cost.cache_hits += 1;
                let (access, why) = carrier::verdict(owned.docking, notoriety);
                if why == Some(Closed::Notoriety) {
                    prepared.cost.notoriety_blocked += 1;
                }
                prepared.index.set(*market_id, access);
            }
            None => prepared.cold.push(*market_id),
        }
    }
    prepared
}

/// Everything one probe needs that this module cannot compute for itself.
///
/// Mirrors the sweep's own context struct, and for the same reason: the
/// alternative is nine positional parameters of which four are `Option`s.
pub struct ProbeCx<'a, H, C, E> {
    pub http: &'a H,
    pub out: &'a Out,
    /// `EDM_ORIGIN_OVERRIDE`, or the game-internal API's own origin.
    pub origin: &'a str,
    pub clock: &'a C,
    pub entropy: &'a E,
    pub credentials: &'a Credentials,
    pub headers: &'a HeaderConfig,
    pub language: &'a str,
    pub method_override: Option<&'a str>,
    pub dry_run: bool,
    pub nonce_override: Option<edm_core::wire::Nonce>,
    pub frontier_time_override: Option<f64>,
    pub request_time_override: Option<u32>,
}

impl<H, C, E> std::fmt::Debug for ProbeCx<'_, H, C, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeCx")
            .field("origin", &self.origin)
            .field("dry_run", &self.dry_run)
            .finish_non_exhaustive()
    }
}

/// What one probe produced.
enum Probed {
    Verdict(carrier::Owned),
    /// HTTP 204: Frontier has no such carrier. Measured, not assumed.
    Gone,
    /// Anything else. The message is for the caller's warning, not for a table.
    Failed(String),
}

/// Read the cold set live, banking and indexing as the answers land.
///
/// Every request goes through `pacer.acquire()`, which is what keeps the probes
/// inside the run's own rate limit, breaker window and `--deadline`, and — the
/// part that is easy to miss — what makes them appear in `spent.requests`, so
/// the coverage table's arithmetic closes without anything being added back by
/// hand.
///
/// Returns `Err` only when the cold set was non-empty and **nothing** answered.
/// That is indistinguishable from a broken endpoint or a dead credential, and
/// ranking two hundred unread carriers under `open` would hand back precisely
/// the unfiltered list the user asked not to have. Any lesser failure count
/// lets the run finish, with the gaps counted and named.
#[expect(
    clippy::too_many_arguments,
    reason = "the transport context, the pacer, the cache, the work, and the commander fact that turns an answer into a verdict"
)]
pub async fn probe<H, C, E, J, T, F>(
    cx: &ProbeCx<'_, H, C, E>,
    pacer: &Pacer<'_, C, T, J>,
    fs: &F,
    cache_root: &Path,
    cold: &[f64],
    now_ms: f64,
    cache_policy: CachePolicy,
    notoriety: f64,
    index: &mut AccessIndex,
    cost: &mut Cost,
    report: Option<&dyn Fn(usize, usize)>,
) -> Result<(), String>
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    J: Entropy,
    T: Timer,
    F: Fs,
{
    use futures_util::StreamExt as _;

    if cold.is_empty() {
        return Ok(());
    }
    let total = cold.len();
    let done = std::cell::Cell::new(0usize);

    let answers: Vec<(f64, Probed)> = futures_util::stream::iter(cold.iter().map(|market_id| {
        let done = &done;
        async move {
            let outcome = probe_one(cx, pacer, *market_id).await;
            done.set(done.get() + 1);
            if let Some(report) = report {
                report(done.get(), total);
            }
            (*market_id, outcome)
        }
    }))
    .buffer_unordered(CONCURRENCY)
    .collect()
    .await;

    let mut answered = 0usize;
    for (market_id, outcome) in answers {
        cost.requests += 1;
        match outcome {
            Probed::Verdict(owned) => {
                answered += 1;
                let (access, why) = carrier::verdict(owned.docking, notoriety);
                if why == Some(Closed::Notoriety) {
                    cost.notoriety_blocked += 1;
                }
                index.set_fresh(market_id, access);
                bank(fs, cache_root, market_id, owned, now_ms, cache_policy);
            }
            Probed::Gone => {
                answered += 1;
                cost.gone += 1;
                // A carrier that does not exist cannot be docked at and cannot
                // be traded with. Restricted under any reading, and not banked
                // — the id may be reissued.
                index.set_fresh(market_id, Access::Restricted);
            }
            Probed::Failed(message) => {
                cost.probe_failures += 1;
                // Not banked: the next run should retry rather than read back a
                // hole this one dug.
                index.set(market_id, Access::Unknown);
                if !cx.out.is_json() {
                    cx.out.line(&format!(
                        "   could not read docking access for market {}: {message}",
                        js::js_number(market_id)
                    ));
                }
            }
        }
    }

    if answered == 0 {
        return Err(format!(
            "no fleet carrier answered: all {} docking-access reads failed",
            js::format_integer(total as f64)
        ));
    }
    Ok(())
}

async fn probe_one<H, C, E, J, T>(
    cx: &ProbeCx<'_, H, C, E>,
    pacer: &Pacer<'_, C, T, J>,
    market_id: f64,
) -> Probed
where
    H: HttpTransport,
    C: Clock,
    E: Entropy,
    J: Entropy,
    T: Timer,
{
    let Some(fleet_carrier_id) = carrier::carrier_id(market_id) else {
        return Probed::Failed("its market id is not a carrier's".to_owned());
    };

    let mut attempts = 0u32;
    let first_attempt_ms = pacer.now_ms();
    loop {
        attempts += 1;
        pacer.acquire().await;

        let stamp: Stamp = crate::sweep::next_stamp(
            cx.clock,
            cx.entropy,
            cx.nonce_override,
            cx.frontier_time_override,
            cx.request_time_override,
        );
        let request = game_api::prepare(
            cx.origin,
            FLEETCARRIER_INFO,
            cx.method_override,
            game_api::fleetcarrier_info_fields(
                fleet_carrier_id,
                cx.language,
                cx.credentials,
                stamp.frontier_time,
            ),
            stamp,
            cx.headers,
        );

        let exchange = crate::exchange::send(
            cx.http,
            cx.out,
            &request,
            cx.dry_run,
            crate::exchange::SendOptions {
                quiet: true,
                ignore_dry_run: false,
                // Two hundred probes each able to print a table is a different
                // thing from forty 410s inside a five-thousand-market sweep.
                // The failures are reported as a count, a note and a JSON
                // field.
                quiet_failure: true,
            },
            |_| {},
            |_| {},
        )
        .await;

        let status = exchange.as_ref().map(|e| e.status);
        let outcome = match &exchange {
            // 204 is Frontier saying there is no such carrier. Measured
            // against two synthetic ids; it is not a 4xx and not an error.
            Some(e) if e.status == 204 => return Probed::Gone,
            Some(e) if (200..300).contains(&e.status) => {
                match e.decrypted.as_deref().map(JsValue::parse) {
                    Some(Ok(document)) => match carrier::parse_info(&document, market_id) {
                        Ok(owned) => return Probed::Verdict(owned),
                        // A shape or identity refusal is not worth retrying:
                        // the same request would produce the same reply.
                        Err(refusal) => return Probed::Failed(refusal.to_string()),
                    },
                    Some(Err(error)) => Probed::Failed(error.to_string()),
                    None => Probed::Failed("the reply could not be decrypted".to_owned()),
                }
            }
            Some(e) => Probed::Failed(format!("HTTP {} {}", e.status, e.status_text)),
            None => Probed::Failed("the request did not complete".to_owned()),
        };

        let transient = crate::sweep::is_transient_status(status);
        if pacer
            .retry_after_failure(transient, attempts, first_attempt_ms)
            .await
            .is_some()
        {
            return outcome;
        }
    }
}

/// Fold in the journal and tally what the index now says.
///
/// Separate from [`probe`] so the caller can run it after a pass that sent
/// nothing — a warm cache, or the second selection on the `--verify-systems`
/// path — and still get counts that describe the verdicts actually used.
pub fn finish(
    index: &mut AccessIndex,
    market_ids: &[f64],
    commander: Option<&CommanderState>,
    cost: &mut Cost,
) {
    overlay_journal(index, commander, cost);
    cost.restricted = 0;
    cost.unknown = 0;
    for market_id in market_ids {
        match index.get(*market_id) {
            Access::Restricted => cost.restricted += 1,
            Access::Unknown => cost.unknown += 1,
            Access::Open => {}
        }
    }
}

/// The one line a docking-access pass prints.
///
/// It names the source, because this is the only fact in the run that does not
/// come from Frontier or Ardent, and it names what was *kept* unproven, because
/// that number is the size of the claim the filter is deliberately not making.
#[must_use]
pub fn note(cost: Cost, removed: Removed) -> String {
    use std::fmt::Write as _;

    let n = |value: usize| edm_core::js::format_integer(value as f64);
    let mut text = format!(
        "carrier access: {} {}",
        n(cost.carriers),
        if cost.carriers == 1 { "carrier" } else { "carriers" },
    );
    // Every clause is a number the reader might otherwise have to infer from
    // the difference between two others.
    if cost.requests > 0 {
        let _ = write!(text, ", {} read live", n(cost.requests));
    }
    if cost.cache_hits > 0 {
        let _ = write!(text, ", {} from cache", n(cost.cache_hits));
    }
    let _ = write!(text, ", {} restrict docking", n(removed.restricted));
    if cost.notoriety_blocked > 0 {
        let _ = write!(
            text,
            " ({} only because of your notoriety)",
            n(cost.notoriety_blocked)
        );
    }
    if cost.gone > 0 {
        let _ = write!(text, " ({} no longer exist)", n(cost.gone));
    }
    if cost.journal_corrections > 0 {
        let _ = write!(
            text,
            ", {} corrected by your own journal",
            n(cost.journal_corrections)
        );
    }
    if removed.unproven > 0 {
        let _ = write!(text, ", {} unread and dropped", n(removed.unproven));
    } else if removed.unproven_kept > 0 {
        let _ = write!(text, ", {} unread and kept", n(removed.unproven_kept));
    }
    text
}

/// Drop the carriers this policy will not admit, and record why.
///
/// Only carriers are touched. `considered` is deliberately left alone — it is
/// the size of what Ardent offered, and the exclusions are a ledger against it,
/// so moving both would make the plan's arithmetic stop closing.
pub fn apply(selection: &mut Selection, index: &AccessIndex, policy: Policy) -> Removed {
    let mut removed = Removed::default();
    if !policy.filters() {
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
        max_age_minutes: None,
    };

    fn owned(level: AccessLevel, notorious_ok: bool) -> carrier::Owned {
        carrier::Owned {
            docking: Docking {
                level,
                notorious_ok,
            },
            owner_squadron_id: Some(82472.0),
            owner_user_id: Some(909_522.0),
        }
    }

    /// Real carrier market ids, so the id arithmetic in `prepare` accepts them.
    const OPEN_ID: f64 = 3_705_929_472.0;
    const SHUT_ID: f64 = 3_711_014_400.0;
    const THIRD_ID: f64 = 3_703_823_104.0;

    #[test]
    fn a_verdict_survives_a_cache_round_trip() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        for (id, level) in [
            (OPEN_ID, AccessLevel::All),
            (SHUT_ID, AccessLevel::SquadronFriends),
            (THIRD_ID, AccessLevel::None),
        ] {
            bank(&fs, root, id, owned(level, true), 1_000.0, LIVE);
            let back = cached(&fs, root, id, 1_000.0, LIVE).expect("banked");
            assert_eq!(back.docking.level, level);
            assert_eq!(back.owner_squadron_id, Some(82472.0));
        }
    }

    /// The cache holds what Frontier said, never what this program concluded —
    /// so clearing notoriety takes effect on the next read rather than on the
    /// next TTL expiry.
    #[test]
    fn the_cache_stores_the_answer_and_not_the_verdict() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, false), 0.0, LIVE);

        let clean = prepare(&fs, root, &[OPEN_ID], 0.0, LIVE, 0.0);
        assert_eq!(clean.index.get(OPEN_ID), Access::Open);
        assert_eq!(clean.cost.notoriety_blocked, 0);

        let notorious = prepare(&fs, root, &[OPEN_ID], 0.0, LIVE, 3.0);
        assert_eq!(notorious.index.get(OPEN_ID), Access::Restricted);
        assert_eq!(notorious.cost.notoriety_blocked, 1);
    }

    #[test]
    fn a_verdict_older_than_the_lifetime_is_a_miss() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, true), 0.0, LIVE);
        let just_inside = LIFETIME_MINUTES * 60_000.0;
        assert!(cached(&fs, root, OPEN_ID, just_inside, LIVE).is_some());
        assert!(cached(&fs, root, OPEN_ID, just_inside + 1.0, LIVE).is_none());
    }

    /// `--max-age` can only tighten, never extend.
    #[test]
    fn max_age_tightens_the_lifetime_and_cannot_extend_it() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, true), 0.0, LIVE);
        let tight = CachePolicy {
            max_age_minutes: Some(1.0),
            ..LIVE
        };
        assert!(cached(&fs, root, OPEN_ID, 60_000.0, tight).is_some());
        assert!(cached(&fs, root, OPEN_ID, 60_001.0, tight).is_none());

        let loose = CachePolicy {
            max_age_minutes: Some(10_000.0),
            ..LIVE
        };
        let past_lifetime = LIFETIME_MINUTES * 60_000.0 + 1.0;
        assert!(cached(&fs, root, OPEN_ID, past_lifetime, loose).is_none());
    }

    #[test]
    fn a_future_timestamp_is_a_miss_not_an_extended_lifetime() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, true), 10_000.0, LIVE);
        assert!(cached(&fs, root, OPEN_ID, 0.0, LIVE).is_none());
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_not_a_crash() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        fs.write(&path(root, OPEN_ID), "{not json").unwrap();
        assert!(cached(&fs, root, OPEN_ID, 0.0, LIVE).is_none());
        fs.write(
            &path(root, SHUT_ID),
            r#"{"provider":"frontier-carrier-access","version":99,"accessLevel":"all"}"#,
        )
        .unwrap();
        assert!(cached(&fs, root, SHUT_ID, 0.0, LIVE).is_none());
    }

    /// The barrier that the Spansh reader wrote and never checked. A verdict
    /// from another provider must not be readable here whatever its shape.
    #[test]
    fn an_entry_from_another_provider_is_unreadable() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        fs.write(
            &path(root, OPEN_ID),
            r#"{"provider":"spansh-carrier-access","marketId":3705929472,"readAt":0,
                "version":1,"accessLevel":"all","notoriousAccess":true}"#,
        )
        .unwrap();
        assert!(cached(&fs, root, OPEN_ID, 0.0, LIVE).is_none());
    }

    #[test]
    fn no_cache_neither_reads_nor_writes() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        let off = CachePolicy {
            enabled: false,
            ..LIVE
        };
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, true), 0.0, off);
        assert!(fs.files.borrow().is_empty());
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, true), 0.0, LIVE);
        assert!(cached(&fs, root, OPEN_ID, 0.0, off).is_none());
    }

    #[test]
    fn refresh_writes_but_never_reads() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        let refresh = CachePolicy {
            refresh: true,
            ..LIVE
        };
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, true), 0.0, refresh);
        assert!(cached(&fs, root, OPEN_ID, 0.0, refresh).is_none());
        assert!(cached(&fs, root, OPEN_ID, 0.0, LIVE).is_some());
    }

    /// The warm/cold split is what the gate prices, so it has to be exact.
    #[test]
    fn prepare_partitions_warm_from_cold_and_prices_only_the_cold() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        bank(&fs, root, OPEN_ID, owned(AccessLevel::All, true), 0.0, LIVE);

        let prepared = prepare(&fs, root, &[OPEN_ID, SHUT_ID, THIRD_ID], 0.0, LIVE, 0.0);
        assert_eq!(prepared.cost.cache_hits, 1);
        assert_eq!(prepared.cold, vec![SHUT_ID, THIRD_ID]);
        assert_eq!(prepared.index.get(OPEN_ID), Access::Open);
        assert_eq!(prepared.index.get(SHUT_ID), Access::Unknown);
    }

    /// Ardent called it a carrier and Frontier's id space disagrees. It costs
    /// no request and it is not silently kept or silently dropped.
    #[test]
    fn a_market_id_that_is_not_a_carriers_is_unprobeable_and_unpriced() {
        let fs = MemFs::default();
        let root = Path::new("/cache");
        let prepared = prepare(&fs, root, &[128_016_384.0, OPEN_ID], 0.0, LIVE, 0.0);
        assert_eq!(prepared.cost.unprobeable, 1);
        assert_eq!(prepared.cold, vec![OPEN_ID], "the bad id costs no request");
        assert_eq!(prepared.index.get(128_016_384.0), Access::Unknown);
    }

    #[test]
    fn prepare_deduplicates_before_pricing() {
        let fs = MemFs::default();
        let prepared = prepare(
            &fs,
            Path::new("/cache"),
            &[OPEN_ID, OPEN_ID, SHUT_ID],
            0.0,
            LIVE,
            0.0,
        );
        assert_eq!(prepared.cold, vec![OPEN_ID, SHUT_ID]);
    }

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

    /// `Admitted` is the one thing Frontier cannot tell us — whether *this*
    /// commander is in the squadron the door opens for.
    #[test]
    fn a_completed_docking_beats_even_a_fresh_restriction() {
        let mut index = AccessIndex::default();
        index.set_fresh(SHUT_ID, Access::Restricted);
        let mut cost = Cost::default();
        let state = state_with(SHUT_ID, CarrierDoor::Admitted);
        overlay_journal(&mut index, Some(&state), &mut cost);
        assert_eq!(index.get(SHUT_ID), Access::Open);
        assert_eq!(cost.journal_corrections, 1);
    }

    /// A refusal from the journal is older than a probe issued this run, and
    /// loses to it — but still counts as a disagreement worth reporting.
    #[test]
    fn a_journal_refusal_loses_to_a_fresh_answer_and_beats_a_cached_one() {
        let mut fresh = AccessIndex::default();
        fresh.set_fresh(OPEN_ID, Access::Open);
        let mut cost = Cost::default();
        let state = state_with(OPEN_ID, CarrierDoor::Refused);
        overlay_journal(&mut fresh, Some(&state), &mut cost);
        assert_eq!(fresh.get(OPEN_ID), Access::Open, "the live answer is newer");
        assert_eq!(cost.journal_disagreements, 1);
        assert_eq!(cost.from_journal, 0);

        let mut warm = AccessIndex::default();
        warm.set(OPEN_ID, Access::Open);
        let mut cost = Cost::default();
        overlay_journal(&mut warm, Some(&state), &mut cost);
        assert_eq!(warm.get(OPEN_ID), Access::Restricted, "the cache is older");
        assert_eq!(cost.from_journal, 1);
    }

    fn state_with(market_id: f64, door: CarrierDoor) -> CommanderState {
        let mut state = CommanderState::default();
        state.carrier_doors.push((
            market_id as u64,
            edm_core::domain::commander::DoorObservation {
                door,
                observed_at: Some("2026-08-26T07:18:31Z".to_owned()),
            },
        ));
        state
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
