//! The Frontier wire codec, against `market-request.ts`.
//!
//! Every base64 constant below was produced by running the TypeScript's own
//! `encryptEnvelope`, `decryptResponse`, `decompressLz4Block` and
//! `decodeOpaqueBody` (ts:114-191, 265-335, 2819-2837) under Bun 1.2.3 and
//! copying the answer. They are golden values, not derived ones: a change to
//! any layer of the codec moves them, and none of them can be re-derived from
//! anything else in this file.

use edm_core::wire::{
    DecodeError, Lz4Error, MAX_DECOMPRESSED, Nonce, is_wire_base64, open_opaque, open_response,
    seal_query,
};

/// The nonce most of the response fixtures were sealed under.
const NONCE: &str = "0f1e2d3c4b5a";

/// `{"name":"Jameson Memorial"}` — 27 bytes, framed and sealed under [`NONCE`].
const JAMESON_BODY: &str = "kJLbAonOvwSdh7dH8YnvnRV9XZ0eTByIBgVNllGjhZB4tmyQnQ==";
const JAMESON_TEXT: &str = r#"{"name":"Jameson Memorial"}"#;
const JAMESON_SIZE: usize = 27;

fn nonce(text: &str) -> Nonce {
    Nonce::parse_arg(text).expect("fixture nonce is twelve hex characters")
}

// ---------------------------------------------------------------------------
// The nonce is the characters, not the bytes
// ---------------------------------------------------------------------------

/// **[R57]**, the mistake this port exists to not make.
///
/// `"0123456789ab"` is twelve ASCII characters *and* a hexadecimal spelling of
/// six bytes. Frontier's client uses the characters. An implementation that
/// decodes the hex first — the obvious reading of "nonce" — produces a
/// completely different stream, and because the same wrong nonce would be used
/// in both directions, a round-trip test would pass with the wire broken. So
/// this compares against the TypeScript's answer and against the wrong one.
#[test]
fn nonce_is_the_ascii_characters() {
    let ours = seal_query(b"hello world", &nonce("0123456789ab"));
    assert_eq!(ours, "OY34r5co34DSbwc=", "the TypeScript's encryptEnvelope output");

    // What decoding the hex would give: six bytes, zero-padded to the IETF
    // twelve. This is the natural bug and it must not agree.
    let decoded_nonce = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0, 0, 0, 0, 0, 0];
    let wrong = {
        use base64::Engine as _;
        use cipher::{KeyIvInit, StreamCipher};
        let mut buffer = b"hello world".to_vec();
        let key = edm_core::wire::KEY;
        let mut cipher = chacha20::ChaCha20::new(key.into(), (&decoded_nonce).into());
        cipher.apply_keystream(&mut buffer);
        base64::engine::general_purpose::STANDARD.encode(&buffer)
    };
    assert_ne!(ours, wrong, "hex-decoding the nonce must change the ciphertext");
}

/// **[R57]** again, from the other end. `validateNonce` lowercases (ts:72) but
/// the response header does not (ts:1268), and this is why: the header's
/// characters are keystream input, so `"ABCDEF012345"` and `"abcdef012345"` are
/// different keys. Normalising the response nonce would silently corrupt every
/// body a server chose to answer in upper case.
#[test]
fn response_nonce_case_is_keystream_relevant() {
    // The same plaintext, sealed twice: once under the lower-case nonce and
    // once under the upper-case one. Both are the TypeScript's output.
    const UPPER_BODY: &str = "FJWkv6qTUvB8C0ZWmCK1Xp3dWcMgGZmGAZ/YrQFvR6GVnaqb1w==";
    assert_ne!(JAMESON_BODY, UPPER_BODY);

    let lower = Nonce::from_response_header("0f1e2d3c4b5a").expect("valid header");
    let upper = Nonce::from_response_header("0F1E2D3C4B5A").expect("valid header");
    assert_eq!(open_response(JAMESON_BODY, &lower, JAMESON_SIZE).as_deref(), Ok(JAMESON_TEXT));
    assert_eq!(open_response(UPPER_BODY, &upper, JAMESON_SIZE).as_deref(), Ok(JAMESON_TEXT));

    // Swap them and the plaintext is unrecognisable — it does not even reach
    // the LZ4 layer.
    assert_eq!(open_response(JAMESON_BODY, &upper, JAMESON_SIZE), Err(DecodeError::MissingFrame));
    assert_eq!(open_response(UPPER_BODY, &lower, JAMESON_SIZE), Err(DecodeError::MissingFrame));
}

