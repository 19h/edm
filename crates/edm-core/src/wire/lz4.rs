//! A raw LZ4 block decompressor transcribed from `market-request.ts:271-318`.
//!
//! There is a perfectly good LZ4 crate and it is not used here. **[R61]** The
//! TypeScript's decoder is not the reference decoder: it rejects a zero match
//! offset, it validates against the *running* output position rather than the
//! declared size, and its six error strings are printed to the user verbatim
//! when a body fails to decrypt. `lz4_flex` disagrees on all three counts —
//! most sharply on `offset == 0`, which it guards with a comparison that can
//! never be true, so it accepts the corrupt block and emits whatever was left
//! in its output buffer where the TypeScript raises an error. Reproducing the
//! error messages while borrowing the acceptance set of a different decoder
//! would be worse than either.
//!
//! `tests/wire.rs` pins the accept and reject sets, and a property test asserts
//! that everything `lz4_flex` *compresses* still decompresses here — the
//! divergence is confined to malformed input.

use std::fmt;

/// A block that `decompressLz4Block` refused.
///
/// Every message is the TypeScript's, byte for byte, because it reaches the
/// terminal through `Could not decrypt response: {message}` (ts:1285).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lz4Error {
    /// An extended length ran off the end of the block (ts:277).
    TruncatedLength,
    /// A literal run overran the input or the output (ts:294).
    InvalidLiteralLength,
    /// Fewer than two bytes remained for the match offset (ts:301).
    TruncatedMatchOffset,
    /// The match offset was zero, or pointed before the start of the output
    /// (ts:305).
    InvalidMatchOffset,
    /// The match would have overrun the output (ts:308).
    InvalidMatchLength,
    /// The block decoded cleanly but not to the declared size (ts:315-317).
    SizeMismatch {
        /// The `uncompressedsize` response header.
        expected: usize,
        /// What the block actually produced.
        produced: usize,
    },
    /// The declared size is larger than this program is willing to allocate.
    ///
    /// **C4**: `uncompressedsize` is chosen by the server, and `new
    /// Uint8Array(9e15)` throws a catchable `RangeError` in JavaScript where
    /// `vec![0; 9e15]` aborts the process. This is that `RangeError`, with our
    /// own wording; the cap is [`MAX_DECOMPRESSED`].
    Allocation {
        /// The size that was refused.
        requested: usize,
    },
}

impl fmt::Display for Lz4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TruncatedLength => f.write_str("Truncated LZ4 length"),
            Self::InvalidLiteralLength => f.write_str("Invalid LZ4 literal length"),
            Self::TruncatedMatchOffset => f.write_str("Truncated LZ4 match offset"),
            Self::InvalidMatchOffset => f.write_str("Invalid LZ4 match offset"),
            Self::InvalidMatchLength => f.write_str("Invalid LZ4 match length"),
            Self::SizeMismatch { expected, produced } => {
                // Both interpolands are non-negative integers below the C4 cap,
                // so `usize` renders them exactly as `Number::toString` would.
                write!(f, "LZ4 size mismatch: expected {expected}, produced {produced}")
            }
            Self::Allocation { requested } => {
                write!(f, "Cannot allocate {requested} bytes for the decompressed response")
            }
        }
    }
}

impl std::error::Error for Lz4Error {}

/// The largest output this decoder will allocate, 256 MiB. **C4**
///
/// A CAPI market snapshot is tens of kilobytes; a shipyard is smaller still.
pub const MAX_DECOMPRESSED: usize = 256 << 20;

/// `decompressLz4Block` (ts:271).
///
/// `expected` comes from the `uncompressedsize` response header and is both the
/// allocation size and the acceptance criterion: a block that decodes to any
/// other length is an error, not a short read.
pub(super) fn decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, Lz4Error> {
    // `new Uint8Array(expectedSize)` is zero-filled and its length is fixed
    // before a byte is read, which is what makes every bound check below a
    // comparison against `expected`.
    if expected > MAX_DECOMPRESSED {
        return Err(Lz4Error::Allocation { requested: expected });
    }
    let mut output = Vec::new();
    output.try_reserve_exact(expected).map_err(|_| Lz4Error::Allocation { requested: expected })?;
    output.resize(expected, 0);

    decompress_into(input, &mut output)?;
    Ok(output)
}

