-- ironrace-memory schema v1

CREATE TABLE IF NOT EXISTS drawers (
    id          TEXT PRIMARY KEY,
    content     TEXT NOT NULL,
    embedding   BLOB NOT NULL,
    wing        TEXT NOT NULL,
    room        TEXT NOT NULL DEFAULT 'general',
    source_file TEXT NOT NULL DEFAULT '',
    added_by    TEXT NOT NULL DEFAULT 'mcp',
    filed_at    TEXT NOT NULL DEFAULT (datetime('now')),
    date        TEXT NOT NULL DEFAULT (date('now'))
);

CREATE INDEX IF NOT EXISTS idx_drawers_wing ON drawers(wing);
CREATE INDEX IF NOT EXISTS idx_drawers_room ON drawers(room);
CREATE INDEX IF NOT EXISTS idx_drawers_wing_room ON drawers(wing, room);
CREATE INDEX IF NOT EXISTS idx_drawers_source ON drawers(source_file);

CREATE TABLE IF NOT EXISTS entities (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    entity_type TEXT NOT NULL DEFAULT 'unknown',
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS triples (
    id            TEXT PRIMARY KEY,
    subject       TEXT NOT NULL REFERENCES entities(id),
    predicate     TEXT NOT NULL,
    object        TEXT NOT NULL REFERENCES entities(id),
    valid_from    TEXT,
    valid_to      TEXT,
    confidence    REAL NOT NULL DEFAULT 1.0,
    source_closet TEXT,
    extracted_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_triples_subject ON triples(subject);
CREATE INDEX IF NOT EXISTS idx_triples_object ON triples(object);
CREATE INDEX IF NOT EXISTS idx_triples_predicate ON triples(predicate);
CREATE INDEX IF NOT EXISTS idx_triples_valid ON triples(valid_from, valid_to);
CREATE UNIQUE INDEX IF NOT EXISTS idx_triples_current_identity
    ON triples(subject, predicate, object)
    WHERE valid_to IS NULL;

CREATE TABLE IF NOT EXISTS wal_log (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL DEFAULT (datetime('now')),
    operation TEXT NOT NULL,
    params    TEXT NOT NULL,
    result    TEXT
);

CREATE INDEX IF NOT EXISTS idx_wal_timestamp ON wal_log(timestamp);
CREATE INDEX IF NOT EXISTS idx_entities_name_lower ON entities(name COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS schema_version (
    version    INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT OR IGNORE INTO schema_version (version) VALUES (1);