/// `parse_arg` accepts upper case and folds it, because a *request* nonce is
/// ours to normalise (ts:72). The two constructors must therefore disagree on
/// this input, and that disagreement is the point.
#[test]
fn the_two_constructors_disagree_on_case() {
    assert_eq!(nonce("ABCDEF012345").as_str(), "abcdef012345");
    assert_eq!(Nonce::from_response_header("ABCDEF012345").unwrap().as_str(), "ABCDEF012345");
    assert_ne!(
        seal_query(b"x", &nonce("ABCDEF012345")),
        seal_query(b"x", &Nonce::from_response_header("ABCDEF012345").unwrap()),
    );
}

// ---------------------------------------------------------------------------
// seal_query
// ---------------------------------------------------------------------------

#[test]
fn seals_an_envelope_byte_for_byte() {
    let plaintext = "commanderId=F1234567&machineId=abc&marketId=3229625088&nonce=a1b2c3d4e5f6";
    assert_eq!(
        seal_query(plaintext.as_bytes(), &nonce("a1b2c3d4e5f6")),
        "gS9P3f64ZZxi9XhfXoH5ZfDjFeaajlIBUFe93EB37eoAsEJ7imcvl3Rvyn2lZZbnk1xZ8gZ4DH/gzIOd7ON8e2WjLwBDOu78og==",
    );
}

/// **[R65]** — the envelope plaintext is UTF-8 bytes. `--language` is the one
/// field nothing validates, so non-ASCII really can get here.
#[test]
fn seals_utf8_plaintext() {
    assert_eq!(
        seal_query("système & 日本語".as_bytes(), &nonce("deadbeef0011")),
        "X3aZu/OqZPqAXtp+rirPfj4bChU=",
    );
}

#[test]
fn seals_an_empty_envelope_to_an_empty_string() {
    assert_eq!(seal_query(b"", &nonce("000000000000")), "");
}

/// **[R64]** — standard alphabet, padded, and appended to the query with no
/// percent-encoding, so these three characters travel raw.
#[test]
fn sealed_output_is_standard_padded_base64() {
    // 0xfb 0xff encodes to `+/8=` under the standard alphabet and to `-_8=`
    // under the URL-safe one.
    let sealed = seal_query(b"\x00\x00", &nonce("000000000000"));
    assert!(is_wire_base64(&sealed));
    assert!(sealed.ends_with('='), "two bytes must produce one pad character: {sealed}");

    let long = seal_query(&[0u8; 96], &nonce("abcdef012345"));
    assert!(long.contains('+') || long.contains('/'), "no URL-safe substitution: {long}");
}

/// ChaCha20 is XOR against a keystream, so the two directions really are one
/// function. This is also what lets the LZ4 vectors below reach the decoder
/// through the public API.
#[test]
fn sealing_and_opening_are_inverses() {
    let mut framed = b"EDDE\0\0\0\0".to_vec();
    framed.extend_from_slice(&[0x30, b'a', b'b', b'c']);
    let body = seal_query(&framed, &nonce(NONCE));
    assert_eq!(open_response(&body, &nonce(NONCE), 3).as_deref(), Ok("abc"));
}

// ---------------------------------------------------------------------------
// open_response
// ---------------------------------------------------------------------------

#[test]
fn opens_a_real_response() {
    let opened = open_response(JAMESON_BODY, &nonce(NONCE), JAMESON_SIZE);
    assert_eq!(opened.as_deref(), Ok(JAMESON_TEXT));
}

/// `decryptResponse` trims first (ts:319), with `String.prototype.trim`'s
/// character set — which includes U+FEFF. **[R25]**
#[test]
fn the_body_is_js_trimmed_first() {
    let padded = format!("  \t\r\n{JAMESON_BODY} \u{FEFF}\u{2028}");
    assert_eq!(open_response(&padded, &nonce(NONCE), JAMESON_SIZE).as_deref(), Ok(JAMESON_TEXT));
}

