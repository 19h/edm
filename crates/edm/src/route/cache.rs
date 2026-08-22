//! A resumable on-disk cache of market listings.
//!
//! A 1,300-market sweep is fifteen minutes and 1,300 authenticated requests.
//! Losing it to a dropped connection, or paying for it again because the user
//! wanted to re-run with a different `--cargo`, is the difference between a
//! tool that can be used and one that can be demonstrated. So prices are kept,
//! keyed by market, with the instant they were read.
//!
//! Two properties matter more than speed here:
//!
//! - **A stale entry is never silently used.** Every read is against an age
//!   bound, and what the bound excluded is counted, so the plan can say how
//!   many of its requests exist only because the cache had gone cold.
//! - **A corrupt entry is a miss, not a crash.** Half-written JSON from an
//!   interrupted run must cost one request, not the whole sweep.
//!
//! One file per market rather than one file for the region: a sweep that is
//! killed halfway has still banked everything it read, and two runs over
//! overlapping regions share what they have in common without either having to
//! know about the other.

use std::path::{Path, PathBuf};

use edm_core::js;
use edm_core::js::json::{JsObject, JsValue};

use crate::ports::Fs;

/// Where the cache lives, and how old an entry may be.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
    max_age_ms: f64,
    /// `--no-cache`: read nothing, write nothing.
    enabled: bool,
    /// `--refresh`: write everything, read nothing. Distinct from disabling,
    /// because a refresh should still leave the next run warm.
    refresh: bool,
}

/// What a lookup pass established, for the plan and the coverage tables.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Hits {
    pub fresh: usize,
    pub stale: usize,
    pub missing: usize,
    /// Entries that were present but unreadable. Counted apart from `missing`
    /// because a nonzero value here means something is wrong with the cache
    /// directory, not merely that it is cold.
    pub corrupt: usize,
}

impl Cache {
    /// `$XDG_CACHE_HOME/edm/route`, or `$HOME/.cache/edm/route`.
    ///
    /// Falls back to a relative path rather than failing: a cache that cannot
    /// find a home should degrade to not caching, never to not running.
    #[must_use]
    pub fn locate(
        xdg_cache_home: Option<&str>,
        home: Option<&str>,
        explicit: Option<&str>,
    ) -> PathBuf {
        if let Some(path) = explicit {
            return PathBuf::from(path);
        }
        if let Some(base) = xdg_cache_home.filter(|value| !value.is_empty()) {
            return Path::new(base).join("edm").join("route");
        }
        if let Some(base) = home.filter(|value| !value.is_empty()) {
            return Path::new(base).join(".cache").join("edm").join("route");
        }
        PathBuf::from(".edm-cache").join("route")
    }

    #[must_use]
    pub fn new(root: PathBuf, max_age_minutes: f64, enabled: bool, refresh: bool) -> Self {
        Self {
            root,
            max_age_ms: max_age_minutes * 60_000.0,
            enabled,
            refresh,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// One file per market, named by id.
    ///
    /// The id is formatted through `js_number` so the name is the same decimal
    /// text the program prints and the wire carries — a market cached by one
    /// path and looked up by another would be a silent permanent miss.
    fn path(&self, market_id: f64) -> PathBuf {
        self.root
            .join(PROVIDER_NAMESPACE)
            .join(format!("{}.json", js::js_number(market_id)))
    }

    /// A cached listing, if there is a fresh one.
    ///
    /// Returns the document and the instant it was read, so the caller can
    /// report an age rather than implying the price is current.
    pub fn get<F: Fs>(&self, fs: &F, market_id: f64, now_ms: f64) -> Lookup {
        if !self.enabled || self.refresh {
            return Lookup::Skipped;
        }
        let Ok(text) = fs.read_to_string(&self.path(market_id)) else {
            return Lookup::Missing;
        };
        let Some(entry) = decode(&text) else {
            return Lookup::Corrupt;
        };
        let age_ms = now_ms - entry.read_at_ms;
        // A future timestamp is not a fresh observation. It is either corrupt
        // data or a clock anomaly, and accepting it would extend the cache
        // lifetime beyond the configured freshness bound.
        if !age_ms.is_finite() || age_ms < 0.0 {
            return Lookup::Corrupt;
        }
        if age_ms > self.max_age_ms {
            return Lookup::Stale { age_ms };
        }
        Lookup::Fresh(entry)
    }

    /// Bank a listing. Failures are silent by design: a cache that cannot be
    /// written is a lost optimisation, not a lost sweep.
    pub fn put<F: Fs>(&self, fs: &F, market_id: f64, document: &JsValue, now_ms: f64) {
        if !self.enabled {
            return;
        }
        let provider_root = self.root.join(PROVIDER_NAMESPACE);
        if fs.create_dir_all(&provider_root).is_err() {
            return;
        }
        let entry = JsObject::from_document_order(vec![
            ("provider".into(), JsValue::Str(PROVIDER_NAMESPACE.into())),
            ("marketId".into(), JsValue::Num(market_id)),
            ("readAt".into(), JsValue::Num(now_ms)),
            ("version".into(), JsValue::Num(f64::from(FORMAT_VERSION))),
            ("payload".into(), document.clone()),
        ]);
        let _ = fs.write(
            &self.path(market_id),
            &JsValue::Obj(entry).stringify_compact(),
        );
    }
}

/// Bumped whenever the stored shape changes. An entry from an older version is
/// a miss rather than a parse error, so an upgrade costs one sweep instead of
/// requiring anyone to remember to clear a directory.
const FORMAT_VERSION: u32 = 2;
/// Cache namespace for authoritative quantity-aware `/market/list` quotes.
/// Candidate marketdata and future providers must use different roots.
const PROVIDER_NAMESPACE: &str = "frontier-market-list";

/// One banked listing.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub market_id: f64,
    pub read_at_ms: f64,
    pub payload: JsValue,
}

/// What a lookup found.
#[derive(Clone, Debug, PartialEq)]
pub enum Lookup {
    Fresh(Entry),
    /// Present, but older than `--max-age`.
    Stale {
        age_ms: f64,
    },
    Missing,
    /// Present and unreadable — truncated, or from an older format version.
    Corrupt,
    /// `--no-cache` or `--refresh`: not consulted at all.
    Skipped,
}

impl Lookup {
    /// Fold this lookup into a running count.
    pub fn tally(&self, hits: &mut Hits) {
        match self {
            Self::Fresh(_) => hits.fresh += 1,
            Self::Stale { .. } => hits.stale += 1,
            Self::Missing | Self::Skipped => hits.missing += 1,
            Self::Corrupt => hits.corrupt += 1,
        }
    }

