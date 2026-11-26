//! Lock-free metrics collection and the live terminal dashboard.

pub mod stats;

pub use stats::{CompletionGuard, Dashboard, Progress, Stats, StatsSnapshot};
