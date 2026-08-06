#![allow(
    clippy::option_option,
    reason = "cache codec must distinguish invalid input from a valid nullable field"
)]

//! Complete, deterministic snapshots of Frontier's daily populated-system digest.
//!
//! A response whose primary table has exactly 4,000 rows is not an ending: an
//! exact multiple needs one more request (which is normally an empty primary
//! page) to prove that the table is exhausted.  This module therefore keeps a
//! crawl private until a short page has been parsed successfully.  A transport
//! error, malformed page, cross-page duplicate, or the page limit discards the
//! accumulator; callers can never mistake a prefix for a snapshot.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use edm_core::domain::digest::{
    DigestError, DigestPage, DigestStatus, DigestSystem, PRIMARY_PAGE_SIZE, parse_page,
};
use edm_core::domain::id64::{self, Coordinates};
use edm_core::js::json::{JsObject, JsValue};

use crate::ports::Fs;

/// Maximum number of response pages in one crawl.
///
/// Page indices are consequently `0..PAGE_CAP`; a full page 1,023 is rejected
/// rather than causing an unbounded request for page 1,024.
pub const PAGE_CAP: usize = 1_024;

/// Default lifetime of a complete daily snapshot: 24 hours.
pub const DEFAULT_MAX_AGE_MS: f64 = 24.0 * 60.0 * 60.0 * 1_000.0;

const FORMAT_VERSION: u32 = 1;

/// A crawl which has proved its terminal page.
///
/// The fields are deliberately private.  In particular, a `Vec<DigestSystem>`
/// cannot be promoted to this type by a caller which fetched only a prefix.
#[derive(Clone, Debug, PartialEq)]
pub struct Crawl {
    systems: Vec<DigestSystem>,
    terminal_page: usize,
}

impl Crawl {
    #[must_use]
    pub fn systems(&self) -> &[DigestSystem] {
        &self.systems
    }

    /// Zero-based index of the page whose primary row count was below 4,000.
    #[must_use]
    pub fn terminal_page(&self) -> usize {
        self.terminal_page
    }

    /// Attach the ambient observation metadata after the complete crawl.
    #[must_use]
    pub fn into_snapshot(self, source: impl Into<String>, read_at_ms: f64) -> Snapshot {
        Snapshot {
            systems: self.systems,
            terminal_page: self.terminal_page,
            read_at_ms,
            source: source.into(),
        }
    }
}

/// A complete normalized digest plus the provenance of the observation.
///
/// Systems are always sorted by integer address.  Construction is restricted
/// to a successful [`crawl`] or a fully validated cache entry, preserving both
/// that ordering and the terminal-page proof.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    systems: Vec<DigestSystem>,
    terminal_page: usize,
    read_at_ms: f64,
    source: String,
}

impl Snapshot {
    #[must_use]
    pub fn systems(&self) -> &[DigestSystem] {
        &self.systems
    }

    #[must_use]
    pub fn terminal_page(&self) -> usize {
        self.terminal_page
    }

    #[must_use]
    pub fn read_at_ms(&self) -> f64 {
        self.read_at_ms
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Systems no farther than `radius_ly`, ordered by exact distance and then
    /// integer address.  The address tie-break means the answer is independent
    /// of page order and of an unstable sort's treatment of equal distances.
    #[must_use]
    pub fn within_radius(&self, centre: Coordinates, radius_ly: f64) -> Vec<RadiusSystem<'_>> {
        if radius_ly.is_nan() || radius_ly < 0.0 {
            return Vec::new();
        }

        let mut found = Vec::new();
        for system in &self.systems {
            let dx = system.coordinates.x - centre.x;
            let dy = system.coordinates.y - centre.y;
            let dz = system.coordinates.z - centre.z;
            let distance_ly = (dx * dx + dy * dy + dz * dz).sqrt();
            if distance_ly <= radius_ly {
                found.push(RadiusSystem {
                    system,
                    distance_ly,
                });
            }
        }
        found.sort_by(|left, right| {
            left.distance_ly
                .total_cmp(&right.distance_ly)
                .then_with(|| left.system.address.cmp(&right.system.address))
        });
        found
    }

