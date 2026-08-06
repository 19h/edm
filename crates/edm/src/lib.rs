//! The impure half of `edm`: transport, terminal, clock, entropy, filesystem.
//!
//! Everything that can be a pure function already is one, in `edm-core`. What
//! is left here is the I/O and the orchestration that sequences it — and the
//! sequencing is itself observable, because the original interleaves network
//! calls with printing in an order the parity harness diffs.

pub mod ardent;
pub mod game_api;
pub mod cmd;
pub mod commander;
pub mod eddn;
pub mod exchange;
pub mod sweep;
pub mod net;
pub mod out;
pub mod ports;
pub mod route;
pub mod secret;
pub mod sys;
