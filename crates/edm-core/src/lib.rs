//! Pure core of `edm` — no I/O, no clock, no entropy, no network.
//!
//! This crate is where the port's correctness lives. It is kept free of
//! `tokio`, `reqwest`, `rustix` and `getrandom` so that every behaviour in
//! `PORTING.md` is reachable from a plain `#[test]`, and so that the whole
//! crate can be driven under `cargo miri` and `cargo fuzz` at full speed.
//! `cargo xtask gates` fails the build if an impure dependency ever appears in
//! this crate's dependency tree.
//!
//! The organising idea: `market-request.ts` is the specification, and its
//! observable behaviour is inherited from JavaScript semantics that Rust does
//! not share — `f64`-only arithmetic, UTF-16 string indexing, ECMAScript object
//! key enumeration order, and `Number::toString`. Rather than dust those
//! conversions across the codebase, they are concentrated in [`js`], which
//! everything else is written in terms of.

#![forbid(unsafe_code)]

pub mod cli;
pub mod js;
pub mod render;
pub mod wire;
