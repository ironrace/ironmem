//! Application state — initialized once, shared across MCP tool handlers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use ironrace_core::VectorIndex;
use ironrace_embed::Embedder;

use crate::config::{Config, EmbedMode};
use crate::db::schema::Database;
use crate::error::MemoryError;
use crate::search::graph::MemoryGraph;

/// HNSW index + id_map bundled together to eliminate TOCTOU between separate locks.
pub struct IndexState {
    pub index: VectorIndex,
    /// Maps HNSW index position → drawer_id.
    pub id_map: Vec<String>,
}

/// Top-level application state.
pub struct App {
    pub config: Config,
    pub db: Database,
    pub embedder: RwLock<Embedder>,
    pub(crate) reranker: RwLock<Option<Arc<dyn ironrace_rerank::RerankerScorer>>>,
    pub index_state: RwLock<IndexState>,
    /// Dirty flag: set after writes, cleared after rebuild.
    dirty: AtomicBool,
    /// Cached memory graph (wing/room adjacency). Invalidated on writes.
    pub graph_cache: RwLock<Option<MemoryGraph>>,
    /// Set to true once background memory init (model load + bootstrap) completes.
    /// False during warmup; tools that need the embedder return a warming_up response.
    pub memory_ready: Arc<AtomicBool>,
    /// Guards the one-time HNSW rebuild triggered when memory_ready transitions to true.
    memory_ready_rebuilt: AtomicBool,
}

impl App {
    /// Initialize the application: open DB, load model, rebuild HNSW index.
    pub fn new(config: Config) -> Result<Self, MemoryError> {
        config.ensure_dirs()?;

        let db = Database::open(&config.db_path)?;
        db.migrate()?;

        // Prune old WAL entries to prevent unbounded growth
        if let Err(e) = db.wal_prune(None) {
            tracing::warn!("WAL pruning failed (non-fatal): {e}");
        }

        // Load embedder
        let embedder = match config.embed_mode {
            EmbedMode::Noop => Embedder::new_noop(),
            EmbedMode::Real => {
                let model_dir = ironrace_embed::embedder::ensure_model_in_dir(
                    &config.model_dir,
                    !config.model_dir_explicit,
                )
                .map_err(MemoryError::Embed)?;
                Embedder::new(&model_dir).map_err(MemoryError::Embed)?
            }
        };

        // Load vectors and build HNSW index
        let vectors_with_ids = db.load_all_vectors()?;
        let drawer_count = vectors_with_ids.len();
        let vectors_for_index: Vec<Vec<f32>> =
            vectors_with_ids.iter().map(|(_, v)| v.clone()).collect();
        let id_map: Vec<String> = vectors_with_ids.into_iter().map(|(id, _)| id).collect();

        let index = if vectors_for_index.is_empty() {
            VectorIndex::build(&[], 100)
        } else {
            VectorIndex::build(&vectors_for_index, 100)
        };

        tracing::info!(
            "Memory loaded: {} drawers, HNSW index built, MCP mode: {:?}",
            drawer_count,
            config.mcp_access_mode,
        );

        Ok(Self {
            config,
            db,
            embedder: RwLock::new(embedder),
            reranker: RwLock::new(None),
            index_state: RwLock::new(IndexState { index, id_map }),
            dirty: AtomicBool::new(false),
            graph_cache: RwLock::new(None),
            memory_ready: Arc::new(AtomicBool::new(true)),
            memory_ready_rebuilt: AtomicBool::new(true),
        })
    }

    /// Phase-1 fast init for `serve`: open DB and migrate schema only (~50ms).
    /// The embedder is a noop placeholder; background init replaces it via
    /// `run_background_memory_init` and signals `memory_ready` when done.
    pub fn new_server_ready(config: Config) -> Result<Self, MemoryError> {
        config.ensure_dirs()?;
        let db = Database::open(&config.db_path)?;
        db.migrate()?;
        if let Err(e) = db.wal_prune(None) {
            tracing::warn!("WAL pruning failed (non-fatal): {e}");
        }
        tracing::info!(
            "Server ready (memory warming up in background), MCP mode: {:?}",
            config.mcp_access_mode,
        );
        Ok(Self {
            config,
            db,
            embedder: RwLock::new(Embedder::new_noop()),
            reranker: RwLock::new(None),
            index_state: RwLock::new(IndexState {
                index: VectorIndex::build(&[], 100),
                id_map: Vec::new(),
            }),
            dirty: AtomicBool::new(false),
            graph_cache: RwLock::new(None),
            memory_ready: Arc::new(AtomicBool::new(false)),
            memory_ready_rebuilt: AtomicBool::new(false),
        })
    }

