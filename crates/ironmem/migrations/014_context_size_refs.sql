-- Migration 014: compact collab task-list refs and MCP-response tool attribution.
--
-- task_list_drawer_id lets new collab sessions expose the potentially large
-- task list by deterministic drawer reference instead of inlining it in every
-- status response. Existing sessions keep NULL and are treated as legacy.
--
-- tool_name attributes source='mcp_response' sizing rows to the MCP tool that
-- produced the response. Non-tool protocol responses keep NULL.

ALTER TABLE collab_sessions ADD COLUMN task_list_drawer_id TEXT
    CHECK (
        task_list_drawer_id IS NULL
        OR length(task_list_drawer_id) IN (16, 32)
    );

ALTER TABLE token_usage ADD COLUMN tool_name TEXT;

CREATE INDEX IF NOT EXISTS idx_token_usage_mcp_tool
    ON token_usage (collab_session_id, tool_name)
    WHERE source = 'mcp_response';

INSERT OR IGNORE INTO schema_version (version) VALUES (14);
