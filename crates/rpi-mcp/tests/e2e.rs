//! Offline end-to-end run of the MCP manager against a real (fake) stdio server.
//!
//! Every test spawns `tests/support/fake_server.rs` as an actual child process
//! and drives ki's public API — no network, no external server. The liveness
//! assertions are OS-level (`kill -0`), which is the only way to prove the
//! process *tree* is gone and not merely unreferenced.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_agent::{AgentError, AgentTool};
use rpi_mcp::{parse_servers, McpManager, ServerConfig};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const FAKE_SERVER: &str = env!("CARGO_BIN_EXE_ki-mcp-fake-server");

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Build one stdio server config through the public parser, so the tests also
/// exercise the config path they depend on.
fn stdio_server(name: &str, args: Vec<String>, timeout_secs: u64, enabled: bool) -> ServerConfig {
    let value = json!({
        name: {
            "command": FAKE_SERVER,
            "args": args,
            "enabled": enabled,
            "timeoutSeconds": timeout_secs,
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
async fn http_server_is_parsed_but_skipped_in_this_build() {
    let tmp = tempfile::tempdir().unwrap();
    let value = json!({
        "remote": { "url": "https://example.test/mcp", "headers": { "Authorization": "x" } }
    });
    let (servers, issues) = parse_servers(Some(&value));
    assert!(issues.is_empty(), "config issues: {issues:?}");

    let (manager, issues) = McpManager::start(&servers, tmp.path()).await;
    assert_eq!(manager.tool_adapters().len(), 0);
    assert!(
        issues.iter().any(|issue| issue.contains("http transport")),
        "expected an http-skip notice, got {issues:?}"
    );
    manager.shutdown().await;
}
