use hnsw_rs::prelude::*;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Default shard size for HNSW index. Shards use sequential insert to
/// guarantee fully connected graphs. 5K maintains ≥97% recall at all
/// tested scales (1K-100K). Larger shards reduce merge overhead but may
/// lower recall. Configurable via `VectorIndex::build_with_shard_size()`.
pub const DEFAULT_SHARD_SIZE: usize = 5000;

/// Internal storage: single graph for small datasets, sharded for large.
enum IndexInner {
    Single {
        hnsw: Hnsw<'static, f32, DistCosine>,
        capacity: usize,
    },
    Sharded {
        shards: Vec<Hnsw<'static, f32, DistCosine>>,
        shard_offsets: Vec<usize>,
        shard_sizes: Vec<usize>,
        shard_capacities: Vec<usize>,
        shard_size_limit: usize,
        ef_construction: usize,
        max_nb_connection: usize,
        max_layer: usize,
    },
}

// SAFETY: Hnsw from hnsw_rs uses internal Arc<RwLock<...>> for graph layers.
// IndexInner only holds Vec<Hnsw> and primitive fields.
// Verified against hnsw_rs =0.3.4 source — no thread-local state or raw pointers
// that would violate Send or Sync. Re-verify on hnsw_rs version upgrades.
// hnsw_rs is pinned to =0.3.4 in Cargo.toml to prevent silent breakage.
unsafe impl Send for IndexInner {}
unsafe impl Sync for IndexInner {}

/// HNSW approximate nearest neighbor index.
///
/// Build once from a collection of embedding vectors, then search many times.
/// For datasets larger than `shard_size` vectors, the index is automatically
/// sharded into smaller HNSW graphs that are built and searched in parallel.
pub struct VectorIndex {
    inner: IndexInner,
    count: usize,
}

/// Compute ef_search for a given top_k and index size.
///
/// Can be overridden via `IRONMEM_EF_SEARCH` env var for tuning/benchmarking.
pub fn compute_ef_search(top_k: usize, count: usize) -> usize {
    if let Ok(val) = std::env::var("IRONMEM_EF_SEARCH") {
        if let Ok(n) = val.parse::<usize>() {
            return n.max(top_k);
        }
    }
    if top_k >= count {
        count * 2
    } else if top_k >= count / 2 {
        count
    } else {
        // 2× overshoot gives ~99% recall at all tested scales; min 100 for small indexes.
        top_k.saturating_mul(2).max(100)
    }
}

/// A scored result for min-heap merging.
struct ScoredId {
    id: usize,
    score: f32,
}

impl PartialEq for ScoredId {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score && self.id == other.id
    }
}

impl Eq for ScoredId {}

impl PartialOrd for ScoredId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredId {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse: smallest score is "greatest" so it gets popped first from the min-heap
        other
            .score
            .partial_cmp(&self.score)
            .unwrap_or(Ordering::Equal)
    }
}

/// Merge top-k results from multiple shards using a bounded min-heap.
pub fn merge_top_k(shard_results: Vec<Vec<(usize, f32)>>, top_k: usize) -> Vec<(usize, f32)> {
    let mut heap: BinaryHeap<ScoredId> = BinaryHeap::with_capacity(top_k + 1);

    for results in shard_results {
        for (id, score) in results {
            if heap.len() < top_k {
                heap.push(ScoredId { id, score });
            } else if let Some(min) = heap.peek() {
                if score > min.score {
                    heap.pop();
                    heap.push(ScoredId { id, score });
                }
            }
        }
    }

    let mut result: Vec<(usize, f32)> = heap.into_iter().map(|s| (s.id, s.score)).collect();
    result.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    result
}

impl VectorIndex {
    /// Build index from a slice of embedding vectors using the default shard size.
    pub fn build(vectors: &[Vec<f32>], ef_construction: usize) -> Self {
        Self::build_with_shard_size(vectors, ef_construction, DEFAULT_SHARD_SIZE)
    }

