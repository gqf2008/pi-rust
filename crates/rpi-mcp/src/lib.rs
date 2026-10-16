//! Host-level MCP (Model Context Protocol) client for ki.
//!
//! MCP is a **host capability** here, not a plugin: external servers are
//! configured in ki's own `settings.json` (`mcpServers`), the connection lives
//! with the session, and each discovered tool is expanded into a native ki tool
//! named `{server}__{tool}` so the model sees it directly.
//!
//! Batch 1 is stdio only. The pieces:
//! * [`config`] — the `mcpServers` shape (reused from the retired adapter),
//!   validation, and global/project merge;
//! * [`manager`] — connect, handshake, enumerate tools, and reap the process
//!   tree on shutdown;
//! * [`tool`] — the `AgentTool` adapter and MCP content → ki content mapping.
//!
//! Nothing is written to stdout from this crate; diagnostics go to stderr.

pub mod config;
pub mod manager;
mod tool;

pub use config::{
    merge_servers, parse_servers, HttpTransport, ServerConfig, StdioTransport, Transport,
};
pub use manager::McpManager;