    /// Short spelling for [`Snapshot::within_radius`].
    #[must_use]
    pub fn radius(&self, centre: Coordinates, radius_ly: f64) -> Vec<RadiusSystem<'_>> {
        self.within_radius(centre, radius_ly)
    }
}

/// One result of a radius lookup.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadiusSystem<'a> {
    pub system: &'a DigestSystem,
    pub distance_ly: f64,
}

/// Why no complete crawl could be produced.
#[derive(Debug)]
pub enum CrawlError<E> {
    Fetch {
        page: usize,
        source: E,
    },
    Parse {
        page: usize,
        source: DigestError,
    },
    DuplicateAddress {
        address: u64,
        first_page: usize,
        page: usize,
    },
    PageCap {
        cap: usize,
    },
}

impl<E: fmt::Display> fmt::Display for CrawlError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch { page, source } => {
                write!(
                    formatter,
                    "daily-digest page {page} could not be fetched: {source}"
                )
            }
            Self::Parse { page, source } => {
                write!(formatter, "daily-digest page {page} was rejected: {source}")
            }
            Self::DuplicateAddress {
                address,
                first_page,
                page,
            } => write!(
                formatter,
                "daily-digest system address {address} occurs on pages {first_page} and {page}",
            ),
            Self::PageCap { cap } => {
                write!(
                    formatter,
                    "daily-digest crawl did not terminate within {cap} pages"
                )
            }
        }
    }
}

impl<E: Error + 'static> Error for CrawlError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Fetch { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::DuplicateAddress { .. } | Self::PageCap { .. } => None,
        }
    }
}

/// Fetch, strictly parse, and merge pages until a short primary page proves
/// completion.
///
/// `fetch_page` is invoked with monotonically increasing zero-based indices.
/// Its successful value can be either an owned `String` or any other value
/// implementing `AsRef<str>`.  No accumulated rows are present in any error.
pub async fn crawl<Fetch, FetchFuture, Body, FetchError>(
    mut fetch_page: Fetch,
) -> Result<Crawl, CrawlError<FetchError>>
where
    Fetch: FnMut(usize) -> FetchFuture,
    FetchFuture: Future<Output = Result<Body, FetchError>>,
    Body: AsRef<str>,
{
    let mut merger = Merger::default();

    for page in 0..PAGE_CAP {
        let document = fetch_page(page)
            .await
            .map_err(|source| CrawlError::Fetch { page, source })?;
        let parsed =
            parse_page(document.as_ref()).map_err(|source| CrawlError::Parse { page, source })?;
        if let Some(complete) = merger.absorb(page, parsed)? {
            return Ok(complete);
        }
    }

    // `Merger::absorb` rejects a full last allowed page, so this is not
    // reachable.  Keeping the fallback makes the bound explicit if its policy
    // changes later.
    Err(CrawlError::PageCap { cap: PAGE_CAP })
}

#[derive(Debug, Default)]
struct Merger {
    systems: Vec<DigestSystem>,
    /// Address -> page, both for rejection and for a useful diagnostic.
    seen: HashMap<u64, usize>,
}

impl Merger {
    fn absorb<E>(
        &mut self,
        page_index: usize,
        page: DigestPage,
    ) -> Result<Option<Crawl>, CrawlError<E>> {
        // Do not append anything until every address on this page has passed.
        // Thus even this private accumulator stays at its last complete-page
        // boundary when a duplicate is found.
        for system in &page.systems {
            if let Some(&first_page) = self.seen.get(&system.address) {
                return Err(CrawlError::DuplicateAddress {
                    address: system.address,
                    first_page,
                    page: page_index,
                });
            }
        }
        for system in page.systems {
            self.seen.insert(system.address, page_index);
            self.systems.push(system);
        }

        if page.primary_rows < PRIMARY_PAGE_SIZE {
            self.systems.sort_unstable_by_key(|system| system.address);
            return Ok(Some(Crawl {
                systems: std::mem::take(&mut self.systems),
                terminal_page: page_index,
            }));
        }
        if page_index + 1 == PAGE_CAP {
            return Err(CrawlError::PageCap { cap: PAGE_CAP });
        }
        Ok(None)
    }
}

