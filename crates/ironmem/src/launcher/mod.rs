//! One-command launchers: validate the assistant binary, canonicalize and warm
//! the target repo, ensure the ironmem MCP server is registered, then launch
//! the assistant with the repo as its working directory.

mod argv;
mod binary;
mod mcp_setup;
