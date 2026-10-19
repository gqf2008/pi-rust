//! Soft-interrupt audit trace: the `InterruptControl` side.
//!
//! This is a *separate integration-test binary* on purpose. `KI_DEBUG_INTERRUPT`
//! is process-global and resolved once into a `OnceLock`, so a test that wants a
//! private trace file must own the whole process; a `#[test]` living next to
//! other tests that press interrupts would race for that first resolution.
//!
//! What is pinned here is the pair of events only the control can emit:
//! `press installed=<bool> kind=<..>` and `latched-cancelled kind=<..>`. The
//! harness verdict (`decision=…`) is pinned in
//! `crates/kiqi-harness/tests/interrupt_trace_e2e.rs`, which drives a real run.

use kiqi_agent::interrupt::{ActionKind, InterruptControl};
use tokio_util::sync::CancellationToken;

/// `2026-10-02T09:15:01.234Z` — the same shape `key_trace` uses.
fn has_diagnostic_timestamp(line: &str) -> bool {
    let Some((stamp, _)) = line.split_once(' ') else {
        return false;
    };
    let bytes = stamp.as_bytes();
    stamp.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && stamp.ends_with('Z')
}

#[test]
fn trace_distinguishes_a_press_from_a_latched_install() {
    let path = std::env::temp_dir().join(format!(
        "kiqi-interrupt-trace-agent-{}.log",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    std::env::set_var("KI_DEBUG_INTERRUPT", &path);
    assert!(
        kiqi_agent::interrupt_trace::enabled(),
        "KI_DEBUG_INTERRUPT must switch the trace on"
    );

    let ctl = InterruptControl::new();

    // 1. A press with nothing installed latches for the next action.
    ctl.soft_interrupt();
    // 2. That next action starts cancelled — the `latched-cancelled` event.
    let stream = CancellationToken::new();
    let id = ctl.install(stream.clone(), ActionKind::Stream);
    assert!(stream.is_cancelled(), "the latch cancels the next action");
    assert!(ctl.take_soft(), "the loop drains the latch at the boundary");
    ctl.clear(id);
    // 3. A press with a tool call installed names the tool.
    let tool = CancellationToken::new();
    let id = ctl.install(tool.clone(), ActionKind::Tool);
    ctl.soft_interrupt();
    assert!(
        tool.is_cancelled(),
        "the press cancelled the installed tool"
    );
    assert!(ctl.take_soft());
    ctl.clear(id);

    let log = std::fs::read_to_string(&path).expect("the trace file was written");
    let _ = std::fs::remove_file(&path);
    let lines: Vec<&str> = log.lines().collect();

    for line in &lines {
        assert!(
            has_diagnostic_timestamp(line),
            "every line starts with a UTC timestamp: {line:?}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|l| l.contains("press installed=false kind=none")),
        "a press with an empty slot must say so: {log}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("latched-cancelled kind=Stream")),
        "the pre-cancelled install must record the action it started: {log}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("press installed=true kind=Tool")),
        "a press on a tool call must name the tool: {log}"
    );
    // Exactly two presses happened; a third line would mean an event is emitted
    // from somewhere it should not be (e.g. a retry inside `install`).
    assert_eq!(
        lines.iter().filter(|l| l.contains("press ")).count(),
        2,
        "one line per press: {log}"
    );
}