/// The on-disk complete-snapshot cache.
#[derive(Clone, Debug)]
pub struct Cache {
    frontier: PathBuf,
    max_age_ms: f64,
    enabled: bool,
    refresh: bool,
}

impl Cache {
    /// Store below `<cache_root>/frontier/daily-digest.json`, with a 24-hour
    /// lifetime.
    #[must_use]
    pub fn new(cache_root: impl AsRef<Path>, enabled: bool, refresh: bool) -> Self {
        Self::with_max_age_ms(cache_root, DEFAULT_MAX_AGE_MS, enabled, refresh)
    }

    /// Constructor with an explicit lifetime, primarily for policy wiring and
    /// deterministic tests.
    #[must_use]
    pub fn with_max_age_ms(
        cache_root: impl AsRef<Path>,
        max_age_ms: f64,
        enabled: bool,
        refresh: bool,
    ) -> Self {
        Self {
            frontier: cache_root.as_ref().join("frontier"),
            max_age_ms,
            enabled,
            refresh,
        }
    }

    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.frontier.join("daily-digest.json")
    }

    /// Return a fresh snapshot produced by exactly `expected_source`.
    ///
    /// Missing, malformed, wrong-version, wrong-source, future-dated and
    /// expired entries all deliberately collapse to a cache miss.
    #[must_use]
    pub fn get<F: Fs>(&self, fs: &F, expected_source: &str, now_ms: f64) -> Option<Snapshot> {
        if !self.enabled
            || self.refresh
            || !now_ms.is_finite()
            || !self.max_age_ms.is_finite()
            || self.max_age_ms < 0.0
        {
            return None;
        }
        let text = fs.read_to_string(&self.path()).ok()?;
        let snapshot = decode(&text, expected_source)?;
        let age_ms = now_ms - snapshot.read_at_ms;
        if !age_ms.is_finite() || age_ms < 0.0 || age_ms > self.max_age_ms {
            return None;
        }
        Some(snapshot)
    }

    /// Bank one already-complete snapshot.
    ///
    /// `Fs` has no rename operation, so this cannot use the usual write-temp /
    /// atomic-rename protocol.  An interrupted direct write can leave a
    /// truncated file; [`Cache::get`] treats that as a miss.  This is why the
    /// cache is only an optimisation, and why [`Cache::get_or_crawl`] does not
    /// call this method until the terminal page has been validated.
    ///
    /// The boolean reports whether a write actually completed.  Failure never
    /// changes the successful network result.
    pub fn put<F: Fs>(&self, fs: &F, snapshot: &Snapshot) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(text) = encode(snapshot) else {
            return false;
        };
        if fs.create_dir_all(&self.frontier).is_err() {
            return false;
        }
        fs.write(&self.path(), &text).is_ok()
    }

    /// Use a fresh cache hit or perform and bank one complete crawl.
    ///
    /// In refresh mode the read is skipped but a successful crawl is still
    /// written.  With caching disabled neither operation touches the cache.
    /// Most importantly, `?` returns before [`Cache::put`] on *every* crawl
    /// error, so a failed run can never replace the last complete snapshot by
    /// its prefix.
    pub async fn get_or_crawl<FsImpl, Fetch, FetchFuture, Body, FetchError>(
        &self,
        fs: &FsImpl,
        source: &str,
        now_ms: f64,
        fetch_page: Fetch,
    ) -> Result<Snapshot, CrawlError<FetchError>>
    where
        FsImpl: Fs,
        Fetch: FnMut(usize) -> FetchFuture,
        FetchFuture: Future<Output = Result<Body, FetchError>>,
        Body: AsRef<str>,
    {
        if let Some(snapshot) = self.get(fs, source, now_ms) {
            return Ok(snapshot);
        }
        let complete = crawl(fetch_page).await?;
        let snapshot = complete.into_snapshot(source, now_ms);
        let _ = self.put(fs, &snapshot);
        Ok(snapshot)
    }
}