    /// Build index with a custom shard size.
    ///
    /// Smaller shards improve recall but increase search latency (more shards
    /// to search + merge). Larger shards are faster but may lower recall if
    /// the graph becomes poorly connected.
    ///
    /// Recommended values: 2500-10000. Default: 5000.
    pub fn build_with_shard_size(
        vectors: &[Vec<f32>],
        ef_construction: usize,
        shard_size: usize,
    ) -> Self {
        let n = vectors.len();
        let max_nb_connection = 16;
        let max_layer = 16;
        let shard_size = if shard_size == 0 {
            DEFAULT_SHARD_SIZE
        } else {
            shard_size
        };

        if n <= shard_size {
            // Small dataset: single index, sequential insert
            // Headroom of 1000 (floor 100) lets insert_one add ~1000 entries before
            // the Single index needs a full rebuild. Larger than the shard headroom
            // (500) because single indexes have no overflow path — rebuilds are more
            // expensive so we defer them longer.
            let capacity = n.max(100) + 1000;
            let hnsw = Hnsw::new(
                max_nb_connection,
                capacity,
                max_layer,
                ef_construction,
                DistCosine,
            );
            for (i, v) in vectors.iter().enumerate() {
                hnsw.insert((v.as_slice(), i));
            }
            VectorIndex {
                inner: IndexInner::Single { hnsw, capacity },
                count: n,
            }
        } else {
            // Large dataset: shard and build each in parallel with sequential insert
            let chunks: Vec<&[Vec<f32>]> = vectors.chunks(shard_size).collect();
            let shard_sizes: Vec<usize> = chunks.iter().map(|c| c.len()).collect();
            // Headroom of 500 (floor 100) per shard: smaller than the Single headroom
            // because sharded inserts overflow into a new shard rather than triggering
            // a full rebuild, so the per-shard cost of overflow is low.
            let shard_capacities: Vec<usize> =
                shard_sizes.iter().map(|&sz| sz.max(100) + 500).collect();
            let shard_offsets: Vec<usize> = shard_sizes
                .iter()
                .scan(0usize, |acc, &size| {
                    let offset = *acc;
                    *acc += size;
                    Some(offset)
                })
                .collect();

            let shards: Vec<Hnsw<'static, f32, DistCosine>> = chunks
                .par_iter()
                .enumerate()
                .map(|(s, chunk)| {
                    let hnsw = Hnsw::new(
                        max_nb_connection,
                        shard_capacities[s],
                        max_layer,
                        ef_construction,
                        DistCosine,
                    );
                    for (local_id, v) in chunk.iter().enumerate() {
                        hnsw.insert((v.as_slice(), local_id));
                    }
                    hnsw
                })
                .collect();

            VectorIndex {
                inner: IndexInner::Sharded {
                    shards,
                    shard_offsets,
                    shard_sizes,
                    shard_capacities,
                    shard_size_limit: shard_size,
                    ef_construction,
                    max_nb_connection,
                    max_layer,
                },
                count: n,
            }
        }
    }

    /// Insert a single vector into the live index. Returns the global index
    /// position assigned to this vector (equal to the previous `len()`).
    ///
    /// Returns `usize::MAX` if the Single index is at capacity — caller must
    /// fall back to a full rebuild from DB.
    ///
    /// For Sharded indexes this never fails: a new shard is opened if needed.
    pub fn insert_one(&mut self, vec: &[f32]) -> usize {
        let pos = self.count;
        match &mut self.inner {
            IndexInner::Single { hnsw, capacity } => {
                if self.count >= *capacity {
                    return usize::MAX;
                }
                hnsw.insert((vec, pos));
            }
            IndexInner::Sharded {
                shards,
                shard_offsets,
                shard_sizes,
                shard_capacities,
                shard_size_limit,
                ef_construction,
                max_nb_connection,
                max_layer,
            } => {
                let last = shards.len() - 1;
                if shard_sizes[last] < shard_capacities[last]
                    && shard_sizes[last] < *shard_size_limit
                {
                    let local_id = shard_sizes[last];
                    shards[last].insert((vec, local_id));
                    shard_sizes[last] += 1;
                } else {
                    // Last shard is full — open a new overflow shard with the same
                    // 500-entry headroom used at build time.
                    let new_cap = (*shard_size_limit).max(100) + 500;
                    let hnsw = Hnsw::new(
                        *max_nb_connection,
                        new_cap,
                        *max_layer,
                        *ef_construction,
                        DistCosine,
                    );
                    hnsw.insert((vec, 0usize));
                    shard_offsets.push(pos);
                    shard_sizes.push(1);
                    shard_capacities.push(new_cap);
                    shards.push(hnsw);
                }
            }
        }
        self.count += 1;
        pos
    }

