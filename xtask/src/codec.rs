//! Body construction for the mock, and envelope recovery for its router.
//!
//! The mock has to *produce* what `edm_core::wire` consumes: an LZ4 block
//! behind an `EDDE` frame, ChaCha20-sealed and base64ed. It also has to read
//! the request envelope back, because the only thing distinguishing one market
//! poll from another is a field inside the encrypted query.

use anyhow::{Result, bail};
use edm_core::wire::{self, Nonce};

/// Wraps `block` in the eight-byte frame `strip_frame` looks for.
///
/// Bytes 4..8 hold a length in the real format and are never inspected by the
/// TypeScript **[R60]**, so the mock writes the decompressed size there: a
/// reader that starts trusting those bytes will find them correct rather than
/// finding zeroes and silently working.
fn frame(block: &[u8], decompressed: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(block.len() + 8);
    out.extend_from_slice(b"EDDE");
    out.extend_from_slice(&(decompressed as u32).to_le_bytes());
    out.extend_from_slice(block);
    out
}

/// An LZ4 block of nothing but literals.
///
/// Valid, trivially correct, and — being match-free — it exercises none of the
/// decompressor's interesting paths, which is what a *transport* fixture wants.
/// The paths that matter (`offset === 0`, extended lengths, overlapping copies)
/// are pinned by `tests/wire.rs` against real vectors; a scenario body only has
/// to arrive intact.
pub(crate) fn lz4_literals(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 255 + 2);
    let length = data.len();
    let token = if length >= 15 { 0xF0 } else { (length as u8) << 4 };
    out.push(token);
    if length >= 15 {
        // The extension is a run of 0xFF bytes plus a terminator, and the
        // terminator is added to the total even when it is 0xFF-adjacent.
        let mut remaining = length - 15;
        while remaining >= 255 {
            out.push(0xFF);
            remaining -= 255;
        }
        out.push(remaining as u8);
    }
    out.extend_from_slice(data);
    out
}

/// How a scripted body is turned into bytes on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// Sent exactly as written.
    Raw,
    /// The full game-internal API pipeline: LZ4, `EDDE` frame, ChaCha20, base64.
    GameApi,
    /// Same, minus the frame — the shape `decodeOpaqueBody`'s unframed path
    /// accepts and `decryptResponse` rejects with `MissingFrame` **[R59]**.
    GameApiUnframed,
    /// ChaCha20 and base64 over a body that is *not* an LZ4 block behind a
    /// frame, so the 2xx path fails inside the decompressor.
    GameApiCorruptBlock,
}

impl Encoding {
    pub(crate) fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "raw" => Self::Raw,
            "game_api" => Self::GameApi,
            "game-api-unframed" => Self::GameApiUnframed,
            "game-api-corrupt-block" => Self::GameApiCorruptBlock,
            other => bail!(
                "unknown encoding `{other}` \
                 (raw, game_api, game-api-unframed, game-api-corrupt-block)"
            ),
        })
    }
}

/// The bytes to send, and the `uncompressedsize` the client will need.
#[derive(Clone, Debug)]
pub(crate) struct Sealed {
    pub(crate) bytes: Vec<u8>,
    /// `None` when the encoding produces no compressed frame.
    pub(crate) uncompressed: Option<usize>,
}

pub(crate) fn seal(plaintext: &str, encoding: Encoding, nonce: &str) -> Result<Sealed> {
    if encoding == Encoding::Raw {
        return Ok(Sealed { bytes: plaintext.as_bytes().to_vec(), uncompressed: None });
    }
    let Some(nonce) = Nonce::from_response_header(nonce) else {
        bail!("`nonce` must be twelve hexadecimal characters to seal a body, got {nonce:?}");
    };
    let raw = plaintext.as_bytes();
    let inner = match encoding {
        Encoding::Raw => unreachable!("handled above"),
        Encoding::GameApi => frame(&lz4_literals(raw), raw.len()),
        Encoding::GameApiUnframed => raw.to_vec(),
        // A frame whose block claims a fifteen-byte literal run that is not
        // there: `Invalid LZ4 literal length`.
        Encoding::GameApiCorruptBlock => frame(&[0xF0, 0x00], raw.len()),
    };
    Ok(Sealed {
        bytes: wire::seal_query(&inner, &nonce).into_bytes(),
        uncompressed: matches!(encoding, Encoding::GameApi | Encoding::GameApiCorruptBlock)
            .then_some(raw.len()),
    })
}