fn encode(snapshot: &Snapshot) -> Option<String> {
    if !snapshot.read_at_ms.is_finite()
        || snapshot.terminal_page >= PAGE_CAP
        || !strictly_address_sorted(&snapshot.systems)
    {
        return None;
    }

    let systems = snapshot
        .systems
        .iter()
        .map(encode_system)
        .collect::<Option<Vec<_>>>()?;
    let entry = object(vec![
        ("version", JsValue::Num(f64::from(FORMAT_VERSION))),
        (
            "source",
            JsValue::Str(snapshot.source.clone().into_boxed_str()),
        ),
        ("readAt", JsValue::Num(snapshot.read_at_ms)),
        ("terminal", JsValue::Num(snapshot.terminal_page as f64)),
        ("systems", JsValue::Arr(systems)),
    ]);
    Some(JsValue::Obj(entry).stringify_compact())
}

fn encode_system(system: &DigestSystem) -> Option<JsValue> {
    let coordinates = system.coordinates;
    if system.address == 0
        || !coordinates.x.is_finite()
        || !coordinates.y.is_finite()
        || !coordinates.z.is_finite()
    {
        return None;
    }
    let parts = id64::decode(system.address as f64).ok()?;
    if !id64::contains(&parts, coordinates) {
        return None;
    }

    Some(JsValue::Obj(object(vec![
        ("address", u64_json(system.address)),
        (
            "coordinates",
            JsValue::Obj(object(vec![
                ("x", JsValue::Num(coordinates.x)),
                ("y", JsValue::Num(coordinates.y)),
                ("z", JsValue::Num(coordinates.z)),
            ])),
        ),
        ("status", encode_status(&system.status)?),
    ])))
}

fn encode_status(status: &DigestStatus) -> Option<JsValue> {
    Some(JsValue::Obj(object(vec![
        ("factionId", optional_u32_json(status.faction_id)),
        ("minorFactionId", optional_u64_json(status.minor_faction_id)),
        ("governmentId", optional_u32_json(status.government_id)),
        (
            "developmentLevel",
            optional_u32_json(status.development_level),
        ),
        (
            "standardOfLiving",
            optional_u32_json(status.standard_of_living),
        ),
        ("population", optional_u64_json(status.population)),
        ("systemValue", optional_f64_json(status.system_value)?),
        ("techLevel", optional_f64_json(status.tech_level)?),
        (
            "economies",
            status.economies.map_or(JsValue::Null, |values| {
                JsValue::Arr(values.into_iter().map(optional_u32_json).collect())
            }),
        ),
        (
            "state",
            status.state.as_ref().map_or(JsValue::Null, |value| {
                JsValue::Str(value.clone().into_boxed_str())
            }),
        ),
        (
            "starsystemId",
            status
                .starsystem_id
                .as_ref()
                .map_or(JsValue::Null, |value| {
                    JsValue::Str(value.clone().into_boxed_str())
                }),
        ),
        (
            "twRescueMarket",
            status.tw_rescue_market.map_or(JsValue::Null, JsValue::Bool),
        ),
        (
            "oldMinorFactionIds",
            status
                .old_minor_faction_ids
                .as_ref()
                .map_or(JsValue::Null, |values| {
                    JsValue::Arr(values.iter().copied().map(u64_json).collect())
                }),
        ),
        ("powerId", optional_u32_json(status.power_id)),
        (
            "powerState",
            status.power_state.as_ref().map_or(JsValue::Null, |value| {
                JsValue::Str(value.clone().into_boxed_str())
            }),
        ),
        ("securityLevel", optional_u32_json(status.security_level)),
    ])))
}

fn decode(text: &str, expected_source: &str) -> Option<Snapshot> {
    let JsValue::Obj(entry) = JsValue::parse(text).ok()? else {
        return None;
    };
    if entry.len() != 5 || entry.get("version") != Some(&JsValue::Num(f64::from(FORMAT_VERSION))) {
        return None;
    }
    let JsValue::Str(source) = entry.get("source")? else {
        return None;
    };
    if source.as_ref() != expected_source {
        return None;
    }
    let JsValue::Num(read_at_ms) = *entry.get("readAt")? else {
        return None;
    };
    if !read_at_ms.is_finite() {
        return None;
    }
    let terminal_page = usize_json(entry.get("terminal")?)?;
    if terminal_page >= PAGE_CAP {
        return None;
    }
    let JsValue::Arr(stored_systems) = entry.get("systems")? else {
        return None;
    };
    let mut systems = stored_systems
        .iter()
        .map(decode_system)
        .collect::<Option<Vec<_>>>()?;
    systems.sort_unstable_by_key(|system| system.address);
    if !strictly_address_sorted(&systems) {
        return None;
    }

    Some(Snapshot {
        systems,
        terminal_page,
        read_at_ms,
        source: source.to_string(),
    })
}