    /// Returns true while background memory init is still in progress.
    /// Embedding-dependent tools should return a warming_up response during this window.
    pub fn is_warming_up(&self) -> bool {
        !self.memory_ready.load(Ordering::Relaxed)
    }

    /// Create an App with an in-memory DB and noop embedder for testing.
    /// No ONNX model required — suitable for unit and integration tests.
    pub fn open_for_test() -> Result<Self, MemoryError> {
        Self::open_for_test_with_mode(crate::config::McpAccessMode::Trusted)
    }

    /// Like `open_for_test` but with a configurable access mode.
    pub fn open_for_test_with_mode(
        mode: crate::config::McpAccessMode,
    ) -> Result<Self, MemoryError> {
        let db = crate::db::schema::Database::open_in_memory()?;
        let state_dir = std::env::temp_dir().join(format!(
            "ironmem-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos(),
        ));
        std::fs::create_dir_all(&state_dir).map_err(MemoryError::Io)?;
        let config = crate::config::Config {
            db_path: std::path::PathBuf::from(":memory:"),
            model_dir: std::path::PathBuf::from("/nonexistent"),
            model_dir_explicit: true,
            state_dir,
            mcp_access_mode: mode,
            embed_mode: crate::config::EmbedMode::Noop,
        };
        let embedder = ironrace_embed::Embedder::new_noop();
        Ok(Self {
            config,
            db,
            embedder: RwLock::new(embedder),
            reranker: RwLock::new(None),
            index_state: RwLock::new(IndexState {
                index: VectorIndex::build(&[], 100),
                id_map: Vec::new(),
            }),
            dirty: AtomicBool::new(false),
            graph_cache: RwLock::new(None),
            memory_ready: Arc::new(AtomicBool::new(true)),
            memory_ready_rebuilt: AtomicBool::new(true),
        })
    }

