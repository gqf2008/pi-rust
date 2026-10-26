//! MCP ↔ harness boundary glue: notice draining and tool-slice refresh.
//!
//! The server-side pieces ([`manager`]/[`tool`]) know nothing about a live
//! [`AgentHarness`]; this module is the one place that crosses over. It is
//! consumed at **session boundaries only** (run tail, between runs, command
//! dispatch when no run is active, AgentEnd, drive-turn edges, ACP
//! between-turn) — never mid-run, because the appended entry would splice
//! between the live run's entries.

use std::collections::HashSet;

use kiqi_harness::agent_harness::AgentHarness;
use kiqi_harness::types::HarnessTool;

use crate::manager::McpManager;

/// Drain MCP resource-update notices and refresh the MCP tool slice, WITHOUT
/// recording anything. The caller decides how the batch reaches the transcript:
/// [`drain_mcp_boundary`] records it and stops there (the non-interactive
/// modes); the interactive TUI records it and may additionally emit a wake
/// trigger (`signal_wake`).
///
/// `autoRead` is applied here: configured uris are read now and their text
/// rides the batch; every other notice stays a pointer.
pub async fn take_mcp_boundary(harness: &AgentHarness, mcp: &McpManager) -> crate::McpNoticeBatch {
    let batch = mcp.take_notices_with_auto_read().await;
    // A `tools/list_changed` / `resources/list_changed` refreshes the cached
    // lists; a changed set is swapped into the harness with a narrow slice
    // replace (not a full `set_tools` from scratch) so everything non-MCP —
    // and each tool's active/inactive state — is preserved untouched.
    if mcp.refresh_if_stale().await {
        replace_mcp_tools(harness, mcp).await;
    }
    batch
}

/// Record one drained batch as the single batched custom entry (the goal extension
/// injection precedent). It becomes a user message on the next context build.
pub async fn append_notice_batch(harness: &AgentHarness, batch: &crate::McpNoticeBatch) {
    if batch.is_empty() {
        return;
    }
    let message = crate::notice_message(batch);
    if let Err(error) = harness.session().append_message(message).await {
        eprintln!("warning: mcp: could not record resource notice: {error}");
    }
}

/// Deliver MCP resource-update notices at a session boundary and refresh the
/// MCP tool slice when a server sent `list_changed`.
///
/// The notice is ONE batched custom entry (the goal extension injection
/// precedent); it becomes a user message on the next context build. Reading
/// the resource is on demand (`{server}__read_resource`) — the notice never
/// carries content.
///
/// Returns the drained (server, uri) notices so a caller with a user-facing
/// channel (ACP `SessionUpdate::Notice`) can forward them; non-ACP callers
/// ignore the result. The model copy above is the only copy those callers had.
pub async fn drain_mcp_boundary(harness: &AgentHarness, mcp: &McpManager) -> Vec<crate::McpNotice> {
    let batch = take_mcp_boundary(harness, mcp).await;
    append_notice_batch(harness, &batch).await;
    batch.notices
}

/// Replace exactly the MCP-contributed tools in the harness registry.
///
/// Chosen over re-running the whole reload-style merge because the boundary
/// callers hold no `Args` and must not disturb the rest of the registry (or
/// the user's per-tool active state from `set_active_tools`). MCP-ownership
/// is decided by the names MCP actually injected ([`McpManager::injected_tools`],
/// falling back to the [`McpManager::owns_tool_name`] prefix before the first
/// merge) — the prefix alone would also remove a non-MCP tool that won a
/// build-time name collision against a server's namespace. The fresh set is
/// gated by the recorded build-time policy ([`McpManager::tool_allowed`]) and
/// the collision gate; active state is preserved per name, with genuinely new
/// MCP names starting active.
async fn replace_mcp_tools(harness: &AgentHarness, mcp: &McpManager) {
    // `get_active_tools`/`set_active_tools` live on the lane trait; the tool
    // registry itself is inherent to the harness.
    let lane = harness.lane("main");
    let current = match harness.get_tools().await {
        Ok(tools) => tools,
        Err(error) => {
            eprintln!("warning: mcp: could not read the tool registry: {error}");
            return;
        }
    };
    let active_before: HashSet<String> = match lane.get_active_tools().await {
        Ok(names) => names.into_iter().collect(),
        Err(error) => {
            eprintln!("warning: mcp: could not read the active tools: {error}");
            return;
        }
    };
    // Removal boundary: the names MCP actually injected, not the `{server}__`
    // prefix — a non-MCP tool that won a build-time collision against that
    // namespace must pass through untouched.
    let injected = mcp.injected_tools();
    let is_mcp_owned = |name: &str| match &injected {
        Some(names) => names.contains(name),
        None => mcp.owns_tool_name(name),
    };
    let previous_mcp: HashSet<String> = current
        .iter()
        .map(|tool| tool.tool.schema().name.clone())
        .filter(|name| is_mcp_owned(name))
        .collect();
    // Non-MCP tools pass through untouched, in their original order.
    let mut tools: Vec<HarnessTool> = current
        .into_iter()
        .filter(|tool| !is_mcp_owned(&tool.tool.schema().name))
        .collect();
    let mut taken: HashSet<String> = tools
        .iter()
        .map(|tool| tool.tool.schema().name.clone())
        .collect();
    let mut injected_now: HashSet<String> = HashSet::new();
    for adapter in mcp.tool_adapters() {
        let name = adapter.schema().name.clone();
        if !mcp.tool_allowed(&name) {
            continue;
        }
        if !taken.insert(name.clone()) {
            eprintln!("warning: MCP tool {name} collides with an existing tool; skipped");
            continue;
        }
        injected_now.insert(name);
        tools.push(HarnessTool::new(adapter));
    }
    // Active state: every surviving tool keeps whatever the user last set
    // (including an MCP tool deactivated via `set_active_tools`); a genuinely
    // NEW MCP name (freshly injected, not present before the replace) starts
    // active — the build default.
    let active: Vec<String> = tools
        .iter()
        .map(|tool| tool.tool.schema().name.clone())
        .filter(|name| {
            active_before.contains(name)
                || (injected_now.contains(name) && !previous_mcp.contains(name))
        })
        .collect();
    if let Err(error) = harness.set_tools(tools, Some(active)).await {
        eprintln!("warning: mcp: could not install the refreshed MCP tools: {error}");
        return;
    }
    // Record only on success, so the registry and the recorded set stay in
    // sync for the next boundary.
    mcp.record_injected_tools(injected_now);
}