fn decode_system(value: &JsValue) -> Option<DigestSystem> {
    let JsValue::Obj(system) = value else {
        return None;
    };
    if system.len() != 3 {
        return None;
    }
    let address = parse_u64_json(system.get("address")?)?;
    if address == 0 {
        return None;
    }
    let JsValue::Obj(coordinates) = system.get("coordinates")? else {
        return None;
    };
    if coordinates.len() != 3 {
        return None;
    }
    let coordinates = Coordinates {
        x: finite_number(coordinates.get("x")?)?,
        y: finite_number(coordinates.get("y")?)?,
        z: finite_number(coordinates.get("z")?)?,
    };
    let parts = id64::decode(address as f64).ok()?;
    if !id64::contains(&parts, coordinates) {
        return None;
    }
    let status = decode_status(system.get("status")?)?;
    Some(DigestSystem {
        address,
        coordinates,
        status,
    })
}

fn decode_status(value: &JsValue) -> Option<DigestStatus> {
    let JsValue::Obj(status) = value else {
        return None;
    };
    if status.len() != 16 {
        return None;
    }
    Some(DigestStatus {
        faction_id: optional_u32(status.get("factionId")?)?,
        minor_faction_id: optional_u64(status.get("minorFactionId")?)?,
        government_id: optional_u32(status.get("governmentId")?)?,
        development_level: optional_u32(status.get("developmentLevel")?)?,
        standard_of_living: optional_u32(status.get("standardOfLiving")?)?,
        population: optional_u64(status.get("population")?)?,
        system_value: optional_f64(status.get("systemValue")?)?,
        tech_level: optional_f64(status.get("techLevel")?)?,
        economies: match status.get("economies")? {
            JsValue::Null => None,
            JsValue::Arr(values) if values.len() == 2 => {
                Some([optional_u32(&values[0])?, optional_u32(&values[1])?])
            }
            _ => return None,
        },
        state: optional_string(status.get("state")?)?,
        starsystem_id: optional_string(status.get("starsystemId")?)?,
        tw_rescue_market: optional_bool(status.get("twRescueMarket")?)?,
        old_minor_faction_ids: match status.get("oldMinorFactionIds")? {
            JsValue::Null => None,
            JsValue::Arr(values) => Some(
                values
                    .iter()
                    .map(parse_u64_json)
                    .collect::<Option<Vec<_>>>()?,
            ),
            _ => return None,
        },
        power_id: optional_u32(status.get("powerId")?)?,
        power_state: optional_string(status.get("powerState")?)?,
        security_level: optional_u32(status.get("securityLevel")?)?,
    })
}

fn object(entries: Vec<(&str, JsValue)>) -> JsObject {
    JsObject::from_document_order(
        entries
            .into_iter()
            .map(|(key, value)| (Box::<str>::from(key), value))
            .collect(),
    )
}

fn u64_json(value: u64) -> JsValue {
    // Addresses and faction ids are strings on disk: converting through the
    // program's JavaScript-number JSON model would round values above 2^53.
    JsValue::Str(value.to_string().into_boxed_str())
}

fn optional_u64_json(value: Option<u64>) -> JsValue {
    value.map_or(JsValue::Null, u64_json)
}

fn optional_u32_json(value: Option<u32>) -> JsValue {
    value.map_or(JsValue::Null, |number| JsValue::Num(f64::from(number)))
}

fn optional_f64_json(value: Option<f64>) -> Option<JsValue> {
    match value {
        None => Some(JsValue::Null),
        Some(number) if number.is_finite() => Some(JsValue::Num(number)),
        Some(_) => None,
    }
}

