//! `edm route` — the acquisition and orchestration half.
//!
//! The optimiser is a separate, pure crate (`edm-route`). What lives here is
//! everything that touches the outside world: enumerating the region through
//! Ardent, pricing the plan, gating the spend, and running the paced two-stage
//! sweep that fills the market table the optimiser is handed.

pub mod pacer;
pub mod plan;
pub mod pool;
