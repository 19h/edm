//! The Frontier transport codec: `ChaCha20`, base64 and raw LZ4 blocks.
//!
//! A request envelope is `k=v&k=v` plaintext, ChaCha20-sealed under a
//! compile-time key with the twelve ASCII characters of the nonce used *as* the
//! IETF nonce, then standard-base64ed and appended raw as the query string.
//! A response runs the other way and adds two layers: an eight-byte `EDDE`
//! frame and an LZ4 block whose decompressed size arrives in a header.
//!
//! Two entry points open a body, and which one applies is decided by the HTTP
//! status, not by the body. [`open_response`] is the 2xx path and reports why
//! it failed; [`open_opaque`] is the everything-else path, which is
//! best-effort, tolerates an unframed body and never explains itself.

mod b64;
mod chacha;
mod lz4;

use std::fmt;

use zeroize::Zeroizing;

pub use b64::is_wire_base64;
pub use lz4::{Lz4Error, MAX_DECOMPRESSED};

/// The `ChaCha20` key (ts:40).
///
/// Not a secret and not derived from one: it is a literal thirty-two-character
/// digit string compiled into the game client, which is why it is a `const`
/// here rather than something loaded at run time.
pub const KEY: &[u8; 32] = b"52381239578582178380088936356181";

/// The eight-byte header Frontier wraps a compressed body in.
///
/// Only the first four bytes are ever compared. Bytes 4..8 hold a length in the
/// real format, but the TypeScript trusts the `uncompressedsize` header
/// instead and never looks — so neither do we. **[R60]**
const FRAME_MAGIC: &[u8; 4] = b"EDDE";

/// The per-request nonce: twelve ASCII hexadecimal characters.
///
/// The characters *are* the IETF nonce. Not the six bytes they spell — the
/// twelve bytes of ASCII (ts:266). **[R57]** Getting this wrong produces a
/// stream that decrypts to nothing recognisable, and it is not the kind of
/// mistake a round-trip test catches, because both directions would be wrong
/// together — so `tests/wire.rs` compares against a fixture produced by the
/// TypeScript and against the hex-decoding bug specifically.
///
/// The two constructors deliberately disagree about case, because the wire
/// does: a nonce we generate is lowercased before use, and a nonce a server
/// hands back is used exactly as received.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Nonce([u8; 12]);

/// `validateNonce` rejected a `--nonce` value or the `NONCE` environment
/// variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonceError;

impl fmt::Display for NonceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // ts:73. The TypeScript interpolates the field name, but `validateNonce`
        // has exactly one call site (ts:91) and it passes "nonce".
        f.write_str("nonce must be exactly 12 hexadecimal characters")
    }
}

impl std::error::Error for NonceError {}