fn parse_u64_json(value: &JsValue) -> Option<u64> {
    let JsValue::Str(text) = value else {
        return None;
    };
    let number = text.parse::<u64>().ok()?;
    (number.to_string() == text.as_ref()).then_some(number)
}

fn finite_number(value: &JsValue) -> Option<f64> {
    let JsValue::Num(number) = *value else {
        return None;
    };
    number.is_finite().then_some(number)
}

fn usize_json(value: &JsValue) -> Option<usize> {
    let number = finite_number(value)?;
    (number >= 0.0 && number.fract() == 0.0 && number <= usize::MAX as f64)
        .then_some(number as usize)
}

fn optional_u32(value: &JsValue) -> Option<Option<u32>> {
    match value {
        JsValue::Null => Some(None),
        JsValue::Num(number)
            if number.is_finite()
                && number.fract() == 0.0
                && *number >= 0.0
                && *number <= f64::from(u32::MAX) =>
        {
            Some(Some(*number as u32))
        }
        _ => None,
    }
}

fn optional_u64(value: &JsValue) -> Option<Option<u64>> {
    match value {
        JsValue::Null => Some(None),
        _ => Some(Some(parse_u64_json(value)?)),
    }
}

fn optional_f64(value: &JsValue) -> Option<Option<f64>> {
    match value {
        JsValue::Null => Some(None),
        _ => Some(Some(finite_number(value)?)),
    }
}

fn optional_string(value: &JsValue) -> Option<Option<String>> {
    match value {
        JsValue::Null => Some(None),
        JsValue::Str(text) => Some(Some(text.to_string())),
        _ => None,
    }
}

fn optional_bool(value: &JsValue) -> Option<Option<bool>> {
    match value {
        JsValue::Null => Some(None),
        JsValue::Bool(value) => Some(Some(*value)),
        _ => None,
    }
}

