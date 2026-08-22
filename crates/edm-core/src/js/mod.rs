//! ECMAScript semantics that Rust does not share.
//!
//! Every function here exists because the Rust-idiomatic equivalent produces
//! different bytes. They are pinned by fixtures generated from the same Bun
//! build that runs `game-internal-api.ts` (`cargo xtask bless`), because arguing
//! about JavaScript engine behaviour is slower and less reliable than
//! measuring it.
//!
//! Rule of thumb for reviewers: if you see `{}` applied to an `f64`, or
//! `.round()`, `.min()`, `.trim()`, or `serde_json::to_string` anywhere in
//! this workspace, it is a bug. `clippy.toml` enforces that.

pub mod collate;
pub mod json;
pub mod text;
pub mod time;

/// `Number.MAX_SAFE_INTEGER`.
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A shortest-round-trip decimal significand. `f64` needs at most 17
/// significant digits; the extra room absorbs ryu's plain-form padding before
/// trailing zeros are stripped.
type Significand = ([u8; 32], usize, i32);

/// Decomposes a finite, strictly positive `v` into `(digits, k, n)` such that
/// `0.<digits> * 10^n == v`, with `k` minimal — ECMA-262 §6.1.6.1.20 step 5.
///
/// ryu gives us the shortest round-tripping digits already; all that is left is
/// to normalise its two output shapes (`123.45` and `1.2345e10`) into one.
fn shortest_digits(v: f64) -> Significand {
    let mut buf = ryu::Buffer::new();
    let s = buf.format_finite(v);

    let (mantissa, exp) = match s.split_once('e') {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (s, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));

    let mut digits = [b'0'; 32];
    let mut k = 0usize;
    let mut push = |bytes: &[u8]| {
        for &b in bytes {
            if k < digits.len() {
                digits[k] = b;
                k += 1;
            }
        }
    };

    let n = if int_part == "0" {
        // `0.000ddd` — the leading fractional zeros are not significant, they
        // are exponent.
        let lead = frac_part.len() - frac_part.trim_start_matches('0').len();
        push(&frac_part.as_bytes()[lead..]);
        exp - lead as i32
    } else {
        push(int_part.as_bytes());
        push(frac_part.as_bytes());
        int_part.len() as i32 + exp
    };

    // ryu writes `100.0` and `1e21`; the trailing zeros in the former are
    // padding, not significance, and would inflate `k`. A shortest
    // representation never ends in a significant zero, so this is lossless.
    while k > 1 && digits[k - 1] == b'0' {
        k -= 1;
    }
    (digits, k, n)
}

/// `Number::toString(x, 10)` — ECMA-262 §6.1.6.1.20.
///
/// The most load-bearing function in the port: it decides how every number
/// reaches stdout, the query envelope and the EDDN payload. Rust's `Display`
/// agrees on most values and disagrees on exactly the ones that matter —
/// `1e21` (`"1e+21"`, not `"1000000000000000000000"`), `-0.0` (`"0"`, not
/// `"-0"`), and any integral double under `serde_json` (`"1"`, not `"1.0"`,
/// which is what would make the EDDN gateway reject every upload).
#[must_use]
pub fn js_number(v: f64) -> String {
    let mut out = String::new();
    write_js_number(&mut out, v);
    out
}