/// The loop itself, over a caller-supplied output buffer.
///
/// Split out so that a test can poison the buffer and prove that success
/// implies every byte was written — the property the caller relies on when it
/// hands the result to a UTF-8 decoder.
fn decompress_into(input: &[u8], output: &mut [u8]) -> Result<(), Lz4Error> {
    let mut source = 0usize;
    let mut destination = 0usize;

    while source < input.len() {
        let token = input[source];
        source += 1;

        let literal_length = read_extended_length(input, &mut source, u64::from(token >> 4))?;
        // Both halves of this test are needed and neither is redundant: the
        // first stops a run that overruns the block, the second one that
        // overruns the declared size. Saturating because `literal_length` is
        // itself saturating; the sums are compared against lengths far below
        // the saturation point, so the answers are the exact ones.
        if (source as u64).saturating_add(literal_length) > input.len() as u64
            || (destination as u64).saturating_add(literal_length) > output.len() as u64
        {
            return Err(Lz4Error::InvalidLiteralLength);
        }
        let literal_length = literal_length as usize;
        output[destination..destination + literal_length]
            .copy_from_slice(&input[source..source + literal_length]);
        source += literal_length;
        destination += literal_length;

        // A block legally ends on a literal run, and the TypeScript does not
        // require the reference format's five trailing literals.
        if source == input.len() {
            break;
        }
        if source + 2 > input.len() {
            return Err(Lz4Error::TruncatedMatchOffset);
        }
        let offset = usize::from(input[source]) | (usize::from(input[source + 1]) << 8);
        source += 2;
        // `destination` is the *running* output position, not `output.len()`:
        // a match may only reach back into bytes already produced. Zero is
        // rejected outright, which is where `lz4_flex` differs.
        if offset == 0 || offset > destination {
            return Err(Lz4Error::InvalidMatchOffset);
        }

        // The `+ 4` is applied before the bound check, so a match length of
        // exactly the remaining space passes and one byte more does not.
        let match_length =
            read_extended_length(input, &mut source, u64::from(token & 0x0f))?.saturating_add(4);
        if (destination as u64).saturating_add(match_length) > output.len() as u64 {
            return Err(Lz4Error::InvalidMatchLength);
        }
        let match_length = match_length as usize;

        if offset < match_length {
            // Overlapping: the match reads bytes this very loop is writing, so
            // the copy has to advance one byte at a time. This is how LZ4
            // encodes runs, and `copy_within` would produce different output.
            for _ in 0..match_length {
                output[destination] = output[destination - offset];
                destination += 1;
            }
        } else {
            let from = destination - offset;
            output.copy_within(from..from + match_length, destination);
            destination += match_length;
        }
    }

    if destination != output.len() {
        return Err(Lz4Error::SizeMismatch { expected: output.len(), produced: destination });
    }
    Ok(())
}

/// The LZ4 length extension (ts:275-286).
///
/// Extends only when the nibble is exactly 15, and the run terminates on the
/// first byte that is not 255 — that terminating byte is *added*, so an
/// extension of `[255, 0]` means 15 + 255 + 0.
///
/// Accumulated in `u64` and saturating: a length is bounded by 255 per input
/// byte, so it cannot approach the saturation point for any block small enough
/// to exist, and saturating keeps the comparisons that follow honest on the
/// pathological input that could.
fn read_extended_length(input: &[u8], source: &mut usize, initial: u64) -> Result<u64, Lz4Error> {
    let mut length = initial;
    if initial == 15 {
        loop {
            if *source >= input.len() {
                return Err(Lz4Error::TruncatedLength);
            }
            let extension = input[*source];
            *source += 1;
            length = length.saturating_add(u64::from(extension));
            if extension != 255 {
                break;
            }
        }
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{Lz4Error, MAX_DECOMPRESSED, decompress, decompress_into};

    #[test]
    fn refuses_an_oversized_allocation_instead_of_aborting() {
        // C4: the TypeScript throws a catchable RangeError here. Rust would
        // abort the process, so this is a registered divergence with its own
        // message rather than one of the six transcribed strings.
        let err = decompress(&[], MAX_DECOMPRESSED + 1).expect_err("over the cap");
        assert_eq!(err, Lz4Error::Allocation { requested: MAX_DECOMPRESSED + 1 });
        assert_eq!(
            err.to_string(),
            "Cannot allocate 268435457 bytes for the decompressed response"
        );
    }

    proptest! {
        /// The divergence from a conventional decoder must be confined to
        /// malformed blocks: whatever a real compressor emits, we accept and
        /// reproduce.
        #[test]
        fn accepts_everything_lz4_flex_compresses(
            data in prop::collection::vec(any::<u8>(), 0..4096)
        ) {
            let block = lz4_flex::compress(&data);
            let round_tripped = decompress(&block, data.len());
            prop_assert_eq!(round_tripped.as_deref(), Ok(data.as_slice()));
        }

        /// Long runs and repeats are where the overlapping-match path lives; a
        /// uniform random buffer almost never reaches it.
        #[test]
        fn accepts_compressible_input(
            unit in prop::collection::vec(any::<u8>(), 1..8),
            repeats in 1usize..400,
        ) {
            let data: Vec<u8> = unit.iter().copied().cycle().take(unit.len() * repeats).collect();
            let block = lz4_flex::compress(&data);
            let round_tripped = decompress(&block, data.len());
            prop_assert_eq!(round_tripped.as_deref(), Ok(data.as_slice()));
        }

        /// Success must mean the whole buffer was written. The caller hands the
        /// output straight to a UTF-8 decoder, so a hole left over from the
        /// zero-fill would decode as embedded NULs rather than fail.
        #[test]
        fn success_writes_every_byte(
            data in prop::collection::vec(0u8..=0xa9, 0..2048)
        ) {
            let block = lz4_flex::compress(&data);
            let mut output = vec![0xaa_u8; data.len()];
            prop_assert_eq!(decompress_into(&block, &mut output), Ok(()));
            prop_assert!(!output.contains(&0xaa), "a byte of the poison survived");
            prop_assert_eq!(output, data);
        }
    }
}
