//! Per-proxy traffic usage collected from the core, kept on disk for good.
//!
//! The core counts traffic per proxy in UTC hours and keeps 31 days of them; the store merges
//! what it reports idempotently, keeps 32 days of hours and folds older days into day totals.

mod files;
mod model;
mod query;
mod store;

pub use model::{Identity, InstanceState, Meta, Route, RouteKind, State, UsageGap, UsageGapReason};
pub use query::{UsageGranularity, UsageItem, UsagePoint, UsageQuery, UsageRange, UsageReport, UsageView};
pub use store::{BATCH_HOURS, CoreBucket, CoreEntry, CoreUsage, InitParams, Store};

#[cfg(test)]
mod tests;
