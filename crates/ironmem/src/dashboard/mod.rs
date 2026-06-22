//! Local read-only dashboard server for `ironmem`.
//!
//! Exposes four inspection surfaces over the configured SQLite store:
//! memory drawers/taxonomy, code maps, collab sessions, and report metrics.
//! The dashboard never mutates state: it opens the DB read-only, only serves
//! `GET`/`HEAD`, and has no write paths.
//!
//! See `ironmem dashboard --help` for the CLI surface.
pub(crate) mod data;
pub(crate) mod routes;
mod server;

pub use server::{run_dashboard, DashboardConfig};
