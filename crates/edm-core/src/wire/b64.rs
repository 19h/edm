//! The base64 half of the wire codec: a strict gate in front of a lenient
//! decoder.
//!
//! The asymmetry is the whole point and is inherited from the TypeScript
//! (ts:319-323): a regular expression decides *whether* to decode, and Node's
//! `Buffer.from(s, "base64")` decides *what* comes out. The regular expression
//! is stricter than the decoder about shape and the decoder is more forgiving
//! than the regular expression about content, so neither alone reproduces the
//! pair.

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, general_purpose};

/// `Buffer.from(s, "base64")` for input that has already passed
/// [`is_wire_base64`].
///
/// Node truncates the final partial group rather than rejecting it, so the bits
/// below the last whole byte are simply dropped: `"QR=="` is one byte, `0x41`,
/// even though `R`'s low four bits are not zero. `decode_allow_trailing_bits`
/// is what buys that; the default engine would call it a corrupt symbol.
/// `DecodePaddingMode::Indifferent` is belt-and-braces — the gate has already
/// fixed the padding — and keeps the two rules from disagreeing about a group
/// the gate accepted.
const LENIENT: general_purpose::GeneralPurpose = general_purpose::GeneralPurpose::new(
    &alphabet::STANDARD,
    general_purpose::GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The response-body gate, `/^[A-Za-z0-9+\/]*={0,2}$/` plus `length % 4 === 0`
/// (ts:320-322). **[R58]**
///
/// Standard alphabet only — a URL-safe body is rejected outright, not
/// re-mapped. Note that it accepts the empty string: the two callers differ on
/// what that means, and neither of them may decide it here.
///
/// The order of the two conditions is observable in the TypeScript only in that
/// the pattern short-circuits the length test, which is why measuring UTF-16
/// units versus bytes cannot matter: anything reaching the modulo is ASCII.
#[must_use]
pub fn is_wire_base64(s: &str) -> bool {
    let bytes = s.as_bytes();

    // `={0,2}` is anchored to `$`, so every `=` in the string must be part of
    // one trailing run of at most two. Counting the run and then demanding that
    // everything before it is alphabet (which excludes `=`) is the same
    // predicate: it rejects `"AA==AAAA"` and `"=AAA"` for the same reason the
    // regular expression does.
    let padding = bytes.iter().rev().take_while(|&&b| b == b'=').count();
    if padding > 2 {
        return false;
    }
    let body = &bytes[..bytes.len() - padding];
    if !body.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/') {
        return false;
    }

    bytes.len().is_multiple_of(4)
}

/// Decodes gated input.
///
/// `None` is unreachable for anything [`is_wire_base64`] accepts — the gate
/// forces a whole number of four-symbol groups over the standard alphabet, and
/// leniency absorbs the rest. `wire_base64_gate_implies_a_successful_decode`
/// holds that claim down; callers still surface it as a base64 rejection rather
/// than assume it.
pub(super) fn decode(s: &str) -> Option<Vec<u8>> {
    LENIENT.decode(s).ok()
}

/// Standard, padded base64 — the request direction. **[R64]**
///
/// Deliberately not the URL-safe alphabet and deliberately not
/// percent-encoded: the TypeScript concatenates this straight onto the query
/// string, so `+`, `/` and `=` travel raw.
pub(super) fn encode(bytes: &[u8]) -> String {
    general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{decode, encode, is_wire_base64};

    #[test]
    fn the_gate_accepts_the_empty_string() {
        // The empty string is where the two callers part company: a 2xx body
        // must fall through to the missing-frame error, a non-2xx body must
        // stop before this. [R58][R59]
        assert!(is_wire_base64(""));
        assert_eq!(decode(""), Some(Vec::new()));
    }

    #[test]
    fn accepts_canonical_and_padded_groups() {
        assert!(is_wire_base64("QUJD"));
        assert!(is_wire_base64("QUI="));
        assert!(is_wire_base64("QQ=="));
        assert!(is_wire_base64("+/+/"));
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(!is_wire_base64("QUJ"), "length not a multiple of four");
        assert!(!is_wire_base64("Q==="), "three padding characters");
        assert!(!is_wire_base64("===="), "four padding characters");
        assert!(!is_wire_base64("=QUJ"), "padding before the body");
        assert!(!is_wire_base64("QU==QUJD"), "padding inside the body");
        assert!(!is_wire_base64("QU-_"), "url-safe alphabet");
        assert!(!is_wire_base64("QU J="), "embedded space");
        assert!(!is_wire_base64("QUJ\n"), "trailing newline: `$` is end-of-input");
    }

    /// Node keeps the whole byte and throws the stray bits away rather than
    /// reporting a corrupt symbol, which strict decoders do not. [R58]
    #[test]
    fn non_zero_trailing_bits_decode_leniently() {
        assert_eq!(decode("QR=="), Some(vec![0x41]));
        // `QUJDRB` and `QUJDRA` differ only in bits the fourth byte cannot
        // hold, and both survive.
        assert_eq!(decode("QUJDRA=="), Some(b"ABCD".to_vec()));
        assert_eq!(decode("QUJDRB=="), Some(b"ABCD".to_vec()));
    }

    #[test]
    fn encoding_is_standard_and_padded() {
        assert_eq!(encode(b"\xfb\xff"), "+/8=");
        assert_eq!(encode(b"A"), "QQ==");
        assert_eq!(encode(b""), "");
    }

    proptest! {
        /// The gate is the only guard the decoder gets, so anything it lets
        /// through must decode. This is what lets the callers treat a decode
        /// failure as unreachable. [R58]
        #[test]
        fn wire_base64_gate_implies_a_successful_decode(s in "[A-Za-z0-9+/=]{0,64}") {
            if is_wire_base64(&s) {
                prop_assert!(decode(&s).is_some(), "gate accepted {s:?} but the decode failed");
            }
        }

        /// Our own encoder's output always satisfies our own gate, so a sealed
        /// envelope could be fed back through the response path.
        #[test]
        fn encoding_round_trips_through_the_gate(
            bytes in prop::collection::vec(any::<u8>(), 0..300)
        ) {
            let encoded = encode(&bytes);
            prop_assert!(is_wire_base64(&encoded));
            prop_assert_eq!(decode(&encoded), Some(bytes));
        }
    }
}
