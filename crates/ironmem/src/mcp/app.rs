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
    /// Test-only — installs a concrete `LlmPreferenceExtractor` that bypasses
    /// the OnceLock-cached `tunables::pref_extractor()` selection so the
    /// pref-extract usage path is exercisable deterministically. `None` in
    /// production; `build_synthetic` consults the tunable when this is unset.
    pub(crate) pref_extractor_override:
        RwLock<Option<Arc<crate::search::pref_extract_llm::LlmPreferenceExtractor>>>,
    /// Test-only: force the rerank stage even when `IRONMEM_RERANK` is unset
    /// (the env gate is OnceLock-cached and can't be flipped per-test).
    pub(crate) force_rerank: bool,
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
    /// Harness session id learned from the MCP `initialize` request (or the
    /// `IRONMEM_SESSION_ID` env), set once. Used to co-key `mcp_chars_served`
    /// with the hook's `session_summary` row (METRICS_SPEC §5.3, Decision D1).
    pub session_id: RwLock<Option<String>>,
    /// Metrics harness attribution learned from MCP `initialize.clientInfo`
    /// (or `IRONMEM_HARNESS`), set once. Stored as the DB enum value.
    pub harness: RwLock<Option<String>>,
    /// Active collab session this server process is participating in. Set by
    /// `collab_start`/`collab_start_code_review` and refreshed by
    /// `collab_send`/`collab_recv`/`collab_wait_my_turn`; deliberately NOT set
    /// by `collab_status`, which is also used to inspect foreign/stale
    /// sessions. Process-global rather than request-local because the dominant
    /// token volume (`search` rerank, pref-extract) carries no collab argument
    /// — only process state can attribute it. Enforced invariant: one active
    /// collab session per server process, regardless of repo — the collab
    /// handlers' conflict guard rejects binding a second still-live session to
    /// this slot. Parallel collab sessions require separate server processes so
    /// `search` / pref-extract / rerank work cannot be stamped onto the wrong
    /// session.
    active_collab_session_id: RwLock<Option<String>>,
    /// Explicit task tag for non-collab work (METRICS_SPEC §2.3 item 2), set
    /// via `status` tool args. Only consulted when no active collab session
    /// resolves.
    explicit_task_tag: RwLock<Option<String>>,
    /// Process-local cache of the ACTIVE generation this MCP process is bound
    /// to per (session_id, agent). Inertness mechanism for the generation lease
    /// (issue #91): a process whose cached generation is behind the DB active
    /// generation is a stale predecessor, rejected from mutating/binding collab
    /// calls. Populated by the first guarded call for a (session, agent) and on
    /// a successful token claim.
    pub(crate) active_collab_generations:
        RwLock<std::collections::HashMap<(String, crate::collab::Agent), u64>>,
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
            pref_extractor_override: RwLock::new(None),
            force_rerank: false,
            index_state: RwLock::new(IndexState { index, id_map }),
            dirty: AtomicBool::new(false),
            graph_cache: RwLock::new(None),
            memory_ready: Arc::new(AtomicBool::new(true)),
            memory_ready_rebuilt: AtomicBool::new(true),
            session_id: RwLock::new(std::env::var("IRONMEM_SESSION_ID").ok()),
            harness: RwLock::new(env_metrics_harness()),
            active_collab_session_id: RwLock::new(None),
            explicit_task_tag: RwLock::new(None),
            active_collab_generations: RwLock::new(std::collections::HashMap::new()),
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
            pref_extractor_override: RwLock::new(None),
            force_rerank: false,
            index_state: RwLock::new(IndexState {
                index: VectorIndex::build(&[], 100),
                id_map: Vec::new(),
            }),
            dirty: AtomicBool::new(false),
            graph_cache: RwLock::new(None),
            memory_ready: Arc::new(AtomicBool::new(false)),
            memory_ready_rebuilt: AtomicBool::new(false),
            session_id: RwLock::new(std::env::var("IRONMEM_SESSION_ID").ok()),
            harness: RwLock::new(env_metrics_harness()),
            active_collab_session_id: RwLock::new(None),
            explicit_task_tag: RwLock::new(None),
            active_collab_generations: RwLock::new(std::collections::HashMap::new()),
        })
    }

    /// Learn metrics context from the MCP `initialize` params.
    pub fn learn_metrics_context(&self, params: &serde_json::Value) {
        self.learn_session_id(params);
        self.learn_harness(params);
    }

    /// Learn the harness session id once, from the MCP `initialize` params.
    /// Probes common locations clients may use; falls back to the env-seeded
    /// value. Never overwrites a value already set (set-once).
    fn learn_session_id(&self, params: &serde_json::Value) {
        let mut guard = self.session_id.write().expect("session_id lock poisoned");
        if guard.is_some() {
            return;
        }
        let found = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .or_else(|| params.get("_meta").and_then(|m| m.get("sessionId")))
            .or_else(|| params.get("_meta").and_then(|m| m.get("session_id")))
            .and_then(|v| v.as_str())
            .and_then(normalize_session_id);
        if found.is_some() {
            *guard = found;
        }
    }

    fn learn_harness(&self, params: &serde_json::Value) {
        let mut guard = self.harness.write().expect("harness lock poisoned");
        if guard.is_some() {
            return;
        }
        if let Some(harness) = harness_from_client_info(params) {
            *guard = Some(harness);
        }
    }

    /// Snapshot of the learned harness session id.
    pub fn session_id_snapshot(&self) -> Option<String> {
        self.session_id
            .read()
            .expect("session_id lock poisoned")
            .clone()
    }

    /// Snapshot of the learned metrics harness attribution.
    pub fn harness_snapshot(&self) -> Option<String> {
        self.harness.read().expect("harness lock poisoned").clone()
    }

    /// Mark `id` as the active collab session for metrics attribution.
    pub fn set_active_collab_session(&self, id: &str) {
        *self
            .active_collab_session_id
            .write()
            .expect("active_collab_session_id lock poisoned") = Some(id.to_string());
    }

    /// Clear the active collab session (ended or missing).
    pub fn clear_active_collab_session(&self) {
        *self
            .active_collab_session_id
            .write()
            .expect("active_collab_session_id lock poisoned") = None;
    }

    pub fn active_collab_session_snapshot(&self) -> Option<String> {
        self.active_collab_session_id
            .read()
            .expect("active_collab_session_id lock poisoned")
            .clone()
    }

    pub fn set_explicit_task_tag(&self, tag: &str) {
        *self
            .explicit_task_tag
            .write()
            .expect("explicit_task_tag lock poisoned") = Some(tag.to_string());
    }

    pub fn clear_explicit_task_tag(&self) {
        *self
            .explicit_task_tag
            .write()
            .expect("explicit_task_tag lock poisoned") = None;
    }

    pub fn explicit_task_tag_snapshot(&self) -> Option<String> {
        self.explicit_task_tag
            .read()
            .expect("explicit_task_tag lock poisoned")
            .clone()
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
            pref_extractor_override: RwLock::new(None),
            force_rerank: false,
            index_state: RwLock::new(IndexState {
                index: VectorIndex::build(&[], 100),
                id_map: Vec::new(),
            }),
            dirty: AtomicBool::new(false),
            graph_cache: RwLock::new(None),
            memory_ready: Arc::new(AtomicBool::new(true)),
            memory_ready_rebuilt: AtomicBool::new(true),
            session_id: RwLock::new(std::env::var("IRONMEM_SESSION_ID").ok()),
            harness: RwLock::new(env_metrics_harness()),
            active_collab_session_id: RwLock::new(None),
            explicit_task_tag: RwLock::new(None),
            active_collab_generations: RwLock::new(std::collections::HashMap::new()),
        })
    }

    /// Cached active generation for (session, agent), if this process bound one.
    pub(crate) fn cached_generation(
        &self,
        session_id: &str,
        agent: crate::collab::Agent,
    ) -> Option<u64> {
        self.active_collab_generations
            .read()
            .ok()
            .and_then(|m| m.get(&(session_id.to_string(), agent)).copied())
    }

    /// Bind/refresh this process's cached active generation for (session, agent).
    pub(crate) fn set_cached_generation(
        &self,
        session_id: &str,
        agent: crate::collab::Agent,
        generation: u64,
    ) {
        if let Ok(mut m) = self.active_collab_generations.write() {
            m.insert((session_id.to_string(), agent), generation);
        }
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

    /// Test-only — like `with_reranker` but also sets `force_rerank` so the
    /// rerank stage runs even when the OnceLock-cached `IRONMEM_RERANK` gate is
    /// unset. Kept separate from `with_reranker` so callers that install a
    /// scorer purely to assert the env gate keeps it dormant (e.g. the
    /// rerank-disabled passthrough test) are unaffected.
    pub fn with_reranker_forced(
        scorer: Arc<dyn ironrace_rerank::RerankerScorer>,
    ) -> Result<Self, MemoryError> {
        let mut app = Self::open_for_test()?;
        app.force_rerank = true;
        *app.reranker.write().unwrap() = Some(scorer);
        Ok(app)
    }

    /// Test-only — install a concrete `LlmPreferenceExtractor` that bypasses
    /// the OnceLock-cached `tunables::pref_extractor()` selection so the
    /// pref-extract usage path is exercisable deterministically. Mirrors
    /// `with_reranker`: builds the base test `App` (in-memory DB, noop
    /// embedder) and installs the override.
    pub fn with_pref_extractor(
        extractor: Arc<crate::search::pref_extract_llm::LlmPreferenceExtractor>,
    ) -> Result<Self, MemoryError> {
        let app = Self::open_for_test()?;
        *app.pref_extractor_override.write().unwrap() = Some(extractor);
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

fn env_metrics_harness() -> Option<String> {
    match std::env::var("IRONMEM_HARNESS").ok().as_deref() {
        Some("codex") => Some("codex".to_string()),
        Some("claude") => Some("claude".to_string()),
        _ => None,
    }
}

fn normalize_session_id(value: &str) -> Option<String> {
    let sanitized = crate::sanitize::sanitize_session_id(value);
    if sanitized == "unknown" {
        None
    } else {
        Some(sanitized)
    }
}

fn harness_from_client_info(params: &serde_json::Value) -> Option<String> {
    let client_info = params
        .get("clientInfo")
        .or_else(|| params.get("client_info"))
        .or_else(|| params.get("_meta").and_then(|m| m.get("clientInfo")))
        .or_else(|| params.get("_meta").and_then(|m| m.get("client_info")))?;
    let name = client_info
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.contains("codex") {
        Some("codex".to_string())
    } else if name.contains("claude") {
        Some("claude".to_string())
    } else {
        None
    }
}
