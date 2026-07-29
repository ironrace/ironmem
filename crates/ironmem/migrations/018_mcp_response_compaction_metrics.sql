-- Migration 018: response-size telemetry for opt-in MCP compaction.
--
-- NULL means compaction was disabled or the response was ineligible. Values
-- are serialized JSON byte lengths before and after the compact transform.

ALTER TABLE token_usage ADD COLUMN original_response_bytes INTEGER
    CHECK (original_response_bytes IS NULL OR original_response_bytes >= 0);

ALTER TABLE token_usage ADD COLUMN compacted_response_bytes INTEGER
    CHECK (compacted_response_bytes IS NULL OR compacted_response_bytes >= 0);

INSERT OR IGNORE INTO schema_version (version) VALUES (18);
