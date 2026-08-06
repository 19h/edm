//! A local copy of what Ardent said about the galaxy's shape.
//!
//! The price cache holds Companion API listings, which go stale in minutes and
//! cost an authenticated request each. This holds the *other* half of a sweep,
//! and it was not cached at all: the `/nearby` pages that enumerate a region
//! and the `/markets` lists that say which stations are in it. At radius 200
//! that is hundreds of round trips before the plan can even be priced, repeated
//! in full on every run — including runs over a region already swept an hour
//! earlier.
//!
//! **These answers age far more slowly than prices do.** A system's coordinates
//! are immutable; the set of systems within 200 Ly of a point does not change
//! at all. A station list changes when somebody builds a station, which is
//! days-to-weeks. So the two get different lifetimes and they are recorded
//! here rather than being folded into the price cache, where a single
//! `--max-age` would have to be wrong for one of them.
//!
//! Ardent serves no `ETag` and no `Last-Modified` — measured 2026-08-06, the
//! full response header set is nine headers and none of them is a validator —
//! so a conditional request is not available and a local lifetime is the only
//! mechanism there is. It is also not CDN-fronted: every request reaches the
//! origin, which makes caching the polite thing to do as well as the fast one.

use std::path::{Path, PathBuf};

use edm_core::js::json::{JsObject, JsValue};

use crate::ports::Fs;

/// Bumped when the stored shape changes; an older entry reads as absent.
const FORMAT_VERSION: u32 = 1;

/// How long a `/nearby` page is good for.
///
/// Systems do not move and new ones are not discovered — the galaxy is
/// generated, not surveyed — so the only thing that can change an answer is
/// Ardent learning about a system it had not recorded. A week is short against
/// that and long against a session.
pub const NEARBY_LIFETIME_MINUTES: f64 = 7.0 * 24.0 * 60.0;

/// How long a station list is good for.
///
/// Shorter, because this one really does change: stations are built, and a
/// station Ardent has not yet seen is the one gap in a route's coverage that
/// this program cannot close for itself. A day keeps a session's repeated runs
/// instant without letting a region go stale across a week.
pub const MARKETS_LIFETIME_MINUTES: f64 = 24.0 * 60.0;

/// The stored copy.
#[derive(Clone, Debug)]
pub struct Atlas {
    root: PathBuf,
    enabled: bool,
    refresh: bool,
}

/// What a run read from here rather than from the network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hits {
    pub fresh: usize,
    pub missed: usize,
}

impl Atlas {
    /// Beside the price cache, in its own directory: the two hold different
    /// kinds of answer with different lifetimes, and one `--max-age` cannot be
    /// right for both.
    #[must_use]
    pub fn new(cache_root: &Path, enabled: bool, refresh: bool) -> Self {
        Self { root: cache_root.join("ardent"), enabled, refresh }
    }

    /// The file a URL is stored under.
    ///
    /// Keyed by a hash of the URL, not by the system name: names carry spaces,
    /// apostrophes and slashes — `Synuefe YK-N c23-15`, `Barnard's Star` — and
    /// a path built from one is a portability problem at best and a directory
    /// traversal at worst. The URL already encodes every parameter that changes
    /// the answer, including `maxDistance`, so hashing it cannot conflate two
    /// different questions.
    fn path(&self, url: &str) -> PathBuf {
        self.root.join(format!("{:016x}.json", fnv1a(url)))
    }

    /// A stored answer, if it is still within `lifetime_minutes`.
    pub fn get<F: Fs>(&self, fs: &F, url: &str, now_ms: f64, lifetime_minutes: f64) -> Option<JsValue> {
        if !self.enabled || self.refresh {
            return None;
        }
        let text = fs.read_to_string(&self.path(url)).ok()?;
        let entry = decode(&text)?;
        // The URL is stored and compared, so a hash collision answers the wrong
        // question at most once — and then only if two URLs collide *and* the
        // stored one is fresh. Without this it would answer it silently for a
        // week.
        if entry.url != url || now_ms - entry.read_at_ms > lifetime_minutes * 60_000.0 {
            return None;
        }
        Some(entry.body)
    }

    /// Store an answer. Failures are silent: a cache that cannot be written is
    /// a slower run, not a failed one.
    pub fn put<F: Fs>(&self, fs: &F, url: &str, body: &JsValue, now_ms: f64) {
        if !self.enabled || fs.create_dir_all(&self.root).is_err() {
            return;
        }
        let entry = JsObject::from_document_order(vec![
            ("url".into(), JsValue::Str(url.into())),
            ("readAt".into(), JsValue::Num(now_ms)),
            ("version".into(), JsValue::Num(f64::from(FORMAT_VERSION))),
            ("body".into(), body.clone()),
        ]);
        let _ = fs.write(&self.path(url), &JsValue::Obj(entry).stringify_compact());
    }
}

/// One stored answer.
struct Entry {
    url: String,
    read_at_ms: f64,
    body: JsValue,
}