/// **[R58]** — the gate's `*` quantifier accepts the empty string, so an empty
/// 2xx body sails past it and dies at the frame check. Reporting a base64
/// failure here would be the wrong message on a real, observable path: a 200
/// with no body.
#[test]
fn an_empty_2xx_body_reports_a_missing_frame_not_bad_base64() {
    assert_eq!(open_response("", &nonce(NONCE), 10), Err(DecodeError::MissingFrame));
    assert_eq!(open_response("   ", &nonce(NONCE), 10), Err(DecodeError::MissingFrame));
    assert_eq!(
        open_response("", &nonce(NONCE), 10).unwrap_err().to_string(),
        "Decrypted response lacks the EDDE compression header",
    );
}

#[test]
fn rejects_non_base64_bodies() {
    for body in ["!!!!", "QUJ", "QU-_", "Q===", "QU==QUJD", "QUJ D"] {
        assert_eq!(
            open_response(body, &nonce(NONCE), 10),
            Err(DecodeError::NotBase64),
            "{body:?} should fail the gate",
        );
    }
    assert_eq!(
        open_response("!!!!", &nonce(NONCE), 10).unwrap_err().to_string(),
        "Response is not valid standard Base64",
    );
}

/// **[R60]** — length first, then magic. A body that decrypts to fewer than
/// eight bytes is rejected before the four magic bytes are compared, so a
/// four-byte `EDDE` is still a missing frame.
#[test]
fn the_frame_check_is_length_first() {
    let short = seal_query(b"EDDE", &nonce(NONCE));
    assert_eq!(open_response(&short, &nonce(NONCE), 10), Err(DecodeError::MissingFrame));

    let wrong_magic = seal_query(b"EDDF\0\0\0\0\x00", &nonce(NONCE));
    assert_eq!(open_response(&wrong_magic, &nonce(NONCE), 10), Err(DecodeError::MissingFrame));
}

/// **[R60]** — bytes 4..8 are the uncompressed length in the real format, and
/// the TypeScript never reads them. Filling them with garbage must change
/// nothing, because the `uncompressedsize` header is the only size that counts.
#[test]
fn the_frame_length_word_is_never_inspected() {
    let mut framed = b"EDDE\xde\xad\xbe\xef".to_vec();
    framed.extend_from_slice(&[0x30, b'a', b'b', b'c']);
    let body = seal_query(&framed, &nonce(NONCE));
    assert_eq!(open_response(&body, &nonce(NONCE), 3).as_deref(), Ok("abc"));
}

/// **[R63]** — `TextDecoder`'s `ignoreBOM` defaults to false, which *removes* a
/// leading U+FEFF rather than preserving it. One, and only one.
#[test]
fn one_leading_bom_is_removed() {
    // The sealed payload is U+FEFF U+FEFF "ok" — eight UTF-8 bytes.
    const BOM_BODY: &str = "kJLbAonOvwTtZHfacFM9l1w=";
    assert_eq!(open_response(BOM_BODY, &nonce(NONCE), 8).as_deref(), Ok("\u{FEFF}ok"));
}

/// **C5** — our message, not Bun's `Invalid byte sequence`.
#[test]
fn invalid_utf8_is_rejected_rather_than_replaced() {
    let mut framed = b"EDDE\0\0\0\0".to_vec();
    framed.extend_from_slice(&[0x20, 0xff, 0xfe]);
    let body = seal_query(&framed, &nonce(NONCE));
    let err = open_response(&body, &nonce(NONCE), 2).expect_err("0xff 0xfe is not UTF-8");
    assert_eq!(err, DecodeError::NotUtf8);
    assert_eq!(err.to_string(), "Response is not valid UTF-8");
}

// ---------------------------------------------------------------------------
// LZ4 — nine that decode and nine that do not
// ---------------------------------------------------------------------------

/// Frames and seals a raw LZ4 block, then opens it: the only route to the
/// decompressor through the public API, and the same route a response takes.
fn open_block(block: &[u8], expected: usize) -> Result<String, DecodeError> {
    let mut framed = b"EDDE\0\0\0\0".to_vec();
    framed.extend_from_slice(block);
    open_response(&seal_query(&framed, &nonce(NONCE)), &nonce(NONCE), expected)
}

