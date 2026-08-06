//! The `js` kernel measured against the JavaScript engine that runs
//! `game-internal-api.ts`.
//!
//! Every fixture here was produced by `xtask/oracle/bless-js.ts` under Bun.
//! When one of these fails, the fixture is right and the Rust is wrong — that
//! is the whole point of generating them rather than reasoning about them.

use edm_core::js::json::JsValue;
use edm_core::js::{self, collate, text, time};

fn fixture(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    std::fs::read_to_string(format!("{path}{name}"))
        .unwrap_or_else(|e| panic!("{name}: {e} — run `bun xtask/oracle/bless-js.ts`"))
}

/// Yields `(line_number, columns)` for every non-comment, non-blank row.
fn rows(body: &str) -> impl Iterator<Item = (usize, Vec<&str>)> {
    body.lines().enumerate().filter_map(|(i, line)| {
        let line = line.strip_suffix('\r').unwrap_or(line);
        (!line.is_empty() && !line.starts_with('#'))
            .then(|| (i + 1, line.split('\t').collect()))
    })
}

fn bits(hex: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(hex, 16).expect("16 hex digits"))
}

/// Reports up to `LIMIT` mismatches, then the total. One assert per fixture
/// beats one per row: a systematic error shows its shape instead of stopping
/// at the first symptom.
struct Failures {
    what: &'static str,
    seen: Vec<String>,
    total: usize,
}

impl Failures {
    const LIMIT: usize = 12;

    fn new(what: &'static str) -> Self {
        Self { what, seen: Vec::new(), total: 0 }
    }

    fn check(&mut self, line: usize, input: &str, expected: &str, actual: &str) {
        if expected == actual {
            return;
        }
        self.total += 1;
        if self.seen.len() < Self::LIMIT {
            self.seen.push(format!(
                "  line {line}: {input}\n    bun:  {expected:?}\n    rust: {actual:?}"
            ));
        }
    }

    fn finish(self, checked: usize) {
        assert!(
            self.total == 0,
            "{}: {} of {checked} rows disagree with Bun\n{}{}",
            self.what,
            self.total,
            self.seen.join("\n"),
            if self.total > Self::LIMIT { "\n  ..." } else { "" },
        );
    }
}

#[test]
fn js_number_matches_bun() {
    let body = fixture("js_number.tsv");
    let mut f = Failures::new("Number::toString");
    let mut n = 0;
    for (line, cols) in rows(&body) {
        let v = bits(cols[0]);
        f.check(line, cols[0], cols[1], &js::js_number(v));
        n += 1;
    }
    f.finish(n);
}

#[test]
fn to_locale_string_matches_bun() {
    let body = fixture("thousands.tsv");
    let mut f = Failures::new("toLocaleString(\"en-US\")");
    let mut n = 0;
    for (line, cols) in rows(&body) {
        let v = bits(cols[0]);
        f.check(line, cols[0], cols[1], &js::format_integer(v));
        n += 1;
    }
    f.finish(n);
}

#[test]
fn to_fixed_1_matches_bun() {
    let body = fixture("to_fixed_1.tsv");
    let mut f = Failures::new("toFixed(1)");
    let mut n = 0;
    for (line, cols) in rows(&body) {
        let v = bits(cols[0]);
        f.check(line, cols[0], cols[1], &js::to_fixed_1(v));
        n += 1;
    }
    f.finish(n);
}

#[test]
fn to_iso_string_matches_bun() {
    let body = fixture("iso8601.tsv");
    let mut f = Failures::new("Date.toISOString");
    let mut n = 0;
    for (line, cols) in rows(&body) {
        let ms = bits(cols[0]);
        let actual = time::iso8601_from_ms(ms).unwrap_or_else(|| "-".to_owned());
        f.check(line, cols[0], cols[1], &actual);
        n += 1;
    }
    f.finish(n);
}

#[test]
fn to_number_matches_bun() {
    let body = fixture("to_number.tsv");
    let mut f = Failures::new("Number(string)");
    let mut n = 0;
    for (line, cols) in rows(&body) {
        let input = unquote(cols[0]);
        let actual = js::js_number(js::to_number(&input));
        f.check(line, cols[0], cols[1], &actual);
        n += 1;
    }
    f.finish(n);
}

#[test]
fn whitespace_predicates_match_bun() {
    let body = fixture("trim.tsv");
    let mut trim = Failures::new("String.prototype.trim");
    let mut number = Failures::new("Number() StrWhiteSpace");
    let mut n = 0;
    for (line, cols) in rows(&body) {
        let cp = u32::from_str_radix(cols[0], 16).expect("hex codepoint");
        let Some(c) = char::from_u32(cp) else { continue };
        let sample = format!("{c}1{c}");

        let strips_trim = text::js_trim(&sample) == "1";
        trim.check(line, cols[0], cols[1], if strips_trim { "1" } else { "0" });

        let strips_number = js::to_number(&sample) == 1.0;
        number.check(line, cols[0], cols[2], if strips_number { "1" } else { "0" });
        n += 1;
    }
    trim.finish(n);
    number.finish(n);
}

