//! Per-proxy traffic usage collected from the core, kept on disk for good, and beside it the
//! decision records of the core's probe strategy and the connection stats.
//!
//! The core counts traffic per proxy in UTC hours and keeps 31 days of them; the store merges
//! what it reports idempotently, keeps 32 days of hours and folds older days into day totals.

mod conns;
mod conns_query;
mod files;
mod model;
mod probes;
mod query;
mod store;

pub use conns::{
    CONN_BATCH_HOURS, CONN_RETENTION_DAYS, ConnInstance, ConnKey, ConnState, ConnStore, CoreConnBucket, CoreConnEntry,
    CoreConns, Counting, RecordingSpan, recording_on_disk,
};
pub use conns_query::{
    CONN_QUERY_HOURS, Collected, ConnDimension, ConnFilter, ConnGroup, ConnHour, ConnHourCore, ConnQuery, ConnReport,
    Coverage, Recorded,
};
pub use model::{Identity, InstanceState, Meta, Route, RouteKind, State, UsageGap, UsageGapReason};
pub use probes::{
    DayIndex, HISTORY_LIMIT, HistoryEvent, HistoryGap, HistoryPage, HistoryQuery, InstanceContexts, InstanceCoverage,
    MissingContexts, PROBE_FORMAT, PROBE_PAGE_SIZE, PROBE_PAGES, PROBE_RETENTION_DAYS, ProbeInstance, ProbeMeta,
    ProbePlan, ProbeStore, SeqGap, SeqGapReason, SeqRange,
};
pub use query::{UsageGranularity, UsageItem, UsagePoint, UsageQuery, UsageRange, UsageReport, UsageView};
pub use store::{BATCH_HOURS, CoreBucket, CoreEntry, CoreUsage, InitParams, Store};

#[cfg(test)]
mod conns_tests;
#[cfg(test)]
mod probes_tests;
#[cfg(test)]
mod tests;