fn accepts(block: &[u8], expected: usize, text: &str) {
    assert_eq!(open_block(block, expected).as_deref(), Ok(text));
}

fn rejects(block: &[u8], expected: usize, message: &str) {
    let err = open_block(block, expected).expect_err("block should have been refused");
    assert_eq!(err.to_string(), message);
    assert!(matches!(err, DecodeError::Lz4(_)), "should be an LZ4 failure, got {err:?}");
}

#[test]
fn lz4_accepts_an_empty_block() {
    // The `while` loop never runs and the size check passes at zero.
    accepts(&[], 0, "");
}

#[test]
fn lz4_accepts_a_literals_only_block() {
    // Token 0x30: three literals, no match. `source === input.length` after the
    // literals, so the loop breaks before looking for an offset.
    accepts(&[0x30, b'a', b'b', b'c'], 3, "abc");
}

#[test]
fn lz4_accepts_an_overlapping_match() {
    // One literal, then a four-byte match at offset 1 — a run. The offset is
    // smaller than the match, so the copy must proceed a byte at a time.
    accepts(&[0x10, b'a', 0x01, 0x00], 5, "aaaaa");
}

#[test]
fn lz4_accepts_a_non_overlapping_match() {
    // Offset 4 and match length 4: the source and destination ranges abut but
    // do not overlap, so a block copy is legitimate here.
    accepts(&[0x40, b'a', b'b', b'c', b'd', 0x04, 0x00], 8, "abcdabcd");
}

#[test]
fn lz4_accepts_a_partially_overlapping_match() {
    // Offset 2, match length 6: the copy reads bytes it is itself writing.
    accepts(&[0x22, b'a', b'b', 0x02, 0x00], 8, "abababab");
}

#[test]
fn lz4_accepts_an_extended_literal_length() {
    // Nibble 15 means "read more"; the extension byte 5 makes it 20.
    let mut block = vec![0xf0, 5];
    block.extend(std::iter::repeat_n(b'A', 20));
    accepts(&block, 20, &"A".repeat(20));
}

#[test]
fn lz4_accepts_a_255_length_continuation() {
    // The run continues while the byte is 255 and the terminating byte counts
    // too: 15 + 255 + 0 = 270.
    let mut block = vec![0xf0, 255, 0];
    block.extend(std::iter::repeat_n(b'B', 270));
    accepts(&block, 270, &"B".repeat(270));
}

#[test]
fn lz4_accepts_an_extended_match_length() {
    // Match nibble 15, extension 1, plus the constant 4 = 20.
    accepts(&[0x1f, b'a', 0x01, 0x00, 0x01], 21, &"a".repeat(21));
}

#[test]
fn lz4_accepts_fewer_than_five_trailing_literals() {
    // The LZ4 format reserves the last five bytes as literals and a real
    // compressor honours it. The TypeScript does not check, so a block ending
    // in two literals decodes rather than failing.
    accepts(&[0x40, b'a', b'b', b'c', b'd', 0x04, 0x00, 0x20, b'x', b'y'], 10, "abcdabcdxy");
}

#[test]
fn lz4_rejects_a_truncated_literal_extension() {
    // Nibble 15 promises an extension byte that is not there.
    rejects(&[0xf0], 10, "Truncated LZ4 length");
}

#[test]
fn lz4_rejects_a_truncated_match_extension() {
    // The same message from the other reader: the match nibble is 15 and the
    // block ends.
    rejects(&[0x1f, b'a', 0x01, 0x00], 30, "Truncated LZ4 length");
}

#[test]
fn lz4_rejects_literals_that_overrun_the_input() {
    rejects(&[0x50, b'a', b'b'], 10, "Invalid LZ4 literal length");
}

#[test]
fn lz4_rejects_literals_that_overrun_the_output() {
    // The second half of the same condition, and the reason `uncompressedsize`
    // is a bound and not just an allocation hint.
    rejects(&[0x30, b'a', b'b', b'c'], 2, "Invalid LZ4 literal length");
}

#[test]
fn lz4_rejects_a_truncated_match_offset() {
    // One byte where two are needed. Distinct from a clean end-of-block, which
    // is checked first.
    rejects(&[0x10, b'a', 0x01], 5, "Truncated LZ4 match offset");
}

