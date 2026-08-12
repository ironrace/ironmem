//! Shared `#[cfg(test)]` fixtures for `mcp::tools` unit tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Config, EmbedMode, McpAccessMode};

pub(crate) fn test_app_with_db_path(db_path: PathBuf, root: &Path) -> Arc<crate::mcp::app::App> {
    let config = Config {
        db_path,
        model_dir: root.join("model"),
        model_dir_explicit: true,
        state_dir: root.join("state"),
        mcp_access_mode: McpAccessMode::Trusted,
        embed_mode: EmbedMode::Noop,
    };
    #[allow(clippy::arc_with_non_send_sync)]
    Arc::new(crate::mcp::app::App::new(config).unwrap())
}
