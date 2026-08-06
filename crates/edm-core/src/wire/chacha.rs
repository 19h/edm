//! `ChaCha20` under the one compile-time key.
//!
//! Frontier uses the IETF variant (RFC 8439): a 32-byte key, a 12-byte nonce
//! and a 32-bit block counter that starts at zero. `game-internal-api.ts` ships its
//! own implementation (ts:105-191); this delegates to the `chacha20` crate,
//! which is the same function.

use cipher::{KeyIvInit, StreamCipher};

use super::KEY;

/// The keystream ran past the end of the 32-bit block counter.
///
/// Rendered as `"ChaCha20 counter exhausted"` (ts:189).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CounterExhausted;

/// XORs `buf` with the keystream in place.
///
/// The `chacha20` crate's IETF counter tops out one block earlier than the
/// TypeScript's, which wraps to zero and only then throws — a 64-byte
/// difference 274 GB into a single message. Registered as **C6**; the error
/// string is unchanged either way.
pub(super) fn apply(buf: &mut [u8], nonce: &[u8; 12]) -> Result<(), CounterExhausted> {
    // C7: the TypeScript's two length checks (ts:106-107) are unreachable
    // because both operands are typed here, so they have no counterpart.
    let mut cipher = chacha20::ChaCha20::new(KEY.into(), nonce.into());
    cipher.try_apply_keystream(buf).map_err(|_| CounterExhausted)
}

#[cfg(test)]
mod tests {
    use cipher::{KeyIvInit, StreamCipher};
    use hex_literal::hex;

    use super::apply;

    /// Runs the RFC's key and nonce instead of Frontier's: `apply` hardcodes
    /// `KEY`, so the vectors are reached through the crate directly. What is
    /// being pinned is that `apply` selects the IETF variant with a zero
    /// starting counter — the two facts `apply` itself contributes.
    fn keystream(key: &[u8; 32], nonce: &[u8; 12], len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let mut cipher = chacha20::ChaCha20::new(key.into(), nonce.into());
        cipher.apply_keystream(&mut buf);
        buf
    }

    /// RFC 8439 §A.1 test vector #1 — the anchor for "the counter starts at 0".
    /// If the crate ever defaulted to 1 this is the test that fails.
    #[test]
    fn counter_starts_at_zero() {
        assert_eq!(
            keystream(&[0u8; 32], &[0u8; 12], 64),
            hex!(
                "76b8e0ada0f13d90405d6ae55386bd28"
                "bdd219b8a08ded1aa836efcc8b770dc7"
                "da41597c5157488d7724e03fb8d84a37"
                "6a43b8f41518a11cc387b669b2ee6586"
            )
        );
    }

    /// §A.1 vectors #1 and #2 are consecutive blocks of one stream, so a
    /// 128-byte request must produce them back to back.
    #[test]
    fn counter_increments_between_blocks() {
        let stream = keystream(&[0u8; 32], &[0u8; 12], 128);
        assert_eq!(
            stream[64..],
            hex!(
                "9f07e7be5551387a98ba977c732d080d"
                "cb0f29a048e3656912c6533e32ee7aed"
                "29b721769ce64e43d57133b074d839d5"
                "31ed1f28510afb45ace10a1f4b794d6f"
            )
        );
    }

    /// RFC 8439 §2.3.2. The nonce here is `00000009 0000004a 00000000`, whose
    /// three little-endian words land in state slots 13, 14 and 15 — swap any
    /// two and this vector dies. The expected block is the RFC's counter-1
    /// block, reached by asking for two blocks and looking at the second.
    #[test]
    fn nonce_word_order() {
        let stream = keystream(
            &hex!("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"),
            &hex!("000000090000004a00000000"),
            128,
        );
        assert_eq!(
            stream[64..],
            hex!(
                "10f1e7e4d13b5915500fdd1fa32071c4"
                "c7d1f4c733c068030422aa9ac3d46c4e"
                "d2826446079faa0914c2d705d98b02a2"
                "b5129cd1de164eb9cbd083e8a2503c4e"
            )
        );
    }

    /// RFC 8439 §2.4.2 — the sunscreen paragraph, encrypted at counter 1. The
    /// leading 64 zero bytes burn block 0 so that the plaintext lines up with
    /// the RFC's ciphertext.
    #[test]
    fn rfc8439_sunscreen_ciphertext() {
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you \
only one tip for the future, sunscreen would be it.";
        let mut buf = vec![0u8; 64];
        buf.extend_from_slice(plaintext);

        let mut cipher = chacha20::ChaCha20::new(
            (&hex!("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")).into(),
            (&hex!("000000000000004a00000000")).into(),
        );
        cipher.apply_keystream(&mut buf);

        assert_eq!(
            buf[64..],
            hex!(
                "6e2e359a2568f98041ba0728dd0d6981"
                "e97e7aec1d4360c20a27afccfd9fae0b"
                "f91b65c5524733ab8f593dabcd62b357"
                "1639d624e65152ab8f530c359f0861d8"
                "07ca0dbf500d6a6156a38e088a22b65e"
                "52bc514d16ccf806818ce91ab7793736"
                "5af90bbf74a35be6b40b8eedf2785e42"
                "874d"
            )
        );
    }

    /// The Frontier key is what `apply` actually uses, and it is a plain ASCII
    /// digit string rather than 32 bytes of entropy (ts:40).
    #[test]
    fn frontier_key_is_ascii_digits() {
        assert_eq!(super::KEY, b"52381239578582178380088936356181");
        assert!(super::KEY.iter().all(u8::is_ascii_digit));
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let mut buf: [u8; 0] = [];
        assert_eq!(apply(&mut buf, b"0123456789ab"), Ok(()));
    }

    /// XOR is an involution, so sealing twice under one nonce is the identity —
    /// which is how the response path undoes what the request path did.
    #[test]
    fn applying_twice_is_the_identity() {
        let original = b"commodityId=128049204&qty=7".to_vec();
        let mut buf = original.clone();
        apply(&mut buf, b"a1b2c3d4e5f6").expect("well under the counter limit");
        assert_ne!(buf, original);
        apply(&mut buf, b"a1b2c3d4e5f6").expect("well under the counter limit");
        assert_eq!(buf, original);
    }
}