#[test]
fn lz4_rejects_a_zero_match_offset() {
    // The check `lz4_flex` gets wrong. A zero offset would read the byte being
    // written and emit a run of whatever the buffer already held.
    rejects(&[0x10, b'a', 0x00, 0x00], 5, "Invalid LZ4 match offset");
}

#[test]
fn lz4_rejects_an_offset_past_the_running_destination() {
    // Offset 2 with only one byte produced. The bound is the *running*
    // position, not the declared size, so this fails even though the output
    // buffer is five bytes long.
    rejects(&[0x10, b'a', 0x02, 0x00], 5, "Invalid LZ4 match offset");
}

#[test]
fn lz4_rejects_a_match_that_overruns_the_output() {
    // The implicit `+ 4` is added before the bound check: 1 + 4 > 3.
    rejects(&[0x10, b'a', 0x01, 0x00], 3, "Invalid LZ4 match length");
}

#[test]
fn lz4_rejects_a_short_block() {
    // Decodes cleanly, just not to the declared size. Both numbers are in the
    // message and both are interpolated ungrouped.
    rejects(&[0x30, b'a', b'b', b'c'], 4, "LZ4 size mismatch: expected 4, produced 3");
}

/// The six transcribed strings, asserted directly. Anything that edits them is
/// changing what a user sees.
#[test]
fn lz4_messages_are_the_typescripts() {
    assert_eq!(Lz4Error::TruncatedLength.to_string(), "Truncated LZ4 length");
    assert_eq!(Lz4Error::InvalidLiteralLength.to_string(), "Invalid LZ4 literal length");
    assert_eq!(Lz4Error::TruncatedMatchOffset.to_string(), "Truncated LZ4 match offset");
    assert_eq!(Lz4Error::InvalidMatchOffset.to_string(), "Invalid LZ4 match offset");
    assert_eq!(Lz4Error::InvalidMatchLength.to_string(), "Invalid LZ4 match length");
    assert_eq!(
        Lz4Error::SizeMismatch { expected: 1_048_576, produced: 0 }.to_string(),
        "LZ4 size mismatch: expected 1048576, produced 0",
    );
}

/// **C4** — `uncompressedsize` is a response header, so the allocation it asks
/// for is chosen by whoever answered. The TypeScript throws a catchable
/// `RangeError`; `vec![0; n]` would abort the process instead, taking any
/// in-flight sweep with it.
#[test]
fn an_absurd_uncompressed_size_is_refused_not_allocated() {
    let err = open_block(&[], MAX_DECOMPRESSED + 1).expect_err("over the cap");
    assert_eq!(err, DecodeError::Lz4(Lz4Error::Allocation { requested: MAX_DECOMPRESSED + 1 }));
}

// ---------------------------------------------------------------------------
// open_opaque
// ---------------------------------------------------------------------------

/// A 27-byte error string sealed under `aabbccddeeff` with **no** frame — the
/// shape a Frontier error body actually takes.
const UNFRAMED_BODY: &str = "+LLtakRVdSmC0XcP3G1EMhYk/yWasDIlFyO+";
const UNFRAMED_NONCE: &str = "aabbccddeeff";

/// **[R59]** — the empty-string test comes first. `open_response` would call
/// this a missing frame; here it is simply nothing to print, and the caller
/// falls through to echoing the raw body (ts:1264).
#[test]
fn an_empty_opaque_body_is_checked_before_anything_else() {
    assert_eq!(open_opaque("", UNFRAMED_NONCE, Some("27")), None);
    assert_eq!(open_opaque("   \n ", UNFRAMED_NONCE, Some("27")), None);
    // Same input, the other entry point, a different answer. [R58][R59]
    assert_eq!(open_response("", &nonce(NONCE), 27), Err(DecodeError::MissingFrame));
}

