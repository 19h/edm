//! String semantics: UTF-16 indexing, the two whitespace predicates, and the
//! cell-fitting primitives the table renderer is built from.

use std::borrow::Cow;

/// What `clampText` appends when it cuts a value short.
pub const TRUNCATION_MARK: char = '~';

/// How a cell's width is measured.
///
/// The TypeScript measures in UTF-16 code units, because that is what
/// `String.prototype.length` returns. That is wrong for a terminal — a CJK
/// station name occupies two columns per code unit, and an astral emoji
/// occupies two columns for two code units — but it is what the original does,
/// so it is the default and the parity path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Metric {
    /// `String.prototype.length`.
    #[default]
    Utf16,
    /// Terminal display columns, UAX #11. Selected by `EDM_WIDTH=display`; a
    /// registered divergence when enabled.
    Display,
}

impl Metric {
    #[must_use]
    pub fn of_char(self, c: char) -> usize {
        match self {
            Self::Utf16 => c.len_utf16(),
            Self::Display => unicode_width::UnicodeWidthChar::width(c).unwrap_or(0),
        }
    }

    #[must_use]
    pub fn of_str(self, s: &str) -> usize {
        match self {
            Self::Utf16 => utf16_len(s),
            Self::Display => unicode_width::UnicodeWidthStr::width(s),
        }
    }
}

/// `String.prototype.length`.
#[must_use]
pub fn utf16_len(s: &str) -> usize {
    // `len()` counts UTF-8 bytes; every code point of 3 UTF-8 bytes or fewer is
    // one UTF-16 unit and every 4-byte one is two, so the ASCII fast path is
    // exact and worth taking — most of this program's strings are ASCII.
    if s.is_ascii() { s.len() } else { s.chars().map(char::len_utf16).sum() }
}

/// Is this an ECMAScript `WhiteSpace` or `LineTerminator` code point?
///
/// Deliberately not `char::is_whitespace`: Rust follows the Unicode
/// `White_Space` property, which includes U+0085 (NEL), while ECMAScript's
/// grammar is TAB/VT/FF/ZWNBSP plus general category `Zs` plus the four line
/// terminators — and excludes NEL. It also excludes U+180E and U+200B, which
/// some older engines used to strip.
#[must_use]
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        // WhiteSpace
        '\u{9}' | '\u{B}' | '\u{C}' | '\u{FEFF}'
        // LineTerminator
        | '\u{A}' | '\u{D}' | '\u{2028}' | '\u{2029}'
        // <USP>: general category Space_Separator
        | '\u{20}' | '\u{A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}'
        | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// `String.prototype.trim`.
///
/// Kept distinct from [`str_white_space_trim`] even though the specification
/// gives them the same character set, because engines have historically
/// disagreed about U+0085 here. `tests/fixtures/trim.tsv` decides it by
/// measurement; if the two ever diverge, this is the one that changes.
#[must_use]
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `StrWhiteSpace` — the trimming `Number(string)` performs before parsing.
#[must_use]
pub fn str_white_space_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `s.slice(0, n)` measured in UTF-16 code units, re-encoded as UTF-8.
///
/// When `n` bisects a surrogate pair, JavaScript keeps the lone high surrogate.
/// That is not a valid scalar value, so writing it out as UTF-8 yields
/// U+FFFD — which is what actually lands on the terminal, and therefore what
/// we produce.
#[must_use]
pub fn slice_utf16_prefix(s: &str, n: usize) -> String {
    let mut out = String::with_capacity(n);
    let mut used = 0;
    for c in s.chars() {
        let w = c.len_utf16();
        if used + w <= n {
            out.push(c);
            used += w;
        } else {
            if used < n {
                out.push('\u{FFFD}');
            }
            break;
        }
    }
    out
}

/// The same, measured by an arbitrary [`Metric`].
fn slice_prefix(s: &str, n: usize, metric: Metric) -> String {
    match metric {
        Metric::Utf16 => slice_utf16_prefix(s, n),
        Metric::Display => {
            let mut out = String::with_capacity(n);
            let mut used = 0;
            for c in s.chars() {
                let w = metric.of_char(c);
                if used + w > n {
                    break;
                }
                out.push(c);
                used += w;
            }
            out
        }
    }
}

/// `clampText` (`game-internal-api.ts:367`).
///
/// The order of the three tests is observable: a value that already fits is
/// returned untouched even when `width` is 1, and only then does a width of 1
/// collapse to a bare truncation mark.
#[must_use]
pub fn clamp(text: &str, width: isize, metric: Metric) -> Cow<'_, str> {
    if width <= 0 {
        return Cow::Borrowed("");
    }
    let width = width as usize;
    if metric.of_str(text) <= width {
        return Cow::Borrowed(text);
    }
    if width == 1 {
        return Cow::Owned(TRUNCATION_MARK.to_string());
    }
    let mut out = slice_prefix(text, width - 1, metric);
    out.push(TRUNCATION_MARK);
    Cow::Owned(out)
}

/// Where a cell's content sits inside its column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Right,
}

/// `padCell` (`game-internal-api.ts:374`) — clamp, then pad to exactly `width`.
#[must_use]
pub fn pad(text: &str, width: usize, align: Align, metric: Metric) -> String {
    let clamped = clamp(text, width as isize, metric);
    // Under `Metric::Display` a clamped string can measure narrower than the
    // budget when the next character is wide, so this must not underflow.
    let padding = width.saturating_sub(metric.of_str(&clamped));
    let mut out = String::with_capacity(clamped.len() + padding);
    if align == Align::Right {
        out.extend(core::iter::repeat_n(' ', padding));
        out.push_str(&clamped);
    } else {
        out.push_str(&clamped);
        out.extend(core::iter::repeat_n(' ', padding));
    }
    out
}

/// `elide` (`game-internal-api.ts:554`).
///
/// Faithful to a quirk: `text.slice(-tail)` with `tail == 0` is `slice(-0)`,
/// which is `slice(0)` — the *whole* string, not the empty one. So
/// `elide(s, h, 0)` returns a head, an ellipsis, and then `s` in full. No
/// caller does that today; reproducing it costs one branch.
#[must_use]
pub fn elide(text: &str, head: usize, tail: usize) -> String {
    let len = utf16_len(text);
    if len <= head + tail + 3 {
        return text.to_owned();
    }
    let mut out = slice_utf16_prefix(text, head);
    out.push_str("...");
    if tail == 0 {
        out.push_str(text);
    } else {
        out.push_str(&slice_utf16_suffix(text, tail));
    }
    out
}

/// `s.slice(-n)` measured in UTF-16 code units, for `n > 0`.
#[must_use]
pub fn slice_utf16_suffix(s: &str, n: usize) -> String {
    let len = utf16_len(s);
    if n >= len {
        return s.to_owned();
    }
    let skip = len - n;
    let mut out = String::new();
    let mut seen = 0;
    for c in s.chars() {
        let w = c.len_utf16();
        if seen >= skip {
            out.push(c);
        } else if seen + w > skip {
            // The cut lands inside a surrogate pair; the low surrogate survives
            // alone and encodes as U+FFFD.
            out.push('\u{FFFD}');
        }
        seen += w;
    }
    out
}