    /// Mark index as dirty after a write operation. The index will be
    /// rebuilt lazily on the next search via `ensure_index_fresh()`.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
        // Invalidate graph cache
        if let Ok(mut cache) = self.graph_cache.write() {
            *cache = None;
        }
    }

    /// Insert a single embedding into the live HNSW index without a full rebuild.
    /// Falls back to a full rebuild from DB if the index is at capacity.
    pub fn insert_into_index(&self, drawer_id: &str, embedding: &[f32]) -> Result<(), MemoryError> {
        let mut state = self
            .index_state
            .write()
            .map_err(|e| MemoryError::Lock(format!("IndexState lock poisoned: {e}")))?;

        let pos = state.index.insert_one(embedding);
        if pos == usize::MAX {
            drop(state);
            tracing::info!("HNSW index at capacity; falling back to full rebuild");
            self.dirty.store(true, Ordering::Release);
            return self.rebuild_index_from_db();
        }

        // pos == id_map.len() is invariant: insert_one returns self.count before
        // incrementing, and id_map is kept in sync with the index on every insert.
        assert_eq!(
            pos,
            state.id_map.len(),
            "HNSW index position desync: pos={pos} id_map.len()={}",
            state.id_map.len()
        );
        state.id_map.push(drawer_id.to_string());

        if let Ok(mut cache) = self.graph_cache.write() {
            *cache = None;
        }

        Ok(())
    }

    /// If background init just completed, swap in the real embedder.
    /// Must be called before any embed operation (add, diary write, search).
    /// Idempotent: the swap happens at most once per server lifetime.
    pub fn ensure_embedder_ready(&self) -> Result<(), MemoryError> {
        if self.memory_ready.load(Ordering::Acquire)
            && !self.memory_ready_rebuilt.swap(true, Ordering::AcqRel)
        {
            self.reload_embedder()?;
            // Mark dirty so the HNSW index is rebuilt on the next search, picking
            // up all drawers written by the background bootstrap.
            self.dirty.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// Rebuild the HNSW index if dirty. Called before search.
    pub fn ensure_index_fresh(&self) -> Result<(), MemoryError> {
        self.ensure_embedder_ready()?;
        if self.dirty.load(Ordering::Acquire) {
            self.rebuild_index_from_db()?;
        }
        Ok(())
    }

    /// Lazy-construct the production LLM reranker. Called from the pipeline on
    /// the first search where `tunables::rerank_enabled()` is true AND the field
    /// is `None`. Construction itself cannot fail — subprocess errors only
    /// surface on first `score_pairs` call, where the rerank module degrades
    /// gracefully (logs a `WARN`, returns the un-reranked candidates).
    ///
    /// Wired in from the search pipeline (step 9).
    pub(crate) fn ensure_reranker_loaded(&self) {
        {
            let r = self.reranker.read().unwrap();
            if r.is_some() {
                return;
            }
        }
        let mut w = self.reranker.write().unwrap();
        if w.is_some() {
            return; // raced — another thread loaded it
        }
        let model = crate::search::tunables::llm_rerank_model();
        let timeout =
            std::time::Duration::from_millis(crate::search::tunables::llm_rerank_timeout_ms());
        let backend = crate::search::tunables::llm_rerank_backend();

        // Backend selection: "api" → direct Anthropic Messages API (billed,
        // requires a key); anything else → local `claude` CLI via subscription
        // auth (free per call, ~1-3s subprocess startup).
        match backend {
            "api" => {
                let key = crate::search::tunables::anthropic_api_key().unwrap_or_else(|| {
                    panic!(
                        "IRONMEM_LLM_RERANK_BACKEND=api requires ANTHROPIC_API_KEY or \
                         IRONMEM_ANTHROPIC_API_KEY to be set"
                    );
                });
                let max_tokens = crate::search::tunables::llm_rerank_max_tokens();
                let client = ironrace_rerank::AnthropicApiClient::new(key, model.clone(), timeout)
                    .with_max_tokens(max_tokens);
                let reranker = ironrace_rerank::LlmReranker::new(client);
                *w = Some(Arc::new(reranker));
                tracing::info!(model = %model, backend = "api", max_tokens, "LLM reranker loaded");
            }
            _ => {
                let client = ironrace_rerank::ClaudeCliClient::new(model.clone(), timeout);
                let reranker = ironrace_rerank::LlmReranker::new(client);
                *w = Some(Arc::new(reranker));
                tracing::info!(model = %model, backend = "cli", "LLM reranker loaded");
            }
        }
    }

    /// Test-only — production code should use `ensure_reranker_loaded`.
    ///
    /// Constructs a test `App` (in-memory DB, noop embedder) and installs a
    /// pre-built `RerankerScorer` so integration tests can exercise the
    /// rerank path without booting ONNX. Mirrors `open_for_test`.
    pub fn with_reranker(
        scorer: Arc<dyn ironrace_rerank::RerankerScorer>,
    ) -> Result<Self, MemoryError> {
        let app = Self::open_for_test()?;
        *app.reranker.write().unwrap() = Some(scorer);
        Ok(app)
    }

    /// Swap the real embedder into this App. Called once after background init completes.
    fn reload_embedder(&self) -> Result<(), MemoryError> {
        let new_embedder = match self.config.embed_mode {
            EmbedMode::Noop => Embedder::new_noop(),
            EmbedMode::Real => {
                let model_dir = ironrace_embed::embedder::ensure_model_in_dir(
                    &self.config.model_dir,
                    !self.config.model_dir_explicit,
                )
                .map_err(MemoryError::Embed)?;
                Embedder::new(&model_dir).map_err(MemoryError::Embed)?
            }
        };
        let mut emb = self
            .embedder
            .write()
            .map_err(|e| MemoryError::Lock(format!("Embedder lock poisoned: {e}")))?;
        *emb = new_embedder;
        tracing::info!("Embedder reloaded after background init");
        Ok(())
    }

    /// Rebuild the HNSW index from DB. Swaps index + id_map atomically.
    /// Dirty flag is cleared inside the write lock so a concurrent
    /// `mark_dirty()` that fires after our DB read is not lost.
    fn rebuild_index_from_db(&self) -> Result<(), MemoryError> {
        let vectors_with_ids = self.db.load_all_vectors()?;
        let vectors: Vec<Vec<f32>> = vectors_with_ids.iter().map(|(_, v)| v.clone()).collect();
        let id_map: Vec<String> = vectors_with_ids.into_iter().map(|(id, _)| id).collect();

        let new_index = if vectors.is_empty() {
            VectorIndex::build(&[], 100)
        } else {
            VectorIndex::build(&vectors, 100)
        };

        // Acquire write lock, swap state, then clear dirty.
        // mark_dirty() only sets the AtomicBool (no lock needed), so if a
        // writer calls mark_dirty() *after* our load_all_vectors snapshot,
        // the next ensure_index_fresh will see dirty=true and rebuild again.
        let mut state = self
            .index_state
            .write()
            .map_err(|e| MemoryError::Lock(format!("IndexState lock poisoned: {e}")))?;
        state.index = new_index;
        state.id_map = id_map;
        self.dirty.store(false, Ordering::Release);
        // Safety note: the MCP server dispatches one request at a time
        // (block_in_place on a single stdin line loop), so concurrent
        // write+search cannot interleave. If the architecture changes to
        // allow concurrency, this should be replaced with a generation
        // counter to avoid clearing a dirty flag set after our DB snapshot.
        Ok(())
    }
}