/// Recovers the `k=v&k=v` request envelope from the query string.
///
/// This is [`wire::open_opaque`]'s unframed path used on purpose: an envelope
/// is not framed, so the function decodes the whole ChaCha20 output and hands
/// it back as text. **[R59]** is what makes the mock able to route on a market
/// id that is otherwise encrypted.
pub(crate) fn open_envelope(query: &str, nonce_header: &str) -> Option<String> {
    wire::open_opaque(query, nonce_header, None)
}

/// A gzip stream with stored (uncompressed) deflate blocks.
///
/// The point of the fixture is `Content-Encoding: gzip` handling, not the
/// compressor: a stored block is the smallest thing both Bun's fetch and
/// reqwest's `gzip` feature must transparently unwrap.
pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        for (index, chunk) in data.chunks(0xFFFF).enumerate() {
            let final_block = u8::from((index + 1) * 0xFFFF >= data.len());
            out.push(final_block);
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789ab";

    #[test]
    fn a_sealed_game_api_body_opens_with_the_real_decoder() {
        let payload = r#"{"lastStarport":{"commodities":[]}}"#;
        let sealed = seal(payload, Encoding::GameApi, NONCE).unwrap();
        let text = String::from_utf8(sealed.bytes).unwrap();
        let nonce = Nonce::from_response_header(NONCE).unwrap();
        let opened = wire::open_response(&text, &nonce, sealed.uncompressed.unwrap()).unwrap();
        assert_eq!(opened, payload);
    }

    #[test]
    fn long_literal_runs_use_the_extended_length() {
        let payload = "x".repeat(1000);
        let sealed = seal(&payload, Encoding::GameApi, NONCE).unwrap();
        let text = String::from_utf8(sealed.bytes).unwrap();
        let nonce = Nonce::from_response_header(NONCE).unwrap();
        assert_eq!(wire::open_response(&text, &nonce, 1000).unwrap(), payload);
    }

    #[test]
    fn an_unframed_body_is_refused_by_the_2xx_path_and_read_by_the_opaque_one() {
        let sealed = seal("plain text", Encoding::GameApiUnframed, NONCE).unwrap();
        let text = String::from_utf8(sealed.bytes).unwrap();
        let nonce = Nonce::from_response_header(NONCE).unwrap();
        assert_eq!(
            wire::open_response(&text, &nonce, 10).unwrap_err(),
            wire::DecodeError::MissingFrame
        );
        assert_eq!(wire::open_opaque(&text, NONCE, None).as_deref(), Some("plain text"));
    }

    #[test]
    fn an_envelope_round_trips_through_the_router() {
        let plaintext = "marketId=3229009408&cmdrId=1";
        let nonce = Nonce::from_response_header(NONCE).unwrap();
        let query = wire::seal_query(plaintext.as_bytes(), &nonce);
        assert_eq!(open_envelope(&query, NONCE).as_deref(), Some(plaintext));
    }

    #[test]
    fn gzip_has_the_header_trailer_and_checksum_a_client_will_check() {
        let out = gzip(b"hello");
        assert_eq!(&out[..3], &[0x1f, 0x8b, 0x08]);
        assert_eq!(&out[out.len() - 4..], &5u32.to_le_bytes());
        // The reference CRC-32 of "hello".
        assert_eq!(crc32(b"hello"), 0x3610_a686);
    }
}
