//! SQLite-backed storage layers for drawers, schema management, and WAL auditing.

pub mod collab;
pub mod drawers;
pub mod knowledge_graph;
pub mod schema;
pub mod wal;

/// Search result types returned from drawer queries.
pub use drawers::{Drawer, ScoredDrawer, SearchFilters};
