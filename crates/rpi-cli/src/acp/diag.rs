//! stderr diagnostics for `ki acp serve`.
//!
//! stdout carries only the ACP protocol (the stdio transport is the wire), so
//! every diagnostic MUST go to stderr — the client treats stdout as the
//! JSON-RPC channel and logs the agent's stderr. `diagln!` is gated behind
//! `KI_ACP_DEBUG`; the shutdown line is unconditional (Zed surfaces it as the
//! agent's last words, the same discipline as rpi-acp's `run()` tail line).

/// Whether method-flow tracing is on (`KI_ACP_DEBUG` set to anything but
/// "" / "0").
pub(crate) fn enabled() -> bool {
    match std::env::var("KI_ACP_DEBUG") {
        Ok(value) => !value.is_empty() && value != "0",
        Err(_) => false,
    }
}

/// The unconditional one-line shutdown record: why the server stopped and with
/// which exit code.
pub(crate) fn shutdown(reason: &str, code: i32) {
    eprintln!("[ki acp] server stopping: {reason} (exit {code})");
}

/// Method-flow tracing to stderr; the gate is checked per call.
macro_rules! diagln {
    ($($arg:tt)*) => {
        if $crate::acp::diag::enabled() {
            eprintln!("[ki acp] {}", format_args!($($arg)*));
        }
    };
}

pub(crate) use diagln;
