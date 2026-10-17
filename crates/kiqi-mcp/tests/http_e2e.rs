//! Offline end-to-end run of the host-level MCP HTTP/Streamable transport.
//!
//! Every test starts an in-process HTTP peer on loopback (`support/http_fake.rs`)
//! and drives the manager's public API. No external server, no network, no
//! tempdir fixtures: session loss, SSE notices and DELETE are all observable
//! through the fake's recorded state.

#[path = "support/http_fake.rs"]
mod http_fake;

use std::sync::Arc;
use std::time::{Duration, Instant};

use kiqi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use kiqi_agent::{AgentError, AgentTool};
use kiqi_mcp::{parse_servers, McpManager, McpNoticeBatch, ServerConfig};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use http_fake::{Behavior, FakeHttpServer};

const FAKE_SERVER: &str = env!("CARGO_BIN_EXE_ki-mcp-fake-server");

/// A stdio entry through the public parser (the mixed-transport test).
fn stdio_server(name: &str) -> ServerConfig {
    let value = json!({
        name: { "command": FAKE_SERVER, "timeoutSeconds": 10 }
    });
    let (servers, issues) = parse_servers(Some(&value));
    assert!(issues.is_empty(), "config issues: {issues:?}");
    servers.into_iter().next().expect("exactly one server")
}

