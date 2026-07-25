//! Application state — initialized once, shared across MCP tool handlers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use ironrace_core::VectorIndex;
use ironrace_embed::Embedder;

use crate::config::{Config, EmbedMode};
use crate::db::schema::Database;
use crate::error::MemoryError;
use crate::mcp::readiness::{ReadinessGate, ReadinessState};
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
    /// Resolves once background memory init (model load + bootstrap) completes
    /// (or fails). Pending during warm-up.
    ///
    /// How a tool should consult this depends on its shape, and getting it
    /// wrong is how warm-up writes were silently discarded:
    ///
    /// - **Write-shaped** (needs the embedder): add the tool's name to
    ///   `tools::WRITE_SHAPED_TOOLS`. That is the ONLY step — `call_tool` and
    ///   `server::dispatch_request` derive the wait from that list. Do NOT
    ///   call `App::wait_for_write_ready` from a handler: that runs the
    ///   SYNCHRONOUS wait on the thread that owns the `App` and freezes every
    ///   connection for the readiness timeout. Returning a soft `warming_up`
    ///   body is worse still — a success-shaped response for a write that
    ///   never happened.
    /// - **Read-shaped**: branch on `App::readiness_snapshot`. A soft
    ///   `warming_up` body is correct for `Pending` only; `Failed` is terminal
    ///   and must be reported as an error, not as "try again shortly".
    ///
    /// `is_warming_up()` is the narrow "can I touch the embedder" check and
    /// collapses `Pending` and `Failed` — do not report it to a client.
    pub memory_ready: Arc<ReadinessGate>,
    /// Guards the one-time HNSW rebuild triggered when `memory_ready` resolves `Ready`.
    memory_ready_rebuilt: AtomicBool,
    /// Active collab sessions this server process is participating in, scoped
    /// by repository path and branch. Set by
    /// `collab_start`/`collab_start_code_review` and refreshed by
    /// `collab_send`/`collab_recv`/`collab_wait_my_turn`; deliberately NOT set
    /// by `collab_status`, which is also used to inspect foreign/stale
    /// sessions. Requests carrying a collab session id resolve that id exactly;
    /// unscoped work is attributed only when this map has one unambiguous
    /// binding. This permits concurrent collaboration in separate repository
    /// branches without stamping work onto an unrelated session.
    active_collab_sessions: RwLock<HashMap<(String, String), String>>,
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
            memory_ready: Arc::new(ReadinessGate::new_ready()),
            memory_ready_rebuilt: AtomicBool::new(true),
            active_collab_sessions: RwLock::new(HashMap::new()),
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
            memory_ready: Arc::new(ReadinessGate::new_pending()),
            memory_ready_rebuilt: AtomicBool::new(false),
            active_collab_sessions: RwLock::new(HashMap::new()),
            explicit_task_tag: RwLock::new(None),
            active_collab_generations: RwLock::new(std::collections::HashMap::new()),
        })
    }

    /// Mark `id` as the active collab session for metrics attribution in one
    /// repository-and-branch scope.
    pub fn set_active_collab_session_for_scope(&self, id: &str, repo_path: &str, branch: &str) {
        self.active_collab_sessions
            .write()
            .expect("active_collab_sessions lock poisoned")
            .insert((repo_path.to_string(), branch.to_string()), id.to_string());
    }

    /// Return the active session bound to exactly this repository-and-branch
    /// scope, if any.
    pub fn active_collab_session_snapshot_for_scope(
        &self,
        repo_path: &str,
        branch: &str,
    ) -> Option<String> {
        self.active_collab_sessions
            .read()
            .expect("active_collab_sessions lock poisoned")
            .get(&(repo_path.to_string(), branch.to_string()))
            .cloned()
    }

    /// Remove a scope binding only when it still refers to `id`. This prevents
    /// ending or self-healing an older session from erasing a newer binding.
    pub fn clear_active_collab_session_for_scope_if_matches(
        &self,
        id: &str,
        repo_path: &str,
        branch: &str,
    ) {
        let mut sessions = self
            .active_collab_sessions
            .write()
            .expect("active_collab_sessions lock poisoned");
        let key = (repo_path.to_string(), branch.to_string());
        if sessions.get(&key).is_some_and(|bound| bound == id) {
            sessions.remove(&key);
        }
    }

    /// Return the sole active binding, including its scope. `None` means no
    /// binding or more than one binding, both of which are intentionally
    /// ineligible for implicit attribution.
    pub fn sole_active_collab_session_snapshot(&self) -> Option<((String, String), String)> {
        let sessions = self
            .active_collab_sessions
            .read()
            .expect("active_collab_sessions lock poisoned");
        (sessions.len() == 1).then(|| {
            let (scope, id) = sessions.iter().next().expect("len checked");
            (scope.clone(), id.clone())
        })
    }

    pub fn active_collab_session_count(&self) -> usize {
        self.active_collab_sessions
            .read()
            .expect("active_collab_sessions lock poisoned")
            .len()
    }

    /// Return every active collab binding from one map read, in a stable order
    /// for status output. Each tuple is `(repo_path, branch, session_id)`.
    pub fn active_collab_sessions_snapshot(&self) -> Vec<(String, String, String)> {
        let mut sessions: Vec<_> = self
            .active_collab_sessions
            .read()
            .expect("active_collab_sessions lock poisoned")
            .iter()
            .map(|((repo_path, branch), session_id)| {
                (repo_path.clone(), branch.clone(), session_id.clone())
            })
            .collect();
        sessions.sort_unstable();
        sessions
    }

    /// Compatibility helper for internal callers that have only a session id.
    /// Production collab handlers must use `set_active_collab_session_for_scope`.
    pub fn set_active_collab_session(&self, id: &str) {
        if let Ok(record) = self.db.collab_load_session_record(id) {
            self.set_active_collab_session_for_scope(id, &record.repo_path, &record.branch);
        } else {
            self.set_active_collab_session_for_scope(id, "", "");
        }
    }

    /// Clear all active collab bindings. Production lifecycle paths must use
    /// `clear_active_collab_session_for_scope_if_matches`.
    pub fn clear_active_collab_session(&self) {
        self.active_collab_sessions
            .write()
            .expect("active_collab_sessions lock poisoned")
            .clear();
    }

    /// Return the sole active session. Multiple scopes deliberately report no
    /// single active id to status and other unscoped callers.
    pub fn active_collab_session_snapshot(&self) -> Option<String> {
        self.sole_active_collab_session_snapshot().map(|(_, id)| id)
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

    /// Returns true while the embedder is not usable — which covers BOTH an
    /// in-progress warm-up and a startup that failed outright.
    ///
    /// Use this only to decide whether the embedder can be touched. Anything
    /// that reports state back to a client must use
    /// [`App::readiness_snapshot`] instead: answering "warming up, try again
    /// shortly" to a client whose server failed at startup is a lie the client
    /// will poll on forever. See [`ReadinessState`].
    pub fn is_warming_up(&self) -> bool {
        !self.memory_ready.is_ready()
    }

    /// Tri-state readiness, for tools that report warm-up state to a client.
    pub fn readiness_snapshot(&self) -> ReadinessState {
        self.memory_ready.snapshot()
    }

    /// Blocks (bounded) until background memory init resolves, for write-shaped
    /// tool handlers that must never silently no-op during warm-up. Returns
    /// `Err(MemoryError::NotReady(_))` if readiness resolves as failed or the
    /// fail-safe timeout (`Config::write_readiness_timeout`) expires — see
    /// `ReadinessGate::wait_for_write`. Called from `tools::call_tool` for
    /// every `WRITE_SHAPED_TOOLS` entry — not from individual handlers.
    ///
    /// Read-shaped tools do NOT use this: they branch on
    /// `App::readiness_snapshot`, returning a soft `warming_up` body while
    /// `Pending` and an error on `Failed`.
    pub fn wait_for_write_ready(&self) -> Result<(), MemoryError> {
        self.memory_ready
            .wait_for_write(self.config.write_readiness_timeout())
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
        Self::open_for_test_with_db(db, std::path::PathBuf::from(":memory:"), mode)
    }

    /// Like `open_for_test_with_mode`, but opens a caller-provided on-disk DB.
    /// Useful for tests that need multiple App instances to share state.
    #[cfg(test)]
    pub fn open_for_test_at_path_with_mode(
        db_path: &std::path::Path,
        mode: crate::config::McpAccessMode,
    ) -> Result<Self, MemoryError> {
        let db = crate::db::schema::Database::open(db_path)?;
        db.migrate()?;
        Self::open_for_test_with_db(db, db_path.to_path_buf(), mode)
    }

    fn open_for_test_with_db(
        db: crate::db::schema::Database,
        db_path: std::path::PathBuf,
        mode: crate::config::McpAccessMode,
    ) -> Result<Self, MemoryError> {
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
            db_path,
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
            memory_ready: Arc::new(ReadinessGate::new_ready()),
            memory_ready_rebuilt: AtomicBool::new(true),
            active_collab_sessions: RwLock::new(HashMap::new()),
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
            .expect("active_collab_generations lock poisoned")
            .get(&(session_id.to_string(), agent))
            .copied()
    }

    /// Bind/refresh this process's cached active generation for (session, agent).
    pub(crate) fn set_cached_generation(
        &self,
        session_id: &str,
        agent: crate::collab::Agent,
        generation: u64,
    ) {
        self.active_collab_generations
            .write()
            .expect("active_collab_generations lock poisoned")
            .insert((session_id.to_string(), agent), generation);
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
    ///
    /// Ordering note: this used to load the old `Arc<AtomicBool>` with
    /// `Ordering::Acquire`, pairing with a `Release` store in
    /// `bootstrap::run_background_memory_init`. `ReadinessGate::is_ready()`
    /// (Task 3) is documented as a plain `Relaxed` load — matching
    /// `is_warming_up()`'s existing semantics — and `resolve_ready()` stores
    /// its fast-path flag with `Relaxed` too, so there was never a `Release`
    /// counterpart on the `ReadinessGate` side to pair with an `Acquire` load
    /// in the first place; an `Acquire` load here would be acquire-in-name
    /// only, with no matching release to synchronize against. The real data
    /// this method depends on (drawers written by the background bootstrap)
    /// lives in SQLite, reached through a *separate* DB connection owned by
    /// the background thread's own `App`; cross-connection visibility is
    /// provided by SQLite's own file-level locking/WAL protocol (a syscall
    /// boundary, which is already a full barrier), not by Rust's atomic
    /// ordering on this flag. So downgrading this check to `is_ready()`
    /// (`Relaxed`) does not remove a real happens-before guarantee.
    /// Failure handling: the latch is claimed up front so the reload is
    /// single-flight, but RELEASED again if the reload fails, so the next
    /// caller retries. Leaving it claimed on failure would be permanent — the
    /// noop embedder stays installed and every later write persists an
    /// all-zero vector that no search can ever match, with each individual
    /// call returning `Ok(())`.
    pub fn ensure_embedder_ready(&self) -> Result<(), MemoryError> {
        // Fail CLOSED. Returning `Ok(())` while the gate is unresolved used to
        // leave the caller embedding through the noop embedder installed by
        // `new_server_ready`, persisting an all-zero vector that no search can
        // ever match — a silent, permanent data loss reported as success.
        //
        // That made `tools::WRITE_SHAPED_TOOLS` a *correctness* invariant: a
        // tool that embedded but was missing from the list would corrupt data
        // silently. With this check the list is only an optimization (park on
        // the gate and then succeed, rather than erroring), and the failure
        // mode of forgetting it is a loud error instead of unsearchable rows.
        if !self.memory_ready.is_ready() {
            return Err(match self.memory_ready.snapshot() {
                ReadinessState::Failed(reason) => MemoryError::NotReady(reason),
                _ => MemoryError::NotReady(
                    "server memory initialization is still in progress; the embedder \
                     is not loaded yet"
                        .to_string(),
                ),
            });
        }
        if !self.memory_ready_rebuilt.swap(true, Ordering::AcqRel) {
            if let Err(e) = self.reload_embedder() {
                self.memory_ready_rebuilt.store(false, Ordering::Release);
                return Err(e);
            }
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

fn normalize_session_id(value: &str) -> Option<String> {
    let sanitized = crate::sanitize::sanitize_session_id(value);
    if sanitized == "unknown" {
        None
    } else {
        Some(sanitized)
    }
}

/// Extract a harness session id from MCP `initialize` params. Probes the
/// common locations clients may use (top-level `sessionId`/`session_id`, or
/// nested under `_meta`). Returns `None` if absent or the value sanitizes to
/// `"unknown"`.
///
/// Pure and connection-agnostic: callers (e.g. `mcp::server::ConnectionContext`)
/// own the "set once per connection" semantics — this function only extracts
/// and normalizes, it never mutates any state.
pub(crate) fn session_id_from_params(params: &serde_json::Value) -> Option<String> {
    params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .or_else(|| params.get("_meta").and_then(|m| m.get("sessionId")))
        .or_else(|| params.get("_meta").and_then(|m| m.get("session_id")))
        .and_then(|v| v.as_str())
        .and_then(normalize_session_id)
}

/// Extract a metrics harness id from MCP `initialize.clientInfo` (or
/// `_meta.clientInfo`). Pure and connection-agnostic — see
/// `session_id_from_params`.
pub(crate) fn harness_from_client_info(params: &serde_json::Value) -> Option<String> {
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
    crate::harness::classify_client_info(&name, crate::harness::REGISTRY).map(|id| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{HarnessSpec, TranscriptParserKind};

    // ---- ensure_embedder_ready failure handling ----------------------------

    /// `ensure_embedder_ready` swaps a one-shot latch to guarantee the real
    /// embedder is loaded at most once. If that latch is claimed *before*
    /// `reload_embedder()` is known to have succeeded, a failed load is
    /// permanent: no later call retries it, and every subsequent write embeds
    /// through the still-installed noop embedder, persisting all-zero vectors
    /// that are silently unsearchable forever.
    ///
    /// A failure must therefore leave the latch unclaimed, so the next call
    /// tries again and — until it succeeds — keeps reporting the error rather
    /// than returning `Ok(())` against a noop embedder.
    /// `ensure_embedder_ready` must FAIL rather than return `Ok(())` while the
    /// gate is unresolved. Returning `Ok` let the caller embed through the noop
    /// embedder and persist an all-zero vector that no search can ever match —
    /// silent, permanent data loss reported as success. `tools::WRITE_SHAPED_TOOLS`
    /// is documented as only an optimization *because* of this check, so the
    /// check has to be pinned.
    #[test]
    fn ensure_embedder_ready_fails_closed_while_not_ready() {
        let mut app = App::open_for_test().unwrap();

        app.memory_ready = Arc::new(ReadinessGate::new_pending());
        let pending = app.ensure_embedder_ready();
        assert!(
            matches!(pending, Err(MemoryError::NotReady(_))),
            "a Pending gate must fail closed, got {pending:?}"
        );

        let gate = ReadinessGate::new_pending();
        gate.resolve_failed("model load exploded".to_string());
        app.memory_ready = Arc::new(gate);
        let failed = app.ensure_embedder_ready();
        assert!(
            matches!(&failed, Err(MemoryError::NotReady(reason))
                if reason.contains("model load exploded")),
            "a Failed gate must surface its reason, got {failed:?}"
        );
    }

    #[test]
    fn ensure_embedder_ready_retries_after_a_failed_reload() {
        let mut app = App::open_for_test().unwrap();
        // Point at a model dir that does not exist, marked explicit so the
        // loader fails outright instead of trying to fetch a model.
        app.config.embed_mode = EmbedMode::Real;
        app.config.model_dir = std::path::PathBuf::from("/nonexistent/ironmem-test-model");
        app.config.model_dir_explicit = true;
        // `open_for_test` starts with the reload already accounted for; clear
        // it so this exercises the real post-warm-up reload path.
        app.memory_ready_rebuilt.store(false, Ordering::Release);

        let first = app.ensure_embedder_ready();
        assert!(
            first.is_err(),
            "a model dir that cannot be loaded must surface as an error"
        );

        let second = app.ensure_embedder_ready();
        assert!(
            second.is_err(),
            "a failed embedder load must be retried, not latched as done — \
             returning Ok() here would leave the noop embedder installed and \
             every later write would persist all-zero vectors"
        );
    }

    // ---- harness_from_client_info wiring -----------------------------------

    #[test]
    fn harness_from_client_info_codex_cli() {
        let params = serde_json::json!({ "clientInfo": { "name": "codex-cli", "version": "1.0" } });
        assert_eq!(harness_from_client_info(&params).as_deref(), Some("codex"));
    }

    #[test]
    fn harness_from_client_info_claude() {
        let params = serde_json::json!({ "clientInfo": { "name": "claude", "version": "1.0" } });
        assert_eq!(harness_from_client_info(&params).as_deref(), Some("claude"));
    }

    #[test]
    fn harness_from_client_info_unknown() {
        let params = serde_json::json!({ "clientInfo": { "name": "unknown-tool" } });
        assert!(harness_from_client_info(&params).is_none());
    }

    // ---- IRONMEM_HARNESS env resolution ------------------------------------
    //
    // `mcp::server::mcp_harness` reads IRONMEM_HARNESS and passes it to
    // canonicalize_input. Manipulating a process-global env var in parallel
    // tests is racy, so we test the delegate directly, which covers the same
    // logical path without shared-state races.

    #[test]
    fn env_metrics_harness_delegate_accepts_claude_code_alias() {
        // "claude-code" is an env_alias for claude; confirm canonicalize_input
        // returns "claude", which is what `mcp_harness` produces when
        // IRONMEM_HARNESS="claude-code".
        let result = crate::harness::canonicalize_input("claude-code", crate::harness::REGISTRY);
        assert_eq!(result, Some("claude"));
    }

    #[test]
    fn env_metrics_harness_delegate_unknown_returns_none() {
        // An unregistered value produces None, so `mcp_harness` also falls
        // back to its default for unknown IRONMEM_HARNESS values. "gemini" is
        // now a REAL registered harness (#190 Task 11); use a placeholder id
        // that is genuinely absent from REGISTRY instead.
        let result =
            crate::harness::canonicalize_input("not-a-real-harness", crate::harness::REGISTRY);
        assert!(result.is_none());
    }

    // ---- third-harness classification via injected registry -----------------

    const GEMINI_SPEC: HarnessSpec = HarnessSpec {
        id: "gemini",
        display_name: "Gemini CLI",
        binary: "gemini",
        rules_file: "GEMINI.md",
        rules_strategy: crate::harness::RulesStrategy::Import {
            directive: "@./AGENTS.md",
        },
        write_rules_default: false,
        client_info_aliases: &["gemini"],
        env_aliases: &["gemini"],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    };

    #[test]
    fn classify_client_info_gemini_in_injected_registry() {
        // Verify that a third registered harness attributes correctly through
        // the registry-slice helper — same code path as harness_from_client_info.
        let reg = [
            crate::harness::REGISTRY[0],
            crate::harness::REGISTRY[1],
            GEMINI_SPEC,
        ];
        let result = crate::harness::classify_client_info("gemini-cli", &reg);
        assert_eq!(result, Some("gemini"));
    }
}
