//! `edm route` — the acquisition and orchestration half.
//!
//! A new command, not a port of one \[C25\]: the TypeScript answers
//! `Unknown command "route"` and exits 2, so nothing under here has an oracle
//! and nothing under here may change what the four ported commands do. The
//! parse that reaches it is the extended one, and `cargo xtask gates` proves
//! the two tables agree on every committed scenario's argv \[C26\].
//!
//! The split is the same as everywhere else in this workspace: arithmetic and
//! grammar live in `edm-core` (and, for the optimiser, in the pure `edm-route`
//! crate); this module is the part that needs a socket, a clock and a screen —
//! enumerating the region through Ardent, pricing the plan, gating the spend,
//! and running the paced two-stage sweep that fills the market table the
//! optimiser is handed.

pub mod acquire;
pub mod cache;
pub mod discover;
pub mod ingest;
pub mod pacer;
pub mod plan;
pub mod pool;
