//! SQLite-backed storage layers for drawers, metrics, schema management, and
//! WAL auditing.

pub mod code_maps;
pub mod collab;
pub mod drawers;
pub mod knowledge_graph;
pub mod metrics;
pub mod read_only;
pub mod schema;
pub mod symbol_graph;
pub mod wal;

/// Search result types returned from drawer queries.
pub use code_maps::CodeMap;
pub use drawers::{Drawer, ScoredDrawer, SearchFilters};
pub use metrics::{
    ExplorationReport, MapStatus, NewOccupancySample, NewTokenUsage, OccupancySample,
    SessionSummary, TaskOutcome, TokenUsage, TokenUsageQuery,
};
pub use read_only::ReadOnlyDb;
