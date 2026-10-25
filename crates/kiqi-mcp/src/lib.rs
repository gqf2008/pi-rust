//! Host-level MCP (Model Context Protocol) client for ki.
//!
//! MCP is a **host capability** here, not a plugin: external servers are
//! configured in ki's own `settings.json` (`mcpServers`), the connection lives
//! with the session, and each discovered tool is expanded into a native ki tool
//! named `{server}__{tool}` so the model sees it directly.
//!
//! The pieces:
//! * [`config`] — the `mcpServers` shape (reused from the retired adapter),
//!   validation, and global/project merge;
//! * [`manager`] — connect (stdio child or HTTP/Streamable session), handshake,
//!   enumerate tools and resources, run the resource-subscription lifecycle,
//!   and reap the process tree / delete the HTTP session on shutdown;
//! * [`tool`] — the `AgentTool` adapters (one per discovered tool, plus a
//!   `{server}__read_resource` per server advertising resources and a
//!   `{server}__watch_resource` when it also advertises
//!   `resources.subscribe`) and MCP content → ki content mapping;
//! * [`notice`] — the bounded, coalescing mailbox behind the light-notice
//!   product semantic: a `notifications/resources/updated` delivers a
//!   (server, uri) notice, never an automatic content read. `autoRead` uris are
//!   the explicit opt-in that also carries content.
//!
//! Nothing is written to stdout from this crate; diagnostics go to stderr.

pub mod boundary;
pub mod config;
mod http;
pub mod manager;
mod notice;
mod tool;

pub use config::{
    merge_servers, merge_signals_defaults, parse_servers, parse_servers_with_defaults,
    parse_signals_defaults, HttpTransport, ServerConfig, SignalsConfig, SignalsDefaults,
    StdioTransport, Transport, WakeMode,
};
pub use manager::{
    McpManager, MAX_MODEL_SUBSCRIPTIONS, READ_RESOURCE_SUFFIX, TOOL_NAME_SEPARATOR,
    WATCH_RESOURCE_SUFFIX,
};
pub use notice::{
    notice_entry_text, notice_message, McpNotice, McpNoticeBatch, McpNoticeMailbox,
    NOTICE_CUSTOM_TYPE,
};
