//! Offline end-to-end run of the MCP manager against a real (fake) stdio server.
//!
//! Every test spawns `tests/support/fake_server.rs` as an actual child process
//! and drives ki's public API — no network, no external server. The liveness
//! assertions are OS-level (`kill -0`), which is the only way to prove the
//! process *tree* is gone and not merely unreferenced.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiqi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use kiqi_agent::{AgentError, AgentTool};
use kiqi_mcp::{parse_servers, McpManager, ServerConfig};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const FAKE_SERVER: &str = env!("CARGO_BIN_EXE_ki-mcp-fake-server");

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Build one stdio server config through the public parser, so the tests also
/// exercise the config path they depend on.
fn stdio_server(name: &str, args: Vec<String>, timeout_secs: u64, enabled: bool) -> ServerConfig {
    stdio_server_with(name, args, timeout_secs, enabled, &[])
}

/// Same, plus config-driven resource subscriptions (`mcpServers.<n>.subscribe`).
fn stdio_server_with(
    name: &str,
    args: Vec<String>,
    timeout_secs: u64,
    enabled: bool,
    subscribe: &[&str],
) -> ServerConfig {
    let value = json!({
        name: {
            "command": FAKE_SERVER,
            "args": args,
            "enabled": enabled,
            "timeoutSeconds": timeout_secs,
            "subscribe": subscribe,
        }
    });
    let (servers, issues) = parse_servers(Some(&value));
    assert!(issues.is_empty(), "config issues: {issues:?}");
    servers.into_iter().next().expect("exactly one server")
}

fn find<'a>(adapters: &'a [Arc<dyn AgentTool>], name: &str) -> &'a Arc<dyn AgentTool> {
    adapters
        .iter()
        .find(|adapter| adapter.schema().name == name)
        .unwrap_or_else(|| {
            let names: Vec<_> = adapters.iter().map(|a| a.schema().name.clone()).collect();
            panic!("tool {name:?} not found; have {names:?}")
        })
}

async fn call(tool: &Arc<dyn AgentTool>, args: Value) -> Result<AgentToolResult, AgentError> {
    let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> =
        Arc::new(|_: ToolResultPartial| {});
    tool.execute("test-call", args, CancellationToken::new(), on_update)
        .await
}

async fn call_with_signal(
    tool: &Arc<dyn AgentTool>,
    args: Value,
    signal: CancellationToken,
) -> Result<AgentToolResult, AgentError> {
    let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> =
        Arc::new(|_: ToolResultPartial| {});
    tool.execute("test-call", args, signal, on_update).await
}

fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| match block {
            TextContentOrImage::Text(text) => text.text.clone(),
            TextContentOrImage::Image(_) => "[image]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Count `--record` lines carrying both `method` and `uri`. The fake server
/// writes one JSON object per observed request/notification.
fn count_recorded(record: &Path, method: &str, uri: &str) -> usize {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(method) && line.contains(uri))
        .count()
}