    /// Search for the top_k nearest neighbors to the query vector.
    ///
    /// Returns a list of (original_index, similarity_score) tuples,
    /// sorted by similarity (highest first).
    pub fn search(&self, query: &[f32], top_k: usize) -> Vec<(usize, f32)> {
        if self.count == 0 {
            return vec![];
        }

        match &self.inner {
            IndexInner::Single { hnsw, .. } => {
                let ef_search = compute_ef_search(top_k, self.count);
                let neighbours = hnsw.search(query, top_k, ef_search);
                neighbours
                    .iter()
                    .map(|n| {
                        let similarity = 1.0 - n.distance;
                        (n.d_id, similarity)
                    })
                    .collect()
            }
            IndexInner::Sharded {
                shards,
                shard_offsets,
                shard_sizes,
                ..
            } => {
                let per_shard_results: Vec<Vec<(usize, f32)>> = shards
                    .par_iter()
                    .enumerate()
                    .map(|(s, hnsw)| {
                        let ef_search = compute_ef_search(top_k, shard_sizes[s]);
                        let neighbours = hnsw.search(query, top_k, ef_search);
                        neighbours
                            .iter()
                            .map(|n| {
                                let global_id = shard_offsets[s] + n.d_id;
                                let similarity = 1.0 - n.distance;
                                (global_id, similarity)
                            })
                            .collect()
                    })
                    .collect();

                merge_top_k(per_shard_results, top_k)
            }
        }
    }

    /// Returns the number of vectors in the index.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the index contains no vectors.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vec(dim: usize, seed: u64) -> Vec<f32> {
        let mut v = Vec::with_capacity(dim);
        let mut s = seed.wrapping_add(1);
        for _ in 0..dim {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            v.push((s >> 33) as f32 / (1u64 << 31) as f32 - 1.0);
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    #[test]
    fn insert_one_empty_index() {
        let mut idx = VectorIndex::build(&[], 100);
        assert_eq!(idx.len(), 0);

        let v = make_vec(64, 1);
        let pos = idx.insert_one(&v);
        assert_eq!(pos, 0);
        assert_eq!(idx.len(), 1);

        let results = idx.search(&v, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn insert_one_extends_single_index() {
        let dim = 64;
        let vecs: Vec<Vec<f32>> = (0u64..10).map(|i| make_vec(dim, i)).collect();
        let mut idx = VectorIndex::build(&vecs, 100);
        assert_eq!(idx.len(), 10);

        let new_vec = make_vec(dim, 99);
        let pos = idx.insert_one(&new_vec);
        assert_eq!(pos, 10);
        assert_eq!(idx.len(), 11);

        let results = idx.search(&new_vec, 1);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 10);
    }

    #[test]
    fn insert_one_returns_sentinel_when_at_capacity() {
        let dim = 16;
        let mut idx = VectorIndex::build(&[], 100);

        for i in 0u64..1100 {
            let v = make_vec(dim, i);
            let pos = idx.insert_one(&v);
            assert_eq!(pos, i as usize);
        }
        assert_eq!(idx.len(), 1100);

        let v = make_vec(dim, 9999);
        assert_eq!(idx.insert_one(&v), usize::MAX);
        assert_eq!(idx.len(), 1100);
    }

    #[test]
    fn insert_one_sharded_appends_to_last_shard() {
        let dim = 64;
        let shard_size = 50;
        let vecs: Vec<Vec<f32>> = (0..120).map(|i| make_vec(dim, i as u64)).collect();
        let mut idx = VectorIndex::build_with_shard_size(&vecs, 100, shard_size);
        assert_eq!(idx.len(), 120);

        let new_vec = make_vec(dim, 999);
        let pos = idx.insert_one(&new_vec);
        assert_eq!(pos, 120);
        assert_eq!(idx.len(), 121);

        let results = idx.search(&new_vec, 1);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 120);
    }
}