/// [`js_number`] writing in place.
pub fn write_js_number(out: &mut String, v: f64) {
    // Step order is ECMA's and matters: `-0.0` must be caught by step 2 before
    // step 3 can prepend a sign to it.
    if v.is_nan() {
        out.push_str("NaN");
        return;
    }
    if v == 0.0 {
        out.push('0');
        return;
    }
    if v < 0.0 {
        out.push('-');
        write_js_number(out, -v);
        return;
    }
    if v.is_infinite() {
        out.push_str("Infinity");
        return;
    }

    let (buf, k, n) = shortest_digits(v);
    let digits = core::str::from_utf8(&buf[..k]).unwrap_or("0");
    let k = k as i32;

    if n >= k && n <= 21 {
        // Step 6 — all digits, then trailing zeros.
        out.push_str(digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if n > 0 && n <= 21 {
        // Step 7 — a decimal point inside the digits.
        let split = n as usize;
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    } else if n > -6 && n <= 0 {
        // Step 8 — "0.", leading zeros, then the digits.
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(digits);
    } else if k == 1 {
        // Step 9 — a bare exponential.
        out.push_str(digits);
        push_exponent(out, n - 1);
    } else {
        // Step 10 — a fractional exponential.
        out.push_str(&digits[..1]);
        out.push('.');
        out.push_str(&digits[1..]);
        push_exponent(out, n - 1);
    }
}

fn push_exponent(out: &mut String, e: i32) {
    out.push('e');
    out.push(if e < 0 { '-' } else { '+' });
    push_u32(out, e.unsigned_abs());
}

fn push_u32(out: &mut String, v: u32) {
    let mut buf = [0u8; 10];
    let mut len = 0;
    let mut v = v;
    loop {
        buf[len] = b'0' + (v % 10) as u8;
        len += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for &b in buf[..len].iter().rev() {
        out.push(b as char);
    }
}

/// `formatInteger` (`game-internal-api.ts:522`) — `Math.trunc` then
/// `toLocaleString("en-US")`, with a `"?"` placeholder for non-finite input.
///
/// Note what this is *not*: it never goes exponential. `1e21` renders as
/// twenty-one zeros, and values above `2^53` render their shortest-round-trip
/// digits zero-padded out to full width, which is what `Intl.NumberFormat`
/// does and what a naive `{:.0}` would get wrong.
#[must_use]
pub fn format_integer(v: f64) -> String {
    if v.is_finite() {
        thousands(v.trunc())
    } else {
        "?".to_owned()
    }
}

/// `Number.prototype.toLocaleString("en-US")` for an integral double.
#[must_use]
pub fn thousands(t: f64) -> String {
    if t == 0.0 {
        // `Intl.NumberFormat` uses the negative pattern for -0, so unlike
        // `String(-0)` this keeps the sign.
        return if t.is_sign_negative() {
            "-0".to_owned()
        } else {
            "0".to_owned()
        };
    }
    if !t.is_finite() {
        return if t.is_sign_negative() {
            "-∞".to_owned()
        } else {
            "∞".to_owned()
        };
    }

    let neg = t < 0.0;
    let (buf, k, n) = shortest_digits(if neg { -t } else { t });
    // `t` is integral, so the decimal point sits at or past the last digit.
    let width = n.max(1) as usize;

    let mut out = String::with_capacity(width + width / 3 + 1);
    if neg {
        out.push('-');
    }
    // The significand, then zero padding out to the full positional width —
    // which is how values above 2^53 render every digit despite carrying only
    // seventeen significant ones.
    let padded = buf[..k]
        .iter()
        .copied()
        .chain(core::iter::repeat(b'0'))
        .take(width);
    for (i, digit) in padded.enumerate() {
        if i > 0 && (width - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(char::from(digit));
    }
    out
}

/// `formatQuantity` (`game-internal-api.ts:527`) — zeroes dominate a market table,
/// so they render as a placeholder.
///
/// The `=== 0` test happens *before* truncation, and `-0 === 0` is true in
/// JavaScript, so `-0.0` is a dash while `0.4` is `"0"`.
#[must_use]
pub fn format_quantity(v: f64) -> String {
    if v == 0.0 {
        "-".to_owned()
    } else {
        format_integer(v)
    }
}

/// `Number.prototype.toFixed(1)` — ECMA-262 §21.1.3.3.
///
/// Ties round *away from zero* (the sign is stripped first, then the larger
/// `n` is chosen), which is neither Rust's `{:.1}` (half-to-even) nor
/// `f64::round`. `-0.0` renders as `"0.0"`, and `|x| >= 1e21` falls back to
/// [`js_number`].
#[must_use]
pub fn to_fixed_1(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if !x.is_finite() || x.abs() >= 1e21 {
        return js_number(x);
    }

    // ECMA strips the sign before choosing `n`. `-0.0 < 0.0` is false, so it
    // takes the positive path — but it must still be normalised to `+0.0`,
    // because step 10 formats the *mathematical value* of `n`, and the
    // mathematical value of -0 is 0. Keeping the negative zero here is how you
    // get `"-0.0"` where Bun says `"0.0"`.
    let neg = x < 0.0;
    let a = x.abs();

    // Round-half-up at one fractional digit has to be decided on the double's
    // *exact* value, not an approximation of it: 0.15 is really
    // 0.1499999999999999944, so it must round down, while 0.25 is exact and
    // must round up. Formatting to the exact number of fractional digits gives
    // us that value as a decimal string to round by hand.
    let exact = format!("{:.*}", exact_fraction_digits(a), a);
    let (int_part, frac_part) = exact.split_once('.').unwrap_or((exact.as_str(), ""));
    let frac = frac_part.as_bytes();

    // Everything after the digit we keep is the remainder. Round up when it is
    // more than half (`>5…`) and also when it is exactly half (`5` then zeros),
    // because ECMA breaks ties toward the larger `n`. Both cases reduce to
    // "the next digit is at least 5".
    let round_up = frac.get(1).is_some_and(|&b| b >= b'5');

    let mut digit = u32::from(frac.first().copied().unwrap_or(b'0') - b'0');
    let mut whole = int_part.to_owned();
    if round_up {
        digit += 1;
        if digit == 10 {
            digit = 0;
            increment_decimal(&mut whole);
        }
    }

    // ECMA takes the sign off in step 6 and puts it back on unconditionally in
    // step 12, so `(-0.04).toFixed(1)` is `"-0.0"` — the sign survives even
    // when every rendered digit is zero. `-0.0` itself never gets here: it is
    // not `< 0.0`, so `neg` is false and it renders as `"0.0"`.
    let mut out = String::with_capacity(whole.len() + 3);
    if neg {
        out.push('-');
    }
    out.push_str(&whole);
    out.push('.');
    out.push(char::from(b'0' + digit as u8));
    out
}

/// The exact number of decimal fraction digits of `a`.
///
/// A double is `m * 2^-j` with `m` odd; that value has exactly `j` fractional
/// decimal digits. Knowing `j` lets [`to_fixed_1`] format the *exact* value
/// rather than a padded approximation of it.
fn exact_fraction_digits(a: f64) -> usize {
    if a == 0.0 || !a.is_finite() {
        return 0;
    }
    let bits = a.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let mantissa = bits & ((1u64 << 52) - 1);
    let (m, e2) = if biased == 0 {
        (mantissa, -1074i32)
    } else {
        (mantissa | (1u64 << 52), biased - 1075)
    };
    if e2 >= 0 || m == 0 {
        return 0;
    }
    let shift = (m.trailing_zeros() as i32).min(-e2);
    (-(e2 + shift)) as usize
}

/// Adds one to a string of ASCII decimal digits in place, growing it on carry.
fn increment_decimal(s: &mut String) {
    let mut bytes = core::mem::take(s).into_bytes();
    let mut i = bytes.len();
    loop {
        if i == 0 {
            bytes.insert(0, b'1');
            break;
        }
        i -= 1;
        if bytes[i] == b'9' {
            bytes[i] = b'0';
        } else {
            bytes[i] += 1;
            break;
        }
    }
    *s = String::from_utf8(bytes).unwrap_or_else(|_| "0".to_owned());
}

/// `Math.round` — half toward **positive infinity**, preserving `-0`.
///
/// Not `f64::round` (half away from zero: `-2.5` would give `-3`), and not
/// `(x + 0.5).floor()` (which turns `0.49999999999999994` into `1`, because
/// adding 0.5 rounds it up to exactly 0.5 before the floor sees it).
#[must_use]
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let floor = x.floor();
    let rounded = if x - floor >= 0.5 { floor + 1.0 } else { floor };
    if rounded == 0.0 && x < 0.0 {
        -0.0
    } else {
        rounded
    }
}

/// `Math.min` — NaN-propagating, and `-0` sorts below `+0`.
#[must_use]
pub fn js_min(a: f64, b: f64) -> f64 {
    use core::cmp::Ordering;
    match a.partial_cmp(&b) {
        Some(Ordering::Less) => a,
        Some(Ordering::Greater) => b,
        // Numerically equal, which for zeroes is not the same as identical:
        // `Math.min(0, -0)` is `-0`, and the sign survives into `String(n)`.
        Some(Ordering::Equal) => {
            if a.is_sign_negative() {
                a
            } else {
                b
            }
        }
        None => f64::NAN,
    }
}

/// `Math.max` — NaN-propagating, and `+0` sorts above `-0`.
#[must_use]
pub fn js_max(a: f64, b: f64) -> f64 {
    use core::cmp::Ordering;
    match a.partial_cmp(&b) {
        Some(Ordering::Greater) => a,
        Some(Ordering::Less) => b,
        Some(Ordering::Equal) => {
            if a.is_sign_negative() {
                b
            } else {
                a
            }
        }
        None => f64::NAN,
    }
}

/// `Number.isSafeInteger`.
///
/// Deliberately not `x as i64 as f64 == x`: the cast saturates, so every value
/// above `i64::MAX` would report as a safe integer.
#[must_use]
pub fn safe_int(x: f64) -> bool {
    x.is_finite() && x.fract() == 0.0 && x.abs() <= MAX_SAFE_INTEGER
}

/// JavaScript truthiness for a number, as used by `a || b` fallback chains.
#[must_use]
pub fn truthy(x: f64) -> bool {
    x != 0.0 && !x.is_nan()
}

/// `ToUint32` — the `>>> 0` operator. Wraps modulo 2³²; it does not saturate.
#[must_use]
pub fn to_uint32(x: f64) -> u32 {
    if !x.is_finite() || x == 0.0 {
        return 0;
    }
    let truncated = x.trunc();
    let wrapped = truncated.rem_euclid(4_294_967_296.0);
    wrapped as u32
}

/// `Number(string)` — ECMA-262 §7.1.4.1 `StringToNumber`.
///
/// Governs every numeric header and flag the program reads. Notably it accepts
/// `"0x10"` (16), `"1e3"`, `" 12 "` and `"Infinity"`, treats `""` as `0`, and
/// rejects `"inf"`, `"nan"` and `"1_0"` — all four of which
/// `f64::from_str` gets wrong in one direction or the other.
#[must_use]
pub fn to_number(s: &str) -> f64 {
    let t = text::str_white_space_trim(s);
    if t.is_empty() {
        return 0.0;
    }

    for (prefix, radix) in [
        ("0x", 16u32),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return radix_value(rest, radix);
        }
    }

    let (sign, body) = match t.as_bytes()[0] {
        b'+' => (1.0, &t[1..]),
        b'-' => (-1.0, &t[1..]),
        _ => (1.0, t),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    if !is_str_decimal_literal(body) {
        return f64::NAN;
    }
    sign * body.parse::<f64>().unwrap_or(f64::NAN)
}

/// `StrUnsignedDecimalLiteral` minus `Infinity`: digits with an optional point
/// and an optional exponent, at least one digit overall, ASCII only, and no
/// numeric separators.
fn is_str_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let mut exp_digits = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            exp_digits += 1;
        }
        if exp_digits == 0 {
            return false;
        }
    }
    i == b.len()
}

fn radix_value(s: &str, radix: u32) -> f64 {
    if s.is_empty() {
        return f64::NAN;
    }
    let mut exact: Option<u128> = Some(0);
    let mut approx = 0.0f64;
    for c in s.chars() {
        let Some(d) = c.to_digit(radix) else {
            return f64::NAN;
        };
        exact = exact.and_then(|v| v.checked_mul(u128::from(radix))?.checked_add(u128::from(d)));
        approx = approx.mul_add(f64::from(radix), f64::from(d));
    }
    exact.map_or(approx, |v| v as f64)
}

/// `parseUnsignedInteger` (`game-internal-api.ts:53`).
///
/// The two failure messages are distinct and the order matters: a 100-digit
/// string passes the pattern and fails the range check, so it gets the second
/// message, not the first.
pub fn parse_unsigned_integer(name: &str, value: &str) -> Result<f64, String> {
    // `/^\d+$/` — ASCII digits only, and JS's `$` without the `m` flag matches
    // end-of-input, so a trailing newline is a rejection.
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{name} must be an unsigned decimal integer"));
    }
    let parsed = to_number(value);
    if !safe_int(parsed) {
        return Err(format!("{name} is outside the safe integer range"));
    }
    Ok(parsed)
}