/// Poll until `f` yields Some, panicking past the deadline (the notifications
/// arrive on rmcp's dispatch task, so every assertion here is a bounded wait,
/// never an immediate read).
fn wait_for<T>(seconds: u64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Poll until `pid` is gone, panicking if it survives the deadline.
fn assert_process_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) {
        assert!(
            Instant::now() < deadline,
            "{what} (pid {pid}) is still alive after shutdown"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read the pidfile written by the fake server, waiting briefly for it to appear.
fn read_pidfile(path: &Path) -> Vec<(String, u32)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let entries: Vec<(String, u32)> = contents
                .lines()
                .filter_map(|line| {
                    let (label, pid) = line.split_once(' ')?;
                    Some((label.to_string(), pid.parse().ok()?))
                })
                .collect();
            if !entries.is_empty() {
                return entries;
            }
        }
        assert!(
            Instant::now() < deadline,
            "pidfile never appeared: {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn discovers_namespaced_tools_and_maps_results() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) =
        McpManager::start(&[stdio_server("fake", vec![], 10, true)], tmp.path()).await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    let adapters = manager.tool_adapters();
    assert_eq!(adapters.len(), 6, "expected all six fake tools to expand");

    // Namespacing: {server}__{tool}.
    let echo = find(&adapters, "fake__echo");
    assert_eq!(echo.label(), "fake__echo");

    let echoed = call(echo, json!({ "text": "hi there" })).await.unwrap();
    assert_eq!(text_of(&echoed), "hi there");

    // isError:true must surface as a tool error, never a silent success.
    let failed = call(find(&adapters, "fake__fail"), json!({})).await;
    let error = failed.expect_err("isError must become an error");
    assert!(
        matches!(error, AgentError::Tool(_)),
        "expected a tool error, got {error:?}"
    );
    assert!(error.to_string().contains("tool exploded"));

    // Image content with data + mimeType maps to a native image block.
    let image = call(find(&adapters, "fake__img"), json!({})).await.unwrap();
    assert!(matches!(
        image.content.first(),
        Some(TextContentOrImage::Image(_))
    ));

    // structuredContent is appended as text.
    let structured = call(find(&adapters, "fake__structured"), json!({}))
        .await
        .unwrap();
    let text = text_of(&structured);
    assert!(text.contains("structuredContent"), "got {text:?}");
    assert!(text.contains("42"), "got {text:?}");

    // Non-object arguments are rejected locally.
    let bad = call(echo, json!("not an object")).await.unwrap_err();
    assert!(bad.to_string().contains("JSON object argument"));

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn paginated_tool_listing_collects_every_page() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[stdio_server(
            "paged",
            vec!["--pages".into(), "3".into()],
            10,
            true,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    assert_eq!(manager.tool_adapters().len(), 6);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_pagination_cursor_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[stdio_server(
            "broken",
            vec!["--repeat-cursor".into()],
            10,
            true,
        )],
        tmp.path(),
    )
    .await;
    assert_eq!(manager.tool_adapters().len(), 0);
    assert!(
        issues.iter().any(|issue| issue.contains("repeated cursor")),
        "expected a repeated-cursor skip, got {issues:?}"
    );
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn per_request_timeout_becomes_a_tool_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) =
        McpManager::start(&[stdio_server("slow", vec![], 1, true)], tmp.path()).await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();

    let error = call(find(&adapters, "slow__sleep"), json!({ "seconds": 30 }))
        .await
        .expect_err("a request past the per-server timeout must fail");
    assert!(
        error.to_string().contains("timeout"),
        "expected a timeout error, got {error}"
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_aborts_an_in_flight_call() {
    let tmp = tempfile::tempdir().unwrap();
    let record = tmp.path().join("notifications.log");
    let (manager, issues) = McpManager::start(
        &[stdio_server(
            "hangy",
            vec!["--record".into(), record.to_string_lossy().into_owned()],
            60,
            true,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();

    let signal = CancellationToken::new();
    let canceller = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        canceller.cancel();
    });

    let error = call_with_signal(find(&adapters, "hangy__hang"), json!({}), signal)
        .await
        .expect_err("a cancelled call must abort");
    assert!(error.is_abort(), "expected Abort, got {error:?}");

    // The MCP cancellation contract: the server must be told to stop working on
    // the abandoned request, not merely left to compute a result nobody reads.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let notified = std::fs::read_to_string(&record)
            .map(|text| text.contains("notifications/cancelled"))
            .unwrap_or(false);
        if notified {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "server never received notifications/cancelled"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_reaps_the_whole_process_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile: PathBuf = tmp.path().join("server.pids");
    let (manager, issues) = McpManager::start(
        &[stdio_server(
            "treed",
            vec![
                "--pidfile".into(),
                pidfile.to_string_lossy().into_owned(),
                "--child".into(),
            ],
            10,
            true,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    // A working call first, so the server is demonstrably up.
    let adapters = manager.tool_adapters();
    call(find(&adapters, "treed__echo"), json!({ "text": "up" }))
        .await
        .unwrap();

    let pids = read_pidfile(&pidfile);
    let self_pid = pids.iter().find(|(l, _)| l == "SELF").unwrap().1;
    let child_pid = pids.iter().find(|(l, _)| l == "CHILD").unwrap().1;
    assert!(process_alive(self_pid), "server should be running");
    assert!(process_alive(child_pid), "grandchild should be running");

    manager.shutdown().await;
    assert_process_gone(self_pid, "server process");
    assert_process_gone(child_pid, "server grandchild");
}

#[tokio::test(flavor = "multi_thread")]
async fn disappearing_process_is_moved_to_a_fresh_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile: PathBuf = tmp.path().join("server.pids");
    let (manager, issues) = McpManager::start(
        &[stdio_server(
            "flaky",
            vec!["--pidfile".into(), pidfile.to_string_lossy().into_owned()],
            10,
            true,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();
    let echo = find(&adapters, "flaky__echo");

    call(echo, json!({ "text": "first" })).await.unwrap();

    // Kill the server out from under ki. The next call must re-initialize a
    // fresh connection rather than fail forever. (The call is retried by the
    // test only to let the manager *notice* the death; production never replays
    // a failed call — it reconnects first, then sends.)
    let old_pid = read_pidfile(&pidfile)
        .into_iter()
        .find(|(l, _)| l == "SELF")
        .unwrap()
        .1;
    let _ = std::process::Command::new("kill")
        .arg(old_pid.to_string())
        .status();
    assert_process_gone(old_pid, "killed server process");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut recovered = false;
    while Instant::now() < deadline {
        match call(echo, json!({ "text": "second" })).await {
            Ok(result) if text_of(&result) == "second" => {
                recovered = true;
                break;
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert!(recovered, "manager never re-initialized the connection");

    let new_pid = read_pidfile(&pidfile)
        .into_iter()
        .find(|(l, _)| l == "SELF")
        .unwrap()
        .1;
    assert_ne!(new_pid, old_pid, "expected a fresh server process");

    manager.shutdown().await;
    assert_process_gone(new_pid, "reconnected server process");
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_server_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) =
        McpManager::start(&[stdio_server("off", vec![], 10, false)], tmp.path()).await;
    assert_eq!(manager.tool_adapters().len(), 0);
    assert!(
        issues.iter().any(|issue| issue.contains("disabled")),
        "expected a disabled-skip notice, got {issues:?}"
    );
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_handshake_is_skipped_loudly_and_does_not_kill_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[
            stdio_server("crasher", vec!["--crash-on-init".into()], 10, true),
            stdio_server("healthy", vec![], 10, true),
        ],
        tmp.path(),
    )
    .await;

    // The bad server is gone; the good one still came up.
    assert_eq!(manager.tool_adapters().len(), 6);
    assert!(
        issues
            .iter()
            .any(|issue| issue.contains("crasher") && issue.contains("skipped")),
        "expected a loud crasher skip, got {issues:?}"
    );
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_http_server_is_skipped_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let value = json!({
        // Port 9 (discard) on loopback: the connect is refused immediately, so
        // this test stays offline and fast while exercising the real HTTP path.
        "remote": { "url": "http://127.0.0.1:9/mcp", "timeoutSeconds": 5 }
    });
    let (servers, issues) = parse_servers(Some(&value));
    assert!(issues.is_empty(), "config issues: {issues:?}");

    let (manager, issues) = McpManager::start(&servers, tmp.path()).await;
    assert_eq!(manager.tool_adapters().len(), 0);
    assert!(
        issues
            .iter()
            .any(|issue| issue.contains("remote") && issue.contains("skipped")),
        "expected a loud http skip, got {issues:?}"
    );
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// batch 2: resources, subscriptions, notices
// ---------------------------------------------------------------------------

const RESOURCES_ARGS: &[&str] = &["--resources"];

/// A resources-mode server exposing the read tool plus its discovery text.
#[tokio::test(flavor = "multi_thread")]
async fn read_tool_is_emitted_only_for_servers_advertising_resources() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[stdio_server(
            "docs",
            RESOURCES_ARGS.iter().map(|s| s.to_string()).collect(),
            10,
            true,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    let adapters = manager.tool_adapters();
    // Six tools + poke + one read tool per resource server.
    assert_eq!(
        adapters.len(),
        8,
        "got {:?}",
        adapters
            .iter()
            .map(|a| a.schema().name.clone())
            .collect::<Vec<_>>()
    );
    let read = find(&adapters, "docs__read_resource");
    let description = read.schema().description.clone();
    assert!(
        description.contains("file:///docs/a.md"),
        "got {description:?}"
    );
    assert!(
        description.contains("file:///docs/{path}"),
        "got {description:?}"
    );

    // A tools-only server (the batch-1 fake without --resources) adds no read
    // tool — the batch-1 tests pin that count at 6 for exactly this reason.
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn read_tool_maps_text_and_images_and_bounds_oversized_text() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[stdio_server(
            "docs",
            RESOURCES_ARGS.iter().map(|s| s.to_string()).collect(),
            10,
            true,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();
    let read = find(&adapters, "docs__read_resource");

    // Text maps to text.
    let text = call(read, json!({ "uri": "file:///docs/a.md" }))
        .await
        .unwrap();
    assert_eq!(text_of(&text), "doc-a body");

    // An image blob maps to a native image block (same policy as tool results).
    let image = call(read, json!({ "uri": "file:///docs/pic.png" }))
        .await
        .unwrap();
    assert!(matches!(
        image.content.first(),
        Some(TextContentOrImage::Image(_))
    ));

    // Oversized text is bounded with the truncation marker, never inlined whole.
    let big = call(read, json!({ "uri": "file:///docs/big.txt" }))
        .await
        .unwrap();
    let big_text = text_of(&big);
    assert!(
        big_text.ends_with("\n[truncated]"),
        "got {} chars",
        big_text.len()
    );
    assert!(big_text.len() < 30_000, "got {} chars", big_text.len());

    // An unknown uri is a tool error, not a silent success.
    let missing = call(read, json!({ "uri": "file:///nope" }))
        .await
        .expect_err("unknown resource must error");
    assert!(missing.to_string().contains("unknown resource"));

    // A malformed argument is rejected locally.
    let bad = call(read, json!({}))
        .await
        .expect_err("missing uri must error");
    assert!(bad.to_string().contains("uri"));

    manager.shutdown().await;
}

/// The adapter semantic: a live subscription means an identical subscribe never
/// reaches the wire twice, and delivery keeps working afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_subscribe_reuses_the_active_remote_subscription() {
    let tmp = tempfile::tempdir().unwrap();
    let record = tmp.path().join("record.log");
    let (manager, issues) = McpManager::start(
        &[stdio_server_with(
            "docs",
            vec![
                "--resources".to_string(),
                "--reject-duplicate-subscribe".to_string(),
                "--record".to_string(),
                record.to_string_lossy().into_owned(),
            ],
            10,
            true,
            &["mem://alpha"],
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    // The config-driven subscribe put exactly one RPC on the wire.
    assert_eq!(
        count_recorded(&record, "resources/subscribe", "mem://alpha"),
        1
    );

    // A duplicate-rejecting server: a second subscribe on the live transport
    // must NOT be sent (it would be rejected) and must not lose the mark.
    manager
        .subscribe("docs", "mem://alpha")
        .await
        .expect("repeated subscribe must be a local no-op");
    assert_eq!(
        count_recorded(&record, "resources/subscribe", "mem://alpha"),
        1
    );

    // Delivery still works after the repeated subscribe.
    let adapters = manager.tool_adapters();
    call(
        find(&adapters, "docs__poke"),
        json!({ "uri": "mem://alpha" }),
    )
    .await
    .unwrap();
    let batch = wait_for(5, "the alpha notice", || {
        let batch = manager.take_notices();
        let hit = batch.notices.iter().any(|n| n.uri == "mem://alpha");
        hit.then_some(batch)
    });
    assert!(
        batch
            .notices
            .iter()
            .any(|n| n.server == "docs" && n.uri == "mem://alpha"),
        "delivery broke after a repeated subscribe: {batch:?}"
    );

    manager.shutdown().await;
}

/// A configured subscription is the user's opt-in to be told, so the parsed
/// policy defaults to `auto`; the manager exposes it so the host can budget the
/// wake. Unknown/failed servers have no policy.
#[tokio::test(flavor = "multi_thread")]
async fn signal_policy_defaults_to_auto_for_a_subscribed_server() {
    let tmp = tempfile::tempdir().unwrap();
    let value = json!({
        "docs": {
            "command": FAKE_SERVER,
            "args": ["--resources"],
            "timeoutSeconds": 10,
            "subscribe": ["mem://alpha"]
        }
    });
    let (servers, issues) = parse_servers(Some(&value));
    assert!(issues.is_empty(), "config issues: {issues:?}");
    let (manager, issues) = McpManager::start(&servers, tmp.path()).await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let policy = manager
        .signal_config("docs")
        .expect("a connected server has a policy");
    assert_eq!(policy.wake, kiqi_mcp::WakeMode::Auto);
    assert!(manager.signal_config("unknown").is_none());
    manager.shutdown().await;
}

/// Mark-before-RPC: the fake server pushes the update BEFORE replying to the
/// subscribe, so the only way the notice can exist is a lifetime installed
/// ahead of the RPC.
#[tokio::test(flavor = "multi_thread")]
async fn update_pushed_during_the_subscribe_handshake_is_accepted() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[stdio_server_with(
            "docs",
            ["--resources".to_string(), "--subscribe-notify".to_string()].to_vec(),
            10,
            true,
            &["mem://alpha"],
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    // The notice arrives on rmcp's dispatch task, which may still be draining
    // when start() returns — wait for it, bounded.
    let batch = wait_for(5, "the racing first update", || {
        let batch = manager.take_notices();
        (!batch.is_empty()).then_some(batch)
    });
    assert_eq!(
        batch.notices,
        vec![kiqi_mcp::McpNotice {
            server: "docs".into(),
            uri: "mem://alpha".into(),
            content: None,
        }],
        "the racing first update was dropped — the mark must precede the RPC"
    );
    manager.shutdown().await;
}

/// The mailbox end to end: three updates for one uri land as ONE coalesced
/// notice whose suppressed counter reports the two folded events.
#[tokio::test(flavor = "multi_thread")]
async fn notifications_coalesce_and_count_suppressed_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[stdio_server_with(
            "docs",
            ["--resources".to_string(), "--subscribe-notify".to_string()].to_vec(),
            10,
            true,
            &["mem://alpha"],
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    // Drain the handshake-race notice first — waiting for it, since dispatch
    // may still be in flight when start() returns.
    let handshake = wait_for(5, "the handshake notice", || {
        let batch = manager.take_notices();
        (!batch.is_empty()).then_some(batch)
    });
    assert_eq!(handshake.notices.len(), 1);

    let adapters = manager.tool_adapters();
    call(
        find(&adapters, "docs__poke"),
        json!({ "uri": "mem://alpha", "times": 3 }),
    )
    .await
    .unwrap();
    // rmcp dispatches every notification in its OWN spawned task (rmcp's
    // `spawn_service_task`), so the poke reply does NOT order the three
    // updates behind it — a fixed sleep here would be load-bearing and flake
    // under load. Poll until all three have landed instead: the first creates
    // the notice, the next two fold into it (suppressed reaches 2). Each take
    // resets the mailbox, so accumulate across polls.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut notices: Vec<kiqi_mcp::McpNotice> = Vec::new();
    let mut suppressed = 0u64;
    while suppressed < 2 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the three updates (suppressed={suppressed})"
        );
        std::thread::sleep(Duration::from_millis(25));
        let batch = manager.take_notices();
        for notice in batch.notices {
            if !notices.contains(&notice) {
                notices.push(notice);
            }
        }
        suppressed += batch.suppressed;
    }
    assert_eq!(notices.len(), 1, "updates must coalesce: {notices:?}");
    assert_eq!(notices[0].server, "docs");
    assert_eq!(notices[0].uri, "mem://alpha");
    let batch = kiqi_mcp::McpNoticeBatch {
        notices,
        suppressed,
    };
    assert_eq!(batch.suppressed, 2, "two folded events must be counted");
    let text = kiqi_mcp::notice_entry_text(&batch);
    assert!(text.contains("- docs: mem://alpha"), "got {text:?}");
    assert!(text.contains("(2 update(s) suppressed)"), "got {text:?}");

    manager.shutdown().await;
}

/// Rotation purge: a notice queued under the CURRENT lifetime is destroyed by
/// the very re-subscribe that rotates it. (Removing the purge from
/// `ServerHandle::rotate_lifetime` fails this test — the purge is the only
/// observable effect of the rotation on ki's side.)
#[tokio::test(flavor = "multi_thread")]
async fn resubscribe_purges_the_queued_stale_notice() {
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[stdio_server_with(
            "docs",
            ["--resources".to_string(), "--subscribe-notify".to_string()].to_vec(),
            10,
            true,
            &["mem://alpha"],
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    // Drain the handshake-race notice first (waiting for it, since dispatch
    // may still be in flight when start() returns).
    let _ = wait_for(5, "the handshake notice", || {
        let batch = manager.take_notices();
        (!batch.is_empty()).then_some(batch)
    });
    // Queue a fresh notice and CONFIRM it is pending — peek, not take: taking
    // it would empty the very queue the rotation is supposed to purge.
    let adapters = manager.tool_adapters();
    call(
        find(&adapters, "docs__poke"),
        json!({ "uri": "mem://alpha" }),
    )
    .await
    .unwrap();
    wait_for(5, "the poked notice to queue", || {
        (manager.pending_notice_count() >= 1).then_some(())
    });
    // Re-subscribe the SAME uri: already-subscribed, so no second RPC goes
    // out — but the delivery lifetime rotates and the uri's queue purges.
    manager
        .subscribe("docs", "mem://alpha")
        .await
        .expect("re-subscribe succeeds");
    let batch = manager.take_notices();
    assert!(
        batch.is_empty(),
        "the pre-rotation notice must be purged, not delivered: {batch:?}"
    );
    assert_eq!(
        manager.pending_notice_count(),
        0,
        "the mailbox must be empty after the rotation purge"
    );

    manager.shutdown().await;
}

/// Invalidate-then-send: local delivery stops at once even though the fake
/// server REJECTS the unsubscribe — and the retired transport reconnects,
/// restoring the surviving subscription exactly once, leaving the rejected uri
/// freely re-subscribable.
#[tokio::test(flavor = "multi_thread")]
async fn rejected_unsubscribe_stops_delivery_but_stays_resubscribable() {
    let tmp = tempfile::tempdir().unwrap();
    let record = tmp.path().join("record.log");
    let (manager, issues) = McpManager::start(
        &[stdio_server_with(
            "docs",
            [
                "--resources".to_string(),
                "--reject-unsubscribe".to_string(),
                "--record".to_string(),
                record.to_string_lossy().into_owned(),
            ]
            .to_vec(),
            10,
            true,
            &["mem://alpha", "mem://beta"],
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();

    // Unsubscribe beta: always reported unsubscribed, delivery stops even
    // though the server rejects the RPC (which retires the transport).
    manager
        .unsubscribe("docs", "mem://beta")
        .await
        .expect("unsubscribe reports unsubscribed");
    // Any in-flight beta update from before (or from the still-registered
    // remote side) must be dropped, never delivered.
    call(
        find(&adapters, "docs__poke"),
        json!({ "uri": "mem://beta" }),
    )
    .await
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let batch = manager.take_notices();
    assert!(
        !batch.notices.iter().any(|n| n.uri == "mem://beta"),
        "delivery must stop after unsubscribe: {batch:?}"
    );

    // The poke call above reconnected (the rejected unsubscribe retired the
    // transport) and restored the SURVIVING subscription exactly once.
    assert_eq!(
        count_recorded(&record, "resources/subscribe", "mem://alpha"),
        2,
        "restore must send exactly one subscribe for alpha"
    );
    assert_eq!(
        count_recorded(&record, "resources/subscribe", "mem://beta"),
        1,
        "the unsubscribed uri must not be restored"
    );
    assert_eq!(
        count_recorded(&record, "resources/unsubscribe", "mem://beta"),
        1
    );

    // Alpha still delivers; beta is freely re-subscribable.
    call(
        find(&adapters, "docs__poke"),
        json!({ "uri": "mem://alpha" }),
    )
    .await
    .unwrap();
    wait_for(5, "the alpha notice", || {
        let batch = manager.take_notices();
        let hit = batch.notices.iter().any(|n| n.uri == "mem://alpha");
        hit.then_some(batch)
    });
    manager
        .subscribe("docs", "mem://beta")
        .await
        .expect("beta must be re-subscribable");
    assert_eq!(
        count_recorded(&record, "resources/subscribe", "mem://beta"),
        2
    );

    manager.shutdown().await;
}

/// The retired adapter's recovery semantic: a dead connection never loses a
/// subscription — the fresh transport restores it (sorted, once per uri), and
/// updates flow again.
#[tokio::test(flavor = "multi_thread")]
async fn reconnect_restores_subscriptions_and_delivery_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile: PathBuf = tmp.path().join("server.pids");
    let record = tmp.path().join("record.log");
    let (manager, issues) = McpManager::start(
        &[stdio_server_with(
            "docs",
            [
                "--resources".to_string(),
                "--pidfile".to_string(),
                pidfile.to_string_lossy().into_owned(),
                "--record".to_string(),
                record.to_string_lossy().into_owned(),
            ]
            .to_vec(),
            10,
            true,
            &["mem://alpha", "mem://beta"],
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();
    call(find(&adapters, "docs__echo"), json!({ "text": "up" }))
        .await
        .unwrap();

    // Kill the server out from under ki, then let the next call reconnect.
    let old_pid = read_pidfile(&pidfile)
        .into_iter()
        .find(|(l, _)| l == "SELF")
        .unwrap()
        .1;
    let _ = std::process::Command::new("kill")
        .arg(old_pid.to_string())
        .status();
    assert_process_gone(old_pid, "killed server process");
    call(find(&adapters, "docs__echo"), json!({ "text": "back" }))
        .await
        .unwrap();

    // Exactly one restore RPC per subscribed uri on the fresh connection.
    assert_eq!(
        count_recorded(&record, "resources/subscribe", "mem://alpha"),
        2
    );
    assert_eq!(
        count_recorded(&record, "resources/subscribe", "mem://beta"),
        2
    );

    // Delivery works across the reconnect.
    call(
        find(&adapters, "docs__poke"),
        json!({ "uri": "mem://alpha" }),
    )
    .await
    .unwrap();
    let batch = wait_for(5, "the alpha notice after reconnect", || {
        let batch = manager.take_notices();
        let hit = batch.notices.iter().any(|n| n.uri == "mem://alpha");
        hit.then_some(batch)
    });
    assert_eq!(batch.notices[0].server, "docs");

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribed_resources_shutdown_still_reaps_the_whole_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile: PathBuf = tmp.path().join("server.pids");
    let (manager, issues) = McpManager::start(
        &[stdio_server_with(
            "treed",
            [
                "--resources".to_string(),
                "--child".to_string(),
                "--pidfile".to_string(),
                pidfile.to_string_lossy().into_owned(),
            ]
            .to_vec(),
            10,
            true,
            &["mem://alpha"],
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();
    call(find(&adapters, "treed__echo"), json!({ "text": "up" }))
        .await
        .unwrap();

    let pids = read_pidfile(&pidfile);
    let self_pid = pids.iter().find(|(l, _)| l == "SELF").unwrap().1;
    let child_pid = pids.iter().find(|(l, _)| l == "CHILD").unwrap().1;
    assert!(process_alive(self_pid), "server should be running");
    assert!(process_alive(child_pid), "grandchild should be running");

    manager.shutdown().await;
    assert_process_gone(self_pid, "server process");
    assert_process_gone(child_pid, "server grandchild");
}