fn strictly_address_sorted(systems: &[DigestSystem]) -> bool {
    systems
        .windows(2)
        .all(|pair| pair[0].address < pair[1].address)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::convert::Infallible;
    use std::path::Path;
    use std::rc::Rc;

    use super::*;
    use crate::ports::RecordingFs;

    const SOL: u64 = 10_477_373_803;
    const SOURCE: &str = "https://frontier.example/dailydigest_part";

    fn envelope(entries: &str) -> String {
        format!(r#"{{"systems":{{{entries}}}}}"#)
    }

    fn sol_row(extra_status: &str) -> String {
        format!(r#""{SOL}":{{"systemAddr":{SOL},"x":49985,"y":40985,"z":24105{extra_status}}}"#)
    }

    fn sentinel_row() -> &'static str {
        r#""":{"factionId":0,"minorfactionId":0,"governmentId":0,"developmentLevel":60,"standardOfLiving":50,"population":900000,"systemValue":40,"techLevel":50,"economies":[0,null],"state":"","starsystem_id":"9999999","systemAddr":0,"tw_rescueMarket":false,"oldMinorFactionIDs":[],"x":1000,"y":-999,"z":-999,"securityLevel":60}"#
    }

    fn full_page(last: Option<&str>) -> String {
        let sentinel_count = PRIMARY_PAGE_SIZE - usize::from(last.is_some());
        let mut entries = Vec::with_capacity(PRIMARY_PAGE_SIZE);
        entries.extend(std::iter::repeat_n(sentinel_row(), sentinel_count));
        if let Some(last) = last {
            entries.push(last);
        }
        envelope(&entries.join(","))
    }

    fn snapshot(read_at_ms: f64, source: &str) -> Snapshot {
        let document = envelope(&sol_row(
            r#", "minorfactionId":72060832334024995,"population":0,"economies":[7,null]"#,
        ));
        let parsed = parse_page(&document).expect("valid Sol page");
        Crawl {
            systems: parsed.systems,
            terminal_page: 0,
        }
        .into_snapshot(source, read_at_ms)
    }

    #[tokio::test]
    async fn an_exact_multiple_fetches_the_extra_terminal_page() {
        let pages = [full_page(None), envelope("")];
        let calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);

        let complete = crawl(move |page| {
            recorded.borrow_mut().push(page);
            std::future::ready(Ok::<_, Infallible>(pages[page].clone()))
        })
        .await
        .expect("the empty extra page terminates");

        assert_eq!(&*calls.borrow(), &[0, 1]);
        assert_eq!(complete.terminal_page(), 1);
        assert!(complete.systems().is_empty());
    }

    #[tokio::test]
    async fn a_short_first_page_ends_immediately() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&calls);
        let page = envelope(&sol_row(""));

        let complete = crawl(move |index| {
            recorded.borrow_mut().push(index);
            std::future::ready(Ok::<_, Infallible>(page.clone()))
        })
        .await
        .expect("one rich row is terminal");

        assert_eq!(&*calls.borrow(), &[0]);
        assert_eq!(complete.systems()[0].address, SOL);
        assert_eq!(complete.terminal_page(), 0);
    }

    #[tokio::test]
    async fn a_malformed_page_discards_the_crawl() {
        let malformed = envelope(&format!(r#""{SOL}":{{"systemAddr":{SOL},"x":49985}}"#));
        let error = crawl(move |_| std::future::ready(Ok::<_, Infallible>(malformed.clone())))
            .await
            .expect_err("partial coordinates are not a page");

        assert!(matches!(error, CrawlError::Parse { page: 0, .. }));
    }

    #[tokio::test]
    async fn a_rich_address_repeated_on_a_later_page_rejects_everything() {
        let sol = sol_row("");
        let pages = [full_page(Some(&sol)), envelope(&sol)];

        let error = crawl(move |page| std::future::ready(Ok::<_, Infallible>(pages[page].clone())))
            .await
            .expect_err("a cross-page duplicate is not merged");

        assert!(matches!(
            error,
            CrawlError::DuplicateAddress {
                address: SOL,
                first_page: 0,
                page: 1
            }
        ));
    }

    #[test]
    fn a_never_short_crawl_stops_at_the_page_cap() {
        let mut merger = Merger::default();
        for page in 0..PAGE_CAP {
            let result: Result<Option<Crawl>, CrawlError<()>> = merger.absorb(
                page,
                DigestPage {
                    systems: Vec::new(),
                    primary_rows: PRIMARY_PAGE_SIZE,
                    overlay_rows: 0,
                    sentinel_rows: PRIMARY_PAGE_SIZE,
                },
            );
            if page + 1 == PAGE_CAP {
                assert!(matches!(result, Err(CrawlError::PageCap { cap: PAGE_CAP })));
            } else {
                assert!(matches!(result, Ok(None)));
            }
        }
    }

    #[test]
    fn merging_normalizes_systems_into_address_order() {
        let status = DigestStatus::default();
        let system = |address| DigestSystem {
            address,
            coordinates: Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            status: status.clone(),
        };
        let mut merger = Merger::default();
        let complete: Result<Option<Crawl>, CrawlError<()>> = merger.absorb(
            0,
            DigestPage {
                systems: vec![system(30), system(10), system(20)],
                primary_rows: 3,
                overlay_rows: 0,
                sentinel_rows: 0,
            },
        );
        let complete = complete.expect("unique").expect("short");
        assert_eq!(
            complete
                .systems()
                .iter()
                .map(|system| system.address)
                .collect::<Vec<_>>(),
            [10, 20, 30]
        );
    }

    #[test]
    fn cache_round_trip_is_lossless_for_u64_status_and_coordinates() {
        let fs = RecordingFs::default();
        let cache = Cache::new(Path::new("/cache"), true, false);
        let original = snapshot(1_000.0, SOURCE);

        assert!(cache.put(&fs, &original));
        let stored = fs.read_to_string(&cache.path()).expect("written");
        assert!(stored.contains(r#""minorFactionId":"72060832334024995""#));
        assert!(stored.contains(r#""address":"10477373803""#));
        assert_eq!(cache.get(&fs, SOURCE, 2_000.0), Some(original));
        assert!(cache.path().ends_with("frontier/daily-digest.json"));
    }

    #[test]
    fn future_expired_and_wrong_source_entries_are_misses() {
        let fs = RecordingFs::default();
        let cache = Cache::new(Path::new("/cache"), true, false);
        let original = snapshot(1_000.0, SOURCE);
        assert!(cache.put(&fs, &original));

        assert!(cache.get(&fs, SOURCE, 999.0).is_none(), "future-dated");
        assert!(
            cache
                .get(&fs, SOURCE, 1_000.0 + DEFAULT_MAX_AGE_MS)
                .is_some(),
            "the TTL bound is inclusive"
        );
        assert!(
            cache
                .get(&fs, SOURCE, 1_000.0 + DEFAULT_MAX_AGE_MS + 1.0)
                .is_none(),
            "past the TTL"
        );
        assert!(cache.get(&fs, "a different endpoint", 2_000.0).is_none());
    }

    #[test]
    fn corrupt_and_other_version_entries_are_misses() {
        let fs = RecordingFs::default();
        let cache = Cache::new(Path::new("/cache"), true, false);
        fs.write(&cache.path(), "{truncated").expect("in memory");
        assert!(cache.get(&fs, SOURCE, 1.0).is_none());

        let valid = encode(&snapshot(0.0, SOURCE)).expect("encodable");
        let other_version = valid.replacen(r#""version":1"#, r#""version":2"#, 1);
        fs.write(&cache.path(), &other_version).expect("in memory");
        assert!(cache.get(&fs, SOURCE, 1.0).is_none());
    }

    #[test]
    fn refresh_reads_nothing_but_writes_and_no_cache_does_neither() {
        let fs = RecordingFs::default();
        let refreshing = Cache::new(Path::new("/cache"), true, true);
        let original = snapshot(1_000.0, SOURCE);
        assert!(refreshing.put(&fs, &original));
        assert!(refreshing.get(&fs, SOURCE, 1_000.0).is_none());
        assert!(
            Cache::new(Path::new("/cache"), true, false)
                .get(&fs, SOURCE, 1_000.0)
                .is_some(),
            "the next ordinary run is warm"
        );

        let off = Cache::new(Path::new("/off"), false, false);
        assert!(!off.put(&fs, &original));
        assert!(off.get(&fs, SOURCE, 1_000.0).is_none());
        assert!(!fs.exists(&off.path()));
    }

    #[tokio::test]
    async fn failed_cached_crawl_never_writes_a_partial_snapshot() {
        let fs = RecordingFs::default();
        let cache = Cache::new(Path::new("/cache"), true, false);
        let malformed = envelope(&format!(r#""{SOL}":{{"systemAddr":{SOL},"x":49985}}"#));

        let result = cache
            .get_or_crawl(&fs, SOURCE, 1_000.0, move |_| {
                std::future::ready(Ok::<_, Infallible>(malformed.clone()))
            })
            .await;

        assert!(matches!(result, Err(CrawlError::Parse { .. })));
        assert!(!fs.exists(&cache.path()), "put was never called");
        assert!(fs.0.borrow().is_empty(), "RecordingFs saw no writes");
    }

    #[test]
    fn radius_results_are_deterministic_by_distance_then_address() {
        let status = DigestStatus::default();
        let system = |address, x, y| DigestSystem {
            address,
            coordinates: Coordinates { x, y, z: 0.0 },
            status: status.clone(),
        };
        // Address-sorted, as every real Snapshot is.  Address 20 is farther,
        // proving the query is not merely preserving snapshot order.
        let snapshot = Crawl {
            systems: vec![
                system(10, -1.0, 0.0),
                system(20, 0.0, 2.0),
                system(30, 1.0, 0.0),
                system(40, 4.0, 0.0),
            ],
            terminal_page: 0,
        }
        .into_snapshot("test", 0.0);

        let found = snapshot.within_radius(
            Coordinates {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            2.0,
        );
        assert_eq!(
            found
                .iter()
                .map(|row| row.system.address)
                .collect::<Vec<_>>(),
            [10, 30, 20]
        );
        assert_eq!(
            found.iter().map(|row| row.distance_ly).collect::<Vec<_>>(),
            [1.0, 1.0, 2.0]
        );
        assert!(
            snapshot
                .radius(
                    Coordinates {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0
                    },
                    -1.0
                )
                .is_empty()
        );
    }
}