/// **[R59]** — an unframed body is decoded whole. No eight-byte skip, and the
/// size header is not consulted at all, so a missing or nonsensical one cannot
/// suppress the diagnostic.
#[test]
fn an_unframed_body_decodes_whole_and_ignores_the_size_header() {
    let expected = Some("Unauthorized: token expired".to_owned());
    assert_eq!(open_opaque(UNFRAMED_BODY, UNFRAMED_NONCE, None), expected);
    assert_eq!(open_opaque(UNFRAMED_BODY, UNFRAMED_NONCE, Some("not a number")), expected);
    assert_eq!(open_opaque(UNFRAMED_BODY, UNFRAMED_NONCE, Some("0")), expected);
    assert_eq!(open_opaque(UNFRAMED_BODY, UNFRAMED_NONCE, Some("")), expected);
}

/// A framed error body takes the full LZ4 path, and *there* the size header
/// decides.
#[test]
fn a_framed_opaque_body_needs_the_size_header() {
    assert_eq!(
        open_opaque(JAMESON_BODY, NONCE, Some("27")),
        Some(JAMESON_TEXT.to_owned()),
    );
    assert_eq!(open_opaque(JAMESON_BODY, NONCE, None), None);
    assert_eq!(open_opaque(JAMESON_BODY, NONCE, Some("0")), None);
    assert_eq!(open_opaque(JAMESON_BODY, NONCE, Some("-1")), None);
    assert_eq!(open_opaque(JAMESON_BODY, NONCE, Some("27.5")), None);
    assert_eq!(open_opaque(JAMESON_BODY, NONCE, Some("9007199254740993")), None);
}

/// The size header goes through `Number()`, not a decimal parser. **[R10]**
#[test]
fn the_size_header_uses_javascript_number_coercion() {
    // ` 0x1b ` is 27: whitespace-trimmed, then read as hexadecimal.
    assert_eq!(
        open_opaque(JAMESON_BODY, NONCE, Some(" 0x1b ")),
        Some(JAMESON_TEXT.to_owned()),
    );
    assert_eq!(open_opaque(JAMESON_BODY, NONCE, Some("2.7e1")), Some(JAMESON_TEXT.to_owned()));
}

/// The response-header nonce rule, applied at the opaque entry point (ts:2830).
#[test]
fn an_opaque_nonce_is_case_preserving_and_gated() {
    // Sealed under the upper-case nonce and read back with it.
    const UPPER: &str = "7IEQXw==";
    assert_eq!(open_opaque(UPPER, "AABBCCDDEEFF", None), Some("nope".to_owned()));
    assert_eq!(open_opaque(UPPER, "aabbccddeeff", None), None, "case must not be folded");

    for bad in ["", "zzzzzzzzzzzz", "aabbccddeef", "aabbccddeeff0", "aabbccddee ff"] {
        assert_eq!(open_opaque(UNFRAMED_BODY, bad, None), None, "{bad:?} is not a nonce");
    }
}

#[test]
fn opaque_swallows_every_other_failure() {
    assert_eq!(open_opaque("!!!!", UNFRAMED_NONCE, None), None, "gate failure");
    assert_eq!(open_opaque("QUJ", UNFRAMED_NONCE, None), None, "length failure");
    // Four bytes that decrypt to 0xff 0xfe 0xfd 0xfc — not UTF-8, and not an
    // error the user is told about.
    let invalid_utf8 = seal_query(&[0xff, 0xfe, 0xfd, 0xfc], &nonce(UNFRAMED_NONCE));
    assert_eq!(open_opaque(&invalid_utf8, UNFRAMED_NONCE, None), None);
}

#[test]
fn opaque_bodies_are_js_trimmed_too() {
    let padded = format!("\u{FEFF}\n{UNFRAMED_BODY}\t ");
    assert_eq!(
        open_opaque(&padded, UNFRAMED_NONCE, None),
        Some("Unauthorized: token expired".to_owned()),
    );
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// **[R58]** — the gate is the public predicate behind both entry points, and
/// its acceptance of the empty string is load-bearing.
#[test]
fn the_base64_gate_matches_the_regular_expression() {
    for good in ["", "QUJD", "QUI=", "QQ==", "+/+/", "QUJDQUJD"] {
        assert!(is_wire_base64(good), "{good:?} should pass");
    }
    for bad in ["QUJ", "Q===", "====", "=QUJ", "QU==QUJD", "QU-_", "QU J=", "QUJ\n", "QUJD "] {
        assert!(!is_wire_base64(bad), "{bad:?} should fail");
    }
}