impl Nonce {
    /// `validateNonce` (ts:71) — the operator-supplied nonce.
    ///
    /// Lowercases *first* and then tests `/^[0-9a-f]{12}$/`, so `"ABCDEF012345"`
    /// is accepted and the lowercased form is what goes on the wire. Full
    /// Unicode lowercasing, as `String.prototype.toLowerCase` performs; no
    /// non-ASCII character lowercases into the hexadecimal set, so this only
    /// ever widens the input by the twenty-two ASCII letters.
    pub fn parse_arg(s: &str) -> Result<Self, NonceError> {
        let lowered = s.to_lowercase();
        let bytes = lowered.as_bytes();
        let ok = bytes.len() == 12
            && bytes
                .iter()
                .all(|&b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !ok {
            return Err(NonceError);
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(bytes);
        Ok(Self(nonce))
    }

    /// The `Nonce` response header (ts:1268) — `/^[0-9a-fA-F]{12}$/`, case
    /// **preserved**.
    ///
    /// The difference from [`parse_arg`](Self::parse_arg) is the whole of
    /// **[R57]**: these characters are keystream input, so lowercasing a header
    /// that arrived uppercase would decrypt the body to garbage. The header is
    /// tested case-insensitively and then used verbatim.
    #[must_use]
    pub fn from_response_header(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != 12 || !bytes.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(bytes);
        Some(Self(nonce))
    }

    /// `randomBytes(6).toString("hex")` (ts:98) — the default when no nonce was
    /// supplied.
    ///
    /// The entropy is a parameter because this crate has none of its own.
    #[must_use]
    pub fn from_entropy(six: [u8; 6]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut nonce = [0u8; 12];
        for (i, byte) in six.iter().enumerate() {
            nonce[i * 2] = HEX[usize::from(byte >> 4)];
            nonce[i * 2 + 1] = HEX[usize::from(byte & 0x0f)];
        }
        Self(nonce)
    }

    /// The twelve characters, which are also the twelve keystream bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 12] {
        &self.0
    }

    /// The nonce as it appears in the URL and in the `Nonce` request header.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Every constructor admits ASCII hexadecimal only.
        core::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a 2xx body could not be turned back into text.
///
/// The `Display` of each variant is the message the TypeScript throws, and it
/// reaches the terminal through `Could not decrypt response: {message}`
/// (ts:1285), so the strings are part of the observable output.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The body failed the base64 gate (ts:321).
    #[error("Response is not valid standard Base64")]
    NotBase64,

    /// The keystream ran out. Unreachable below 274 GB; see **C6**.
    #[error("ChaCha20 counter exhausted")]
    CounterExhausted,

    /// The plaintext is shorter than eight bytes, or does not open with
    /// `EDDE` (ts:328).
    ///
    /// This is also where an empty 2xx body lands, because the base64 gate
    /// accepts the empty string. **[R58]**
    #[error("Decrypted response lacks the EDDE compression header")]
    MissingFrame,

    /// The LZ4 block was malformed, or declared a size we will not allocate.
    #[error(transparent)]
    Lz4(#[from] Lz4Error),

    /// The decompressed bytes are not UTF-8.
    ///
    /// **C5**: the TypeScript gets a `TypeError` from
    /// `TextDecoder("utf-8", { fatal: true })` whose text is engine-internal,
    /// so this message is ours. Reachable only on a corrupt body.
    ///
    /// Bun 1.2.3 (`JavaScriptCore`) says `Invalid byte sequence`, measured by
    /// running ts:271-331 on a block that decompresses to `FF FE`. C5 promises
    /// that string is recorded; this is the record. Node and V8 word it
    /// differently, which is why C5 exists at all.
    #[error("Response is not valid UTF-8")]
    NotUtf8,
}

impl From<chacha::CounterExhausted> for DecodeError {
    fn from(_: chacha::CounterExhausted) -> Self {
        Self::CounterExhausted
    }
}

/// `encryptEnvelope` (ts:265) — seals the request envelope and returns the
/// base64 to append to the query string.
///
/// The result is standard-alphabet, padded, and concatenated raw: no
/// percent-encoding, so `+`, `/` and `=` travel as themselves. **[R64]**
///
/// `plaintext` is UTF-8 **bytes**, already assembled and already
/// percent-encoded where the envelope wanted it. **[R65]**
///
/// # Panics
///
/// If `plaintext` is at least 256 GiB, which exhausts the 32-bit block counter.
/// The envelope is a few kilobytes of credentials and market identifiers, so
/// this cannot be reached by any input the program accepts.
#[must_use]
pub fn seal_query(plaintext: &[u8], nonce: &Nonce) -> String {
    // C12: the plaintext carries the auth and machine tokens. We keep only the
    // ciphertext; the working copy is wiped when this returns. The TypeScript
    // reads nothing but `.length` from its own copy, so nothing observable
    // depends on it surviving.
    let mut buffer = Zeroizing::new(plaintext.to_vec());
    chacha::apply(&mut buffer, nonce.as_bytes()).expect("envelope is far below 256 GiB");
    b64::encode(&buffer)
}

/// `decryptResponse` (ts:319) — opens a 2xx body.
///
/// The pipeline is base64 gate, `ChaCha20`, `EDDE` frame, LZ4 block, strict
/// UTF-8. `uncompressed` is the `uncompressedsize` response header, which the
/// caller has already checked is a safe integer greater than zero (ts:1275);
/// it is both the allocation size and the acceptance criterion for the block.
pub fn open_response(
    body: &str,
    nonce: &Nonce,
    uncompressed: usize,
) -> Result<String, DecodeError> {
    let compact = crate::js::text::js_trim(body);
    if !is_wire_base64(compact) {
        return Err(DecodeError::NotBase64);
    }
    // Unreachable given the gate, and pinned as such by a property test in
    // `b64`; reported as a base64 rejection rather than assumed away.
    let mut buffer = b64::decode(compact).ok_or(DecodeError::NotBase64)?;

    chacha::apply(&mut buffer, nonce.as_bytes())?;
    let block = strip_frame(&buffer).ok_or(DecodeError::MissingFrame)?;
    let plaintext = lz4::decompress(block, uncompressed)?;
    decode_utf8_fatal(&plaintext)
}

/// `decodeOpaqueBody` (ts:2819) — best-effort decode of a non-2xx body.
///
/// Failure bodies are encrypted too, so this exists to print something more
/// useful than base64. It differs from [`open_response`] in three ways, all of
/// them **[R59]**:
///
/// * the empty-string test comes **first**, so an empty error body yields
///   `None` here where a 2xx body would report a missing frame;
/// * an unframed plaintext is accepted, and the **whole** buffer is decoded —
///   there is no eight-byte skip;
/// * the size header is never read on that unframed path, so a malformed or
///   absent `uncompressedsize` does not stop an unframed body from printing.
///
/// Everything else — a bad nonce, a bad block, invalid UTF-8 — is swallowed,
/// matching the TypeScript's bare `catch`.
#[must_use]
pub fn open_opaque(body: &str, nonce_header: &str, size_header: Option<&str>) -> Option<String> {
    let compact = crate::js::text::js_trim(body);
    if compact.is_empty() || !is_wire_base64(compact) {
        return None;
    }
    let nonce = Nonce::from_response_header(nonce_header)?;

    let mut buffer = b64::decode(compact)?;
    chacha::apply(&mut buffer, nonce.as_bytes()).ok()?;

    let Some(block) = strip_frame(&buffer) else {
        return decode_utf8_fatal(&buffer).ok();
    };

    // `Number(null)` is 0, so an absent header is a rejection rather than a
    // reason to guess. [R10]
    let size = size_header.map_or(0.0, crate::js::to_number);
    if !crate::js::safe_int(size) || size <= 0.0 {
        return None;
    }
    let expected = usize::try_from(size as u64).ok()?;

    decode_utf8_fatal(&lz4::decompress(block, expected).ok()?).ok()
}

/// The `EDDE` frame test, and the payload behind it.
///
/// Length first, then the magic — an eight-byte minimum even though only four
/// bytes are compared, so a four-byte body that happens to spell `EDDE` is
/// still rejected. **[R60]**
fn strip_frame(plaintext: &[u8]) -> Option<&[u8]> {
    if plaintext.len() >= 8 && &plaintext[..4] == FRAME_MAGIC {
        Some(&plaintext[8..])
    } else {
        None
    }
}

/// `new TextDecoder("utf-8", { fatal: true }).decode(..)` (ts:331).
///
/// Two halves, and both are load-bearing. `fatal: true` means an invalid
/// sequence is an error rather than a run of replacement characters. And
/// `ignoreBOM` defaults to **false**, which — against the intuition its name
/// invites — means the decoder *removes* one leading U+FEFF. **[R63]** Skipping
/// the strip leaves a zero-width no-break space at the head of the string,
/// which then survives into `JSON.parse` and makes it fail.
fn decode_utf8_fatal(bytes: &[u8]) -> Result<String, DecodeError> {
    let text = core::str::from_utf8(bytes).map_err(|_| DecodeError::NotUtf8)?;
    Ok(text.strip_prefix('\u{FEFF}').unwrap_or(text).to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Nonce, decode_utf8_fatal, strip_frame};

    #[test]
    fn entropy_becomes_lowercase_hex() {
        let nonce = Nonce::from_entropy([0x00, 0x0f, 0xa5, 0xff, 0x10, 0x9c]);
        assert_eq!(nonce.as_str(), "000fa5ff109c");
    }

    #[test]
    fn parse_arg_lowercases_before_testing() {
        assert_eq!(
            Nonce::parse_arg("ABCDEF012345").unwrap().as_str(),
            "abcdef012345"
        );
        assert_eq!(
            Nonce::parse_arg("abcdef012345").unwrap().as_str(),
            "abcdef012345"
        );
    }

    #[test]
    fn parse_arg_rejects_non_hex_and_wrong_lengths() {
        // The last of these is twelve *characters* of fullwidth digits, which
        // `toLowerCase` leaves alone and the pattern rejects. [R41]
        let bad = [
            "",
            "abcdef01234",
            "abcdef0123456",
            "abcdefg12345",
            "abcdef 12345",
            "０１２３４５６７８９ab",
        ];
        for value in bad {
            assert!(
                Nonce::parse_arg(value).is_err(),
                "{value:?} should not parse"
            );
        }
    }

    #[test]
    fn parse_arg_message_matches_the_typescript() {
        assert_eq!(
            Nonce::parse_arg("nope").unwrap_err().to_string(),
            "nonce must be exactly 12 hexadecimal characters"
        );
    }

    #[test]
    fn response_header_preserves_case() {
        assert_eq!(
            Nonce::from_response_header("ABCDEF012345")
                .unwrap()
                .as_str(),
            "ABCDEF012345"
        );
        assert_eq!(
            Nonce::from_response_header("aBcDeF012345")
                .unwrap()
                .as_str(),
            "aBcDeF012345"
        );
        assert!(Nonce::from_response_header("abcdefg12345").is_none());
        assert!(Nonce::from_response_header("abcdef01234").is_none());
    }

    #[test]
    fn frame_is_length_checked_before_the_magic() {
        // Four bytes of magic and nothing else is still too short. [R60]
        assert_eq!(strip_frame(b"EDDE"), None);
        assert_eq!(strip_frame(b"EDDE\0\0\0"), None);
        assert_eq!(strip_frame(b"EDDE\0\0\0\0"), Some(&b""[..]));
        assert_eq!(
            strip_frame(b"edde\0\0\0\0"),
            None,
            "the magic is case-sensitive"
        );
    }

    #[test]
    fn frame_never_inspects_the_length_word() {
        // Bytes 4..8 are garbage here and it makes no difference. [R60]
        assert_eq!(
            strip_frame(b"EDDE\xff\xff\xff\xffpayload"),
            Some(&b"payload"[..])
        );
    }

    #[test]
    fn one_bom_is_stripped_and_only_one() {
        assert_eq!(
            decode_utf8_fatal("\u{FEFF}{}".as_bytes()).as_deref(),
            Ok("{}")
        );
        assert_eq!(
            decode_utf8_fatal("\u{FEFF}\u{FEFF}{}".as_bytes()).as_deref(),
            Ok("\u{FEFF}{}")
        );
        assert_eq!(
            decode_utf8_fatal("{\u{FEFF}}".as_bytes()).as_deref(),
            Ok("{\u{FEFF}}")
        );
    }
}
