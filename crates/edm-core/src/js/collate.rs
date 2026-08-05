//! `String.prototype.localeCompare`.
//!
//! Every sort in `market-request.ts` goes through it — commodity names,
//! category bands, header rows, station names, market types. Byte order is not
//! a substitute: it puts every uppercase letter before every lowercase one, so
//! `"Silver"` would sort before `"agronomic treatment"` and every table would
//! come out in the wrong order.
//!
//! An ASCII approximation was written first and measured against Bun. It was
//! wrong in ways that no amount of care would have caught by reasoning:
//! `ß` expands to `ss` (so `"straße" > "strasse"`, not `<`), `þ` sorts *after*
//! `z` rather than as `th`, `ø` is a secondary variant of `o`, `æ` groups with
//! `a`, `½` falls between `1` and `2`, and the ASCII punctuation order is not
//! the one you would guess. Elite Dangerous system and station names are full
//! of exactly these characters.
//!
//! So this delegates to ICU4X, which is the same CLDR data JavaScript engines
//! collate with. `tests/js_oracle.rs` holds it to Bun's answer over a corpus of
//! Latin-1 and Latin Extended-A strings.
//!
//! One caveat worth stating plainly: bare `localeCompare()` with no locale
//! argument uses the *runtime's* default locale, so the TypeScript's own
//! ordering is not reproducible across machines. We pin root deliberately
//! rather than inherit that nondeterminism.

use std::cmp::Ordering;
use std::sync::OnceLock;

use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences, options::CollatorOptions};

/// Built once. Construction parses collation data, so doing it per comparison
/// would dominate the cost of every sort in the program.
fn collator() -> &'static CollatorBorrowed<'static> {
    static COLLATOR: OnceLock<CollatorBorrowed<'static>> = OnceLock::new();
    COLLATOR.get_or_init(|| {
        // Root locale, default options: tertiary strength (so case breaks
        // ties), non-ignorable punctuation — which is what `localeCompare`
        // does by default.
        Collator::try_new(CollatorPreferences::default(), CollatorOptions::default())
            .expect("ICU4X ships compiled root collation data")
    })
}

/// `a.localeCompare(b)` under the root locale.
#[must_use]
pub fn locale_cmp(a: &str, b: &str) -> Ordering {
    // Sorts here run over columns with many repeated values, so the identity
    // check earns its keep.
    if a == b {
        return Ordering::Equal;
    }
    collator().compare(a, b)
}

/// Sorts in place, stably, by a string key.
///
/// Stability is not incidental: `Array.prototype.sort` has been required to be
/// stable since ES2019, and several tables sort by one key after grouping by
/// another. `sort_unstable_by` would reorder ties and break the snapshots.
pub fn sort_by_key_locale<T, F>(items: &mut [T], mut key: F)
where
    F: FnMut(&T) -> &str,
{
    items.sort_by(|left, right| locale_cmp(key(left), key(right)));
}
