-- Migration 012: local symbol/import graph index (feature: symbol-import-graph-index).
-- Adds four tables for indexing source symbols, imports, and edges
-- incrementally from a git worktree. No full source bodies are persisted —
-- only bounded declaration metadata (signature/raw, each truncated to
-- MAX_SNIPPET_LEN before storage).
--
-- Supported in v0: Rust + Python only. TS/JS deferred.
-- Edge scope in v0: `import` (file → module) and `contains` (symbol → parent)
-- only. Cross-symbol call/reference resolution is deferred (table created now
-- for forward-compatibility).
--
-- All DDL is idempotent (IF NOT EXISTS).

-- Per-file index tracking: one row per (repo, path) combination.
-- content_hash = SHA-256 hex of file bytes for incremental-update decisions.
-- head_sha = git HEAD SHA at index time ("0000...0" placeholder when no commit).
CREATE TABLE IF NOT EXISTS code_index_files (
    repo         TEXT NOT NULL,
    path         TEXT NOT NULL,    -- repo-relative, forward-slash
    head_sha     TEXT NOT NULL,
    content_hash TEXT NOT NULL,    -- SHA-256 hex of file bytes
    language     TEXT NOT NULL,
    indexed_at   TEXT NOT NULL,
    PRIMARY KEY (repo, path)
);

-- Symbol rows: one per declaration (fn, struct, enum, trait, impl, const,
-- static, type, mod, macro_rules!, class, def).
-- id = SHA-256 hex of "repo:path:kind:qualified_name:start_line:start_col".
-- signature is the declaration header, truncated to MAX_SNIPPET_LEN.
-- parent_id references another row in this table (nullable).
CREATE TABLE IF NOT EXISTS code_symbols (
    id             TEXT NOT NULL PRIMARY KEY,
    repo           TEXT NOT NULL,
    path           TEXT NOT NULL,    -- repo-relative, forward-slash
    language       TEXT NOT NULL,
    name           TEXT NOT NULL,    -- short/local name
    qualified_name TEXT NOT NULL,    -- dotted/path-qualified name
    kind           TEXT NOT NULL,    -- fn|struct|enum|trait|impl|mod|const|static|type|macro|class|method
    visibility     TEXT,             -- pub|pub(crate)|pub(super)|private|null
    signature      TEXT,             -- declaration header, truncated to MAX_SNIPPET_LEN
    start_line     INTEGER NOT NULL,
    start_col      INTEGER NOT NULL,
    end_line       INTEGER,
    parent_id      TEXT,             -- nullable FK into this table
    confidence     REAL NOT NULL DEFAULT 1.0,
    indexed_at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_code_symbols_repo_name
    ON code_symbols (repo, name);
CREATE INDEX IF NOT EXISTS idx_code_symbols_repo_qname
    ON code_symbols (repo, qualified_name);
CREATE INDEX IF NOT EXISTS idx_code_symbols_repo_path
    ON code_symbols (repo, path);

-- Import rows: one per import/use statement.
-- module = the imported module or package name.
-- symbol = the specific imported symbol (if any), e.g. "HashMap" from "std::collections::HashMap".
-- alias = the local alias (if any).
-- raw = the original import statement, truncated to MAX_SNIPPET_LEN.
CREATE TABLE IF NOT EXISTS code_imports (
    id         TEXT NOT NULL PRIMARY KEY,
    repo       TEXT NOT NULL,
    path       TEXT NOT NULL,    -- repo-relative, forward-slash
    language   TEXT NOT NULL,
    module     TEXT NOT NULL,    -- e.g. "std::collections" or "os"
    symbol     TEXT,             -- e.g. "HashMap" (nullable for wildcard/module imports)
    alias      TEXT,             -- e.g. "HashMap as HM" (nullable)
    raw        TEXT,             -- original statement, truncated to MAX_SNIPPET_LEN
    line       INTEGER NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    indexed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_code_imports_repo_module
    ON code_imports (repo, module);
CREATE INDEX IF NOT EXISTS idx_code_imports_repo_path
    ON code_imports (repo, path);

-- Symbol edge rows: directed relationships between symbols or between a file
-- and a module. Created now for forward-compatibility; v0 populates only
-- `import` (file → module) and `contains` (symbol → parent symbol) edges.
-- Cross-symbol call/reference edges are deferred.
--
-- from_kind = "symbol" | "file"
-- to_kind   = "symbol" | "module"
-- edge_kind = "import" | "contains" | "calls" | "references" (calls/refs deferred)
CREATE TABLE IF NOT EXISTS code_symbol_edges (
    id         TEXT NOT NULL PRIMARY KEY,
    repo       TEXT NOT NULL,
    from_kind  TEXT NOT NULL,    -- "symbol" | "file"
    from_id    TEXT NOT NULL,    -- symbol id or repo-relative file path
    to_kind    TEXT NOT NULL,    -- "symbol" | "module"
    to_ref     TEXT NOT NULL,    -- symbol id or module name
    edge_kind  TEXT NOT NULL,    -- "import" | "contains" | "calls" | "references"
    path       TEXT NOT NULL,    -- repo-relative source file for this edge
    line       INTEGER,
    confidence REAL NOT NULL DEFAULT 1.0,
    indexed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_code_symbol_edges_repo_from
    ON code_symbol_edges (repo, from_id);
CREATE INDEX IF NOT EXISTS idx_code_symbol_edges_repo_to
    ON code_symbol_edges (repo, to_ref);
CREATE INDEX IF NOT EXISTS idx_code_symbol_edges_repo_kind
    ON code_symbol_edges (repo, edge_kind);

INSERT OR IGNORE INTO schema_version (version) VALUES (12);
