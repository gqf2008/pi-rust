#![forbid(unsafe_code)]

pub mod kiqi;

use std::process::{Command, Stdio};
use std::time::Duration;

mod kit;

use crate::kit::optional_string;
use serde_json::Value;
use wait_timeout::ChildExt;

pub(crate) fn code_lens(params: &Value) -> Result<String, String> {
    let check = optional_string(params, "check", "diff");
    let (program, args): (&str, &[&str]) = match check.as_str() {
        "diff" => ("git", &["diff", "--check"]),
        "cargo" => ("cargo", &["check"]),
        "fmt" => ("cargo", &["fmt", "--", "--check"]),
        "clippy" => ("cargo", &["clippy", "--", "-D", "warnings"]),
        _ => return Err("check must be one of: diff, cargo, fmt, clippy".into()),
    };
    let timeout = params
        .get("timeoutSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(1, 120);
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("unable to run {program}: {err}"))?;
    // Drain both pipes on their own threads before waiting. A check that writes
    // more than the pipe buffer (64 KiB) otherwise blocks inside `write` while
    // this thread sits in `wait_timeout`: the deadline fires, the child is
    // killed, and its report is reported as a timeout instead of returned.
    let stdout_reader = child.stdout.take().map(spawn_reader);
    let stderr_reader = child.stderr.take().map(spawn_reader);
    if child
        .wait_timeout(Duration::from_secs(timeout))
        .map_err(|err| format!("failed while waiting for {check}: {err}"))?
        .is_none()
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{check} timed out after {timeout}s"));
    }
    let status = child
        .wait()
        .map_err(|err| format!("failed to collect {check} status: {err}"))?;
    let stdout = join_reader(stdout_reader);
    let stderr = join_reader(stderr_reader);
    let mut text = stdout;
    text.push_str(&stderr);
    truncate_output(&mut text);
    Ok(format!(
        "check={check} exit={}\n{}",
        status.code().unwrap_or(-1),
        text.trim()
    ))
}

/// Read one of the child's pipes to EOF on its own thread.
fn spawn_reader<R: std::io::Read + Send + 'static>(
    mut reader: R,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        String::from_utf8_lossy(&buffer).into_owned()
    })
}

fn join_reader(reader: Option<std::thread::JoinHandle<String>>) -> String {
    reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

/// Cap combined output at 40_000 bytes, truncating on a char boundary:
/// `String::truncate` panics when the byte offset lands mid-codepoint, so a
/// large enough clippy report with non-ASCII (e.g. a quoted identifier in
/// CJK) would take the tool down with a panic instead of returning.
fn truncate_output(text: &mut String) {
    if text.len() > 40_000 {
        let mut end = 40_000;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[truncated]");
    }
}

export_single_tool_plugin!(
    code_lens,
    "code_lens",
    "Run an allowlisted, timeout-bounded local code diagnostic.",
    r#"{"type":"object","properties":{"check":{"type":"string","enum":["diff","cargo","fmt","clippy"]},"cwd":{"type":"string"},"timeoutSeconds":{"type":"integer","minimum":1,"maximum":120}}}"#
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_check() {
        let value = serde_json::json!({"check":"shell"});
        assert!(code_lens(&value).unwrap_err().contains("diff"));
    }

    /// Regression: the 40_000-byte cap must truncate on a char boundary. With
    /// a multi-byte codepoint straddling the cut, `String::truncate(40_000)`
    /// panics — this would have taken the tool down on a large non-ASCII
    /// report.
    #[test]
    fn truncation_does_not_panic_mid_codepoint() {
        // '中' is 3 bytes; put its start one byte before the cut so a naive
        // truncate at 40_000 lands inside it.
        let mut text = "a".repeat(39_999);
        text.push_str("中文 tail");
        truncate_output(&mut text);
        assert!(text.ends_with("\n[truncated]"));
        // The cut kept whole codepoints only: the body before the marker is
        // at most the cap.
        assert!(text.len() <= 40_000 + "\n[truncated]".len());
    }

    #[test]
    fn truncation_leaves_short_output_alone() {
        let mut text = "short output".to_string();
        truncate_output(&mut text);
        assert_eq!(text, "short output");
    }
}
