//! The Frontier transport codec: ChaCha20, base64 and raw LZ4 blocks.
//!
//! A request envelope is `k=v&k=v` plaintext, ChaCha20-sealed under a
//! compile-time key with the twelve ASCII characters of the nonce used *as* the
//! IETF nonce, then standard-base64ed and appended raw as the query string.
//! A response runs the other way and adds two layers: an eight-byte `EDDE`
//! frame and an LZ4 block whose decompressed size arrives in a header.