    #[must_use]
    pub fn entry(self) -> Option<Entry> {
        match self {
            Self::Fresh(entry) => Some(entry),
            _ => None,
        }
    }
}

/// Parse a stored entry, treating anything unexpected as absent.
fn decode(text: &str) -> Option<Entry> {
    let JsValue::Obj(object) = JsValue::parse(text).ok()? else {
        return None;
    };
    // A version from the future is unreadable by definition; one from the past
    // may have a different shape. Either way, re-read it.
    if object.get("version") != Some(&JsValue::Num(f64::from(FORMAT_VERSION))) {
        return None;
    }
    if object.get("provider").and_then(JsValue::as_str) != Some(PROVIDER_NAMESPACE) {
        return None;
    }
    let JsValue::Num(market_id) = *object.get("marketId")? else {
        return None;
    };
    let JsValue::Num(read_at_ms) = *object.get("readAt")? else {
        return None;
    };
    // A non-finite timestamp would make every age comparison false, so an entry
    // carrying one would be cached forever.
    if !read_at_ms.is_finite() {
        return None;
    }
    Some(Entry {
        market_id,
        read_at_ms,
        payload: object.get("payload")?.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::RecordingFs;

    const MINUTE: f64 = 60_000.0;

    fn cache() -> Cache {
        Cache::new(PathBuf::from("/cache"), 30.0, true, false)
    }

    fn payload(name: &str) -> JsValue {
        JsValue::Obj(JsObject::from_document_order(vec![(
            "name".into(),
            JsValue::Str(name.into()),
        )]))
    }

    #[test]
    fn a_fresh_entry_round_trips() {
        let fs = RecordingFs::default();
        let cache = cache();
        cache.put(&fs, 3_229_009_408.0, &payload("Jaques Station"), 1_000.0);

        let found = cache.get(&fs, 3_229_009_408.0, 1_000.0 + 5.0 * MINUTE);

        let Lookup::Fresh(entry) = found else {
            panic!("{found:?}")
        };
        assert_eq!(entry.market_id, 3_229_009_408.0);
        assert_eq!(entry.read_at_ms, 1_000.0);
        assert_eq!(entry.payload, payload("Jaques Station"));
    }

    /// Past the age bound the entry is not used — and is reported as stale
    /// rather than missing, because "the cache had gone cold" and "we have
    /// never seen this market" are different things to a user deciding whether
    /// a sweep is worth repeating.
    #[test]
    fn a_stale_entry_is_not_used_and_says_why() {
        let fs = RecordingFs::default();
        let cache = cache();
        cache.put(&fs, 1.0, &payload("Old"), 0.0);

        let found = cache.get(&fs, 1.0, 31.0 * MINUTE);

        assert!(matches!(found, Lookup::Stale { .. }), "{found:?}");
        let mut hits = Hits::default();
        found.tally(&mut hits);
        assert_eq!(
            hits,
            Hits {
                stale: 1,
                ..Hits::default()
            }
        );
    }

    /// Exactly at the bound is still fresh; the comparison is strict so that a
    /// `--max-age 0` run means "read everything now" rather than "everything
    /// written this millisecond counts".
    #[test]
    fn the_age_bound_is_inclusive() {
        let fs = RecordingFs::default();
        let cache = cache();
        cache.put(&fs, 1.0, &payload("Edge"), 0.0);
        assert!(matches!(
            cache.get(&fs, 1.0, 30.0 * MINUTE),
            Lookup::Fresh(_)
        ));
        assert!(matches!(
            cache.get(&fs, 1.0, 30.0 * MINUTE + 1.0),
            Lookup::Stale { .. }
        ));
    }

    /// Half a file from an interrupted run costs one request, not the sweep.
    #[test]
    fn a_truncated_entry_is_a_miss() {
        let fs = RecordingFs::default();
        let cache = cache();
        fs.write(&cache.path(9.0), "{\"marketId\":9,\"readAt\":1,\"vers")
            .expect("in memory");

        assert_eq!(cache.get(&fs, 9.0, 2.0), Lookup::Corrupt);
    }

    /// An entry written by an older build is re-read rather than misparsed.
    #[test]
    fn an_entry_from_another_format_version_is_a_miss() {
        let fs = RecordingFs::default();
        let cache = cache();
        fs.write(
            &cache.path(9.0),
            "{\"marketId\":9,\"readAt\":1,\"version\":0,\"payload\":{}}",
        )
        .expect("in memory");

        assert_eq!(cache.get(&fs, 9.0, 2.0), Lookup::Corrupt);
    }

    /// A timestamp ahead of this process's clock must not remain "fresh" for
    /// the duration of the skew (or forever after a poisoned cache write).
    #[test]
    fn a_future_timestamp_is_corrupt_not_fresh() {
        let fs = RecordingFs::default();
        let cache = cache();
        cache.put(&fs, 9.0, &payload("Future"), 10_000.0);
        assert_eq!(cache.get(&fs, 9.0, 1_000.0), Lookup::Corrupt);
    }

    #[test]
    fn another_providers_entry_never_satisfies_market_list() {
        let fs = RecordingFs::default();
        let cache = cache();
        fs.write(
            &cache.path(9.0),
            r#"{"provider":"frontier-marketdata","marketId":9,"readAt":1,"version":2,"payload":{}}"#,
        )
        .expect("in memory");
        assert_eq!(cache.get(&fs, 9.0, 2.0), Lookup::Corrupt);
    }

    /// A non-finite timestamp would make `now - readAt > maxAge` false forever,
    /// pinning a price in the cache permanently.
    #[test]
    fn an_entry_with_a_nonfinite_timestamp_never_becomes_immortal() {
        let fs = RecordingFs::default();
        let cache = cache();
        fs.write(
            &cache.path(9.0),
            "{\"marketId\":9,\"readAt\":null,\"version\":1,\"payload\":{}}",
        )
        .expect("in memory");

        assert_eq!(cache.get(&fs, 9.0, 1e12), Lookup::Corrupt);
    }

    /// `--refresh` re-reads everything but still banks what it reads, so the
    /// next run is warm. `--no-cache` does neither.
    #[test]
    fn refresh_and_no_cache_differ_in_what_they_write() {
        let fs = RecordingFs::default();
        let refresh = Cache::new(PathBuf::from("/cache"), 30.0, true, true);
        refresh.put(&fs, 1.0, &payload("A"), 0.0);
        assert_eq!(refresh.get(&fs, 1.0, 0.0), Lookup::Skipped);
        // Written even so: a plain `Cache` reading the same directory finds it.
        assert!(matches!(cache().get(&fs, 1.0, 0.0), Lookup::Fresh(_)));

        let off = Cache::new(PathBuf::from("/cache2"), 30.0, false, false);
        off.put(&fs, 2.0, &payload("B"), 0.0);
        assert_eq!(off.get(&fs, 2.0, 0.0), Lookup::Skipped);
        assert_eq!(
            cache().get(&fs, 2.0, 0.0),
            Lookup::Missing,
            "and nothing was written"
        );
    }

    /// The file name is the id as the program prints it, not as Rust's default
    /// float formatting would render it — `4306502403.json`, never
    /// `4306502403.0.json`, or every lookup would miss forever.
    #[test]
    fn the_file_name_is_the_id_the_program_prints() {
        let cache = cache();
        assert!(cache.path(4_306_502_403.0).ends_with("4306502403.json"));
    }

    #[test]
    fn the_cache_directory_follows_xdg_then_home() {
        assert_eq!(
            Cache::locate(Some("/x"), Some("/h"), None),
            PathBuf::from("/x/edm/route"),
            "XDG wins"
        );
        assert_eq!(
            Cache::locate(None, Some("/h"), None),
            PathBuf::from("/h/.cache/edm/route")
        );
        assert_eq!(
            Cache::locate(Some(""), Some("/h"), None),
            PathBuf::from("/h/.cache/edm/route")
        );
        assert_eq!(
            Cache::locate(Some("/x"), Some("/h"), Some("/e")),
            PathBuf::from("/e")
        );
        // Homeless, but still runnable.
        assert_eq!(
            Cache::locate(None, None, None),
            PathBuf::from(".edm-cache/route")
        );
    }
}
