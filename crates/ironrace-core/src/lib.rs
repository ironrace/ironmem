//! Shared vector-search primitives for the IronRace memory workspace.

mod vector;

/// Compute a search-time `ef_search` value for a requested `top_k`.
pub use vector::compute_ef_search;
/// Merge top-k results returned from multiple HNSW shards.
pub use vector::merge_top_k;
/// Approximate nearest-neighbor index used by the memory server.
pub use vector::VectorIndex;
/// Default shard size used when building a sharded HNSW index.
pub use vector::DEFAULT_SHARD_SIZE;
