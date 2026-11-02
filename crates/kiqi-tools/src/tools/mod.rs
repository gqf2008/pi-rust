//! Mirrors `packages/agent/src/harness/tools/*` — the built-in `read`/`write`/
//! `edit`/`bash` `AgentTool` impls + their shared helpers.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod powershell;
pub mod read;
pub mod tool_context;
pub mod write;

pub use bash::{create_bash_tool, BashExecution, BashPrepare, BashToolOptions};
pub use edit::create_edit_tool;
pub use powershell::create_powershell_tool;
pub use read::{create_read_tool, ProcessedImage, ReadImageProcessor, ReadToolOptions};
pub use tool_context::ExecutionToolContext;
pub use write::create_write_tool;