#[test]
fn locale_compare_matches_bun() {
    let body = fixture("collate.txt");
    let mut lines = rows(&body);

    // Row 1: ASCII, Latin-1 Supplement and Latin Extended-A, in the order
    // localeCompare puts them.
    let (line, order) = lines.next().expect("primary order row");
    let expected: Vec<char> = order[0]
        .split(' ')
        .map(|h| {
            let cp = u32::from_str_radix(h, 16).expect("hex codepoint");
            char::from_u32(cp).expect("scalar value")
        })
        .collect();
    let mut actual = expected.clone();
    actual.sort_by(|a, b| collate::locale_cmp(&a.to_string(), &b.to_string()));
    assert_eq!(
        render(&actual),
        render(&expected),
        "line {line}: ASCII primary order disagrees with Bun"
    );

    // Then sampled pairs, which catch multi-character and level-interaction
    // effects the single-character permutation cannot reach.
    let mut f = Failures::new("localeCompare");
    let mut n = 0;
    for (line, cols) in lines {
        let (a, b) = (unquote(cols[0]), unquote(cols[1]));
        let sign = match collate::locale_cmp(&a, &b) {
            std::cmp::Ordering::Less => "-1",
            std::cmp::Ordering::Equal => "0",
            std::cmp::Ordering::Greater => "1",
        };
        f.check(line, &format!("{a:?} vs {b:?}"), cols[2], sign);
        n += 1;
    }
    f.finish(n);
}

/// The two findings that a port can get wrong while looking entirely correct:
/// ECMAScript's object enumeration order, and integral doubles serializing
/// without a decimal point. See `js::json`'s module docs for why each matters.
#[test]
fn json_key_order_and_serialization_match_bun() {
    let body = fixture("json.tsv");
    let mut keys = Failures::new("Object.keys");
    let mut compact = Failures::new("JSON.stringify");
    let mut pretty = Failures::new("JSON.stringify(_, null, 2)");
    let mut n = 0;

    for (line, cols) in rows(&body) {
        // Decoded with serde_json rather than our own parser, so a bug in the
        // thing under test cannot hide itself by also mis-reading the fixture.
        let doc: String = serde_json::from_str(cols[0]).expect("doc column");
        let want_keys: Vec<String> = serde_json::from_str(cols[1]).expect("keys column");
        let want_compact: String = serde_json::from_str(cols[2]).expect("compact column");
        let want_pretty: String = serde_json::from_str(cols[3]).expect("pretty column");

        let value = match JsValue::parse(&doc) {
            Ok(v) => v,
            Err(e) => panic!("line {line}: {doc} failed to parse: {e}"),
        };

        if let Some(object) = value.as_object() {
            let actual: Vec<&str> = object.iter().map(|(k, _)| k).collect();
            keys.check(line, &doc, &want_keys.join("\u{1f}"), &actual.join("\u{1f}"));
        }
        compact.check(line, &doc, &want_compact, &value.stringify_compact());
        pretty.check(line, &doc, &want_pretty, &value.stringify(2));
        n += 1;
    }

    keys.finish(n);
    compact.finish(n);
    pretty.finish(n);
}

/// The boundary itself, stated directly rather than inferred from the corpus.
#[test]
fn array_index_boundary() {
    use edm_core::js::json::array_index;
    for yes in ["0", "1", "9", "4294967294"] {
        assert!(array_index(yes).is_some(), "{yes} should be an array index");
    }
    for no in [
        "4294967295", "4294967296", "01", "007", "", "1.5", "1e3", "-1", "+1", " 1", "1 ",
        "0x1", "١", "9007199254740993",
    ] {
        assert!(array_index(no).is_none(), "{no} should not be an array index");
    }
}

/// The one that would break every EDDN upload: an integral double must not
/// acquire a decimal point on the way out.
#[test]
fn integral_doubles_serialize_without_a_point() {
    let value = JsValue::parse(r#"{"meanPrice":123.0,"stock":0,"demand":1e3}"#).unwrap();
    assert_eq!(value.stringify_compact(), r#"{"meanPrice":123,"stock":0,"demand":1000}"#);
}

fn render(chars: &[char]) -> String {
    chars.iter().collect()
}

/// Minimal JSON string unescape, for the fixture's quoted inputs.
fn unquote(s: &str) -> String {
    let inner = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let cp = u32::from_str_radix(&hex, 16).expect("\\u escape");
                out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            }
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}