fn decode(text: &str) -> Option<Entry> {
    let JsValue::Obj(object) = JsValue::parse(text).ok()? else {
        return None;
    };
    if object.get("version") != Some(&JsValue::Num(f64::from(FORMAT_VERSION))) {
        return None;
    }
    let JsValue::Str(url) = object.get("url")? else { return None };
    let JsValue::Num(read_at_ms) = *object.get("readAt")? else {
        return None;
    };
    // A non-finite instant would make every age comparison false and pin the
    // entry for ever.
    if !read_at_ms.is_finite() {
        return None;
    }
    Some(Entry { url: url.to_string(), read_at_ms, body: object.get("body")?.clone() })
}

/// FNV-1a, 64-bit.
///
/// A hash, not a checksum: the stored URL is compared on read, so this only has
/// to spread well enough that two hot URLs do not collide. Chosen over
/// `DefaultHasher` because that one is explicitly not stable across releases,
/// and a cache whose keys move when the toolchain moves is a cache that is
/// always cold.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::RecordingFs;

    const MINUTE: f64 = 60_000.0;

    fn atlas() -> Atlas {
        Atlas::new(Path::new("/cache"), true, false)
    }

    fn body(name: &str) -> JsValue {
        JsValue::Obj(JsObject::from_document_order(vec![(
            "systemName".into(),
            JsValue::Str(name.into()),
        )]))
    }

    #[test]
    fn a_stored_answer_round_trips() {
        let fs = RecordingFs::default();
        let atlas = atlas();
        atlas.put(&fs, "https://x/nearby?maxDistance=60", &body("Sol"), 0.0);

        assert_eq!(
            atlas.get(&fs, "https://x/nearby?maxDistance=60", MINUTE, 60.0),
            Some(body("Sol"))
        );
    }

    /// `maxDistance` is part of the URL, so a wider query is a different
    /// question and must not be answered from a narrower one's page.
    #[test]
    fn a_different_radius_is_a_different_answer() {
        let fs = RecordingFs::default();
        let atlas = atlas();
        atlas.put(&fs, "https://x/nearby?maxDistance=60", &body("Sol"), 0.0);

        assert_eq!(atlas.get(&fs, "https://x/nearby?maxDistance=200", MINUTE, 60.0), None);
    }

    #[test]
    fn an_answer_past_its_lifetime_is_not_used() {
        let fs = RecordingFs::default();
        let atlas = atlas();
        atlas.put(&fs, "u", &body("Sol"), 0.0);

        assert!(atlas.get(&fs, "u", 60.0 * MINUTE, 60.0).is_some(), "the bound is inclusive");
        assert!(atlas.get(&fs, "u", 60.0 * MINUTE + 1.0, 60.0).is_none());
    }

    /// The URL is stored and compared, so a hash collision costs one wrong
    /// lookup rather than a week of silently answering the wrong question.
    #[test]
    fn a_mismatched_url_is_a_miss_even_at_the_same_path() {
        let fs = RecordingFs::default();
        let atlas = atlas();
        atlas.put(&fs, "one", &body("Sol"), 0.0);
        // Write the other URL's answer at the first one's path by hand.
        let stored = fs.read_to_string(&atlas.path("one")).expect("stored");
        fs.write(&atlas.path("two"), &stored).expect("in memory");

        assert!(atlas.get(&fs, "two", 0.0, 60.0).is_none());
    }

    #[test]
    fn refresh_reads_nothing_but_still_writes() {
        let fs = RecordingFs::default();
        let refreshing = Atlas::new(Path::new("/cache"), true, true);
        refreshing.put(&fs, "u", &body("Sol"), 0.0);

        assert!(refreshing.get(&fs, "u", 0.0, 60.0).is_none());
        assert!(atlas().get(&fs, "u", 0.0, 60.0).is_some(), "the next run finds it");
    }

    #[test]
    fn disabled_neither_reads_nor_writes() {
        let fs = RecordingFs::default();
        let off = Atlas::new(Path::new("/cache"), false, false);
        off.put(&fs, "u", &body("Sol"), 0.0);

        assert!(atlas().get(&fs, "u", 0.0, 60.0).is_none());
    }

    /// Names carry spaces and apostrophes; a path built from one is a
    /// portability problem at best and a traversal at worst.
    #[test]
    fn a_hostile_name_cannot_escape_the_directory() {
        let path = atlas().path("https://x/system/name/../../etc/passwd/markets");
        assert!(path.starts_with("/cache/ardent"), "{}", path.display());
        assert_eq!(path.components().filter(|c| c.as_os_str() == "..").count(), 0);
    }

    /// Pinned against the published FNV-1a 64 test vectors, because a cache
    /// whose keys move with the toolchain is a cache that is always cold —
    /// which is exactly why `DefaultHasher` was not used.
    #[test]
    fn the_hash_is_the_published_fnv1a() {
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a("foobar"), 0x8594_4171_f739_67e8);
    }
}