fn http_server(
    name: &str,
    url: &str,
    timeout_secs: u64,
    subscribe: &[&str],
    auto_read: &[&str],
    headers: Option<Value>,
) -> ServerConfig {
    let mut entry = json!({
        "url": url,
        "timeoutSeconds": timeout_secs,
        "subscribe": subscribe,
        "autoRead": auto_read,
    });
    if let Some(headers) = headers {
        entry["headers"] = headers;
    }
    let value = json!({ name: entry });
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

/// Poll the mailbox (without draining) until an update lands, then drain with
/// the `autoRead` opt-in applied. Notifications arrive on rmcp's dispatch task,
/// so every assertion here is a bounded wait, never an immediate read.
async fn wait_for_notices(manager: &McpManager, expected: usize, seconds: u64) -> McpNoticeBatch {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if manager.pending_notice_count() >= expected {
            return manager.take_notices_with_auto_read().await;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} HTTP SSE notice(s); got {}",
            manager.pending_notice_count()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn http_initializes_lists_tools_calls_and_deletes_the_session() {
    let server = FakeHttpServer::start(Behavior::default()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server(
            "web",
            &server.url,
            10,
            &[],
            &[],
            Some(json!({ "Authorization": "Bearer t0k", "X-Test": "yes" })),
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    let adapters = manager.tool_adapters();
    let names: Vec<_> = adapters.iter().map(|a| a.schema().name.clone()).collect();
    assert!(names.contains(&"web__echo".to_string()), "{names:?}");
    // The server advertises resources, so the read tool exists too.
    assert!(
        names.contains(&"web__read_resource".to_string()),
        "{names:?}"
    );
    assert_eq!(server.initialize_count(), 1);
    assert!(server.saw_method("resources/list"));

    // Config headers pass through; the conventional Authorization header
    // becomes rmcp's bearer token (the wire still says "Bearer t0k").
    assert!(server.posted_with_header("authorization", "Bearer t0k"));
    assert!(server.posted_with_header("x-test", "yes"));

    let echoed = call(find(&adapters, "web__echo"), json!({ "text": "hi" }))
        .await
        .unwrap();
    assert_eq!(text_of(&echoed), "echo: hi");

    manager.shutdown().await;
    assert!(
        server.session_deleted(),
        "shutdown must DELETE the MCP session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_post_replies_are_parsed_like_json_replies() {
    let server = FakeHttpServer::start(Behavior {
        sse_post_replies: true,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server("web", &server.url, 10, &[], &[], None)],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    let adapters = manager.tool_adapters();
    let echoed = call(find(&adapters, "web__echo"), json!({ "text": "sse" }))
        .await
        .unwrap();
    assert_eq!(text_of(&echoed), "echo: sse");

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_session_reinitializes_once_replays_and_restores_subscriptions() {
    let server = FakeHttpServer::start(Behavior::default()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server(
            "web",
            &server.url,
            10,
            &["mem://doc"],
            &[],
            None,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    assert_eq!(server.method_count("resources/subscribe"), 1);
    let adapters = manager.tool_adapters();

    let first = call(find(&adapters, "web__echo"), json!({ "text": "one" }))
        .await
        .unwrap();
    assert_eq!(text_of(&first), "echo: one");
    assert_eq!(server.initialize_count(), 1);

    // The server forgets the session: the next request gets HTTP 404, which
    // rmcp turns into one transparent re-init + replay.
    server.expire_session();
    let second = call(find(&adapters, "web__echo"), json!({ "text": "two" }))
        .await
        .unwrap();
    assert_eq!(text_of(&second), "echo: two");
    assert_eq!(
        server.initialize_count(),
        2,
        "the 404 must trigger exactly one re-initialization"
    );

    // MCP subscriptions are per session, so the new session has none. The
    // next use of the connection must restore them (rather than leaving the
    // map looking subscribed forever).
    let third = call(find(&adapters, "web__echo"), json!({ "text": "three" }))
        .await
        .unwrap();
    assert_eq!(text_of(&third), "echo: three");
    assert_eq!(
        server.method_count("resources/subscribe"),
        2,
        "the re-initialized session must be re-subscribed exactly once"
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn standalone_sse_delivers_notices_and_autoread_appends_content() {
    let server = FakeHttpServer::start(Behavior {
        notify_on_subscribe: true,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server(
            "web",
            &server.url,
            10,
            &["mem://doc", "mem://other"],
            // Only one uri opts into eager reads; the other stays a light notice.
            &["mem://doc"],
            None,
        )],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    assert!(server.subscribed("mem://doc"));
    assert!(server.subscribed("mem://other"));

    let batch = wait_for_notices(&manager, 2, 5).await;
    assert_eq!(batch.notices.len(), 2, "got {batch:?}");
    let doc = batch
        .notices
        .iter()
        .find(|notice| notice.uri == "mem://doc")
        .expect("the doc notice");
    assert_eq!(
        doc.content.as_deref(),
        Some("fresh content"),
        "autoRead must read through the HTTP connection"
    );
    let other = batch
        .notices
        .iter()
        .find(|notice| notice.uri == "mem://other")
        .expect("the other notice");
    assert_eq!(other.content, None, "unlisted uris keep the light notice");

    let text = kiqi_mcp::notice_entry_text(&batch);
    assert!(
        text.contains("- web: mem://doc\n  fresh content"),
        "{text:?}"
    );
    assert!(text.contains("- web: mem://other"), "{text:?}");

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_response_over_the_16_mib_cap_is_rejected() {
    let server = FakeHttpServer::start(Behavior {
        huge_response: true,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server("web", &server.url, 10, &[], &[], None)],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();

    let error = call(find(&adapters, "web__big"), json!({}))
        .await
        .expect_err("an oversized HTTP response must fail the call");
    let message = error.to_string();
    assert!(message.contains("cap"), "got {message}");
    assert!(message.contains("16777216"), "got {message}");

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cross_origin_redirect_is_refused_and_never_contacted() {
    let target = FakeHttpServer::start(Behavior::default()).await;
    let source = FakeHttpServer::start(Behavior::default()).await;
    source.redirect_to(&target.url);

    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server("web", &source.url, 10, &[], &[], None)],
        tmp.path(),
    )
    .await;

    assert!(
        !issues.is_empty(),
        "a stopped cross-origin redirect must fail the server start"
    );
    assert!(
        issues.iter().any(|issue| issue.contains("302")),
        "the failure must surface the unfollowed redirect: {issues:?}"
    );
    assert_eq!(
        target.request_count(),
        0,
        "ki must never contact the redirect target"
    );
    assert_eq!(target.initialize_count(), 0);
    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chunked_response_over_the_cap_is_rejected() {
    // No Content-Length: only the streaming accumulation guard can catch this.
    let server = FakeHttpServer::start(Behavior {
        huge_response_chunked: true,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server("web", &server.url, 10, &[], &[], None)],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();

    let error = call(find(&adapters, "web__big"), json!({}))
        .await
        .expect_err("a chunked response past the cap must fail the call");
    let message = error.to_string();
    assert!(message.contains("cap"), "got {message}");
    assert!(message.contains("16777216"), "got {message}");

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_post_sse_reply_over_the_cap_fails_the_call() {
    let server = FakeHttpServer::start(Behavior {
        huge_sse_response: true,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    // The over-cap stream is killed but the request has no reply; the call
    // must fail at the configured timeout, not succeed or hang forever.
    let (manager, issues) = McpManager::start(
        &[http_server("web", &server.url, 2, &[], &[], None)],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();

    let started = Instant::now();
    let error = call(find(&adapters, "web__big"), json!({}))
        .await
        .expect_err("an over-cap SSE reply must fail the call");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the call must fail at its timeout, took {:?}: {error}",
        started.elapsed()
    );

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_http_servers_keep_notices_and_auto_reads_per_server() {
    let first = FakeHttpServer::start(Behavior {
        notify_on_subscribe: true,
        ..Default::default()
    })
    .await;
    let second = FakeHttpServer::start(Behavior {
        notify_on_subscribe: true,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[
            http_server("one", &first.url, 10, &["mem://doc"], &["mem://doc"], None),
            http_server("two", &second.url, 10, &["mem://doc"], &["mem://doc"], None),
        ],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    let batch = wait_for_notices(&manager, 2, 5).await;
    assert_eq!(batch.notices.len(), 2, "got {batch:?}");
    for server in ["one", "two"] {
        let notice = batch
            .notices
            .iter()
            .find(|notice| notice.server == server)
            .unwrap_or_else(|| panic!("no notice for server {server}: {batch:?}"));
        assert_eq!(notice.uri, "mem://doc");
        assert_eq!(
            notice.content.as_deref(),
            Some("fresh content"),
            "autoRead for {server} must read through that server's connection"
        );
    }

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_post_does_not_wedge_the_worker_for_later_calls() {
    let server = FakeHttpServer::start(Behavior {
        hang_tools_calls: 1,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server("web", &server.url, 1, &[], &[], None)],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();

    // The first call observes the per-request timeout...
    let error = call(find(&adapters, "web__echo"), json!({ "text": "first" }))
        .await
        .expect_err("a hung request must time out");
    assert!(error.to_string().contains("timeout"), "got {error}");

    // ...and the worker must be free again for the next one. Without a wire
    // timeout the worker is still blocked on the first POST and this call
    // times out too.
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        call(find(&adapters, "web__echo"), json!({ "text": "second" })),
    )
    .await
    .expect("the worker stayed wedged on the hung POST")
    .expect("the second call must reach the server");
    assert_eq!(text_of(&second), "echo: second");

    manager.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_while_a_http_request_is_in_flight_returns() {
    let server = FakeHttpServer::start(Behavior {
        hang_tools_calls: usize::MAX,
        ..Default::default()
    })
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server("web", &server.url, 60, &[], &[], None)],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");
    let adapters = manager.tool_adapters();
    let tool = Arc::clone(find(&adapters, "web__echo"));

    let in_flight = tokio::spawn(async move { call(&tool, json!({ "text": "hang" })).await });
    // Wait until the fake actually received the tool call.
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.method_count("tools/call") == 0 {
        assert!(
            Instant::now() < deadline,
            "the tool call never reached the server"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    manager.shutdown().await;
    let result = tokio::time::timeout(Duration::from_secs(20), in_flight)
        .await
        .expect("the in-flight call must not outlive shutdown")
        .expect("the call task must not panic");
    assert!(
        result.is_err(),
        "a call interrupted by shutdown must fail, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_and_http_servers_coexist_in_one_manager() {
    let http = FakeHttpServer::start(Behavior::default()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[
            stdio_server("local"),
            http_server("web", &http.url, 10, &[], &[], None),
        ],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    let adapters = manager.tool_adapters();
    let names: Vec<_> = adapters.iter().map(|a| a.schema().name.clone()).collect();
    assert!(names.contains(&"local__echo".to_string()), "{names:?}");
    assert!(names.contains(&"web__echo".to_string()), "{names:?}");

    let local = call(find(&adapters, "local__echo"), json!({ "text": "l" }))
        .await
        .unwrap();
    assert_eq!(text_of(&local), "l");
    let web = call(find(&adapters, "web__echo"), json!({ "text": "w" }))
        .await
        .unwrap();
    assert_eq!(text_of(&web), "echo: w");

    manager.shutdown().await;
    assert!(
        http.session_deleted(),
        "shutdown must still DELETE the HTTP session with a stdio peer present"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fake_refuses_a_get_or_delete_without_the_session_id() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let server = FakeHttpServer::start(Behavior::default()).await;
    let tmp = tempfile::tempdir().unwrap();
    let (manager, issues) = McpManager::start(
        &[http_server("web", &server.url, 10, &[], &[], None)],
        tmp.path(),
    )
    .await;
    assert!(issues.is_empty(), "start issues: {issues:?}");

    let addr = server
        .url
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_suffix("/mcp"))
        .expect("loopback fake url")
        .to_string();

    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(b"GET /mcp HTTP/1.1\r\nHost: fake\r\n\r\n")
        .await
        .unwrap();
    let mut response = [0u8; 64];
    let read = stream.read(&mut response).await.unwrap();
    assert!(
        String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 400"),
        "a GET without the session id must be refused"
    );

    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(b"DELETE /mcp HTTP/1.1\r\nHost: fake\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    let mut response = [0u8; 64];
    let read = stream.read(&mut response).await.unwrap();
    assert!(
        String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 404"),
        "a DELETE without the session id must be refused"
    );
    assert!(
        !server.session_deleted(),
        "a DELETE without the session id must not delete the session"
    );

    manager.shutdown().await;
    assert!(
        server.session_deleted(),
        "the manager's own DELETE carries the session id"
    );
}
