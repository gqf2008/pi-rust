//! Env-gated audit trace for soft interrupts.
//!
//! A soft interrupt is mostly invisible: the run keeps going, the status stays
//! `Working`, and the only evidence is one dim note. When a user reports "Esc
//! did nothing" the interesting question is *where the press landed* — on a tool
//! call, on a provider request, or in the gap between the two (where the flag is
//! latched instead). That is exactly the state this module records, so the
//! diagnosis does not depend on guesswork or on a debugger attached to a TUI.
//!
//! Enable with `KI_DEBUG_INTERRUPT=1` (appends to `<temp>/kiqi-interrupt.log`)
//! or `KI_DEBUG_INTERRUPT=<path>` to pick the file. One line per event:
//!
//! ```text
//! 2026-10-02T09:15:01.234Z press installed=true kind=Tool
//! 2026-10-02T09:15:04.001Z press installed=false kind=none
//! 2026-10-02T09:15:04.250Z latched-cancelled kind=Stream
//! 2026-10-02T09:15:07.900Z decision=soft kind=Tool
//! 2026-10-02T09:15:09.100Z decision=no-run kind=none
//! ```
//!
//! Deliberately **not** a metrics pipeline: nothing here influences a return
//! value, an ordering, or a timing decision. Every IO error is swallowed (a
//! diagnostic that can fail a run is worse than no diagnostic), and when the env
//! var is unset the whole module costs one `OnceLock` lookup per event.
//!
//! ## Why every call site traces *after* releasing its lock
//!
//! The three events are emitted from `InterruptControl::soft_interrupt`,
//! `InterruptControl::install` and the harness `interrupt()`. The first two hold
//! the interrupt control's own `current` mutex while they read/cancel the slot.
//! Appending a line under that mutex would put a filesystem write on the hot path
//! of *every* action install of *every* run (the loop installs one slot per tool
//! call and per stream request), and a slow or networked log path would then turn
//! into a global stall of the agent loop — a debug switch that changes behaviour.
//! So each call site copies the values it needs (`kind`, `installed`) into locals,
//! drops the guard, and only then calls into this module.

use std::io::Write;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::interrupt::ActionKind;

/// Resolved trace destination, or `None` when tracing is off.
fn target() -> Option<&'static std::path::PathBuf> {
    static TARGET: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    TARGET
        .get_or_init(|| {
            let value = std::env::var("KI_DEBUG_INTERRUPT").ok()?;
            let value = value.trim();
            if value.is_empty() || value == "0" {
                return None;
            }
            if value == "1" || value.eq_ignore_ascii_case("true") {
                Some(std::env::temp_dir().join("kiqi-interrupt.log"))
            } else {
                Some(std::path::PathBuf::from(value))
            }
        })
        .as_ref()
}

/// Whether interrupt tracing is enabled.
pub fn enabled() -> bool {
    target().is_some()
}

/// Path the trace is written to (for the startup banner / user hints).
pub fn path() -> Option<&'static std::path::Path> {
    target().map(std::path::PathBuf::as_path)
}

/// A soft press arrived. `kind` is the action it found installed, if any:
/// `installed=false` means the flag was latched for the next action instead.
pub fn press(kind: Option<ActionKind>) {
    let Some(path) = target() else {
        return;
    };
    append(
        path,
        &format!(
            "{} press installed={} kind={}\n",
            timestamp(),
            kind.is_some(),
            kind_label(kind)
        ),
    );
}

/// An action was installed while the soft flag was still latched, so its token
/// started out cancelled (the "pressed during a stream / between actions" case).
pub fn latched_cancelled(kind: ActionKind) {
    let Some(path) = target() else {
        return;
    };
    append(
        path,
        &format!(
            "{} latched-cancelled kind={}\n",
            timestamp(),
            kind_label(Some(kind))
        ),
    );
}

/// The harness verdict for one press: `soft`, `nothing` or `no-run`.
pub fn decision(decision: &str, kind: Option<ActionKind>) {
    let Some(path) = target() else {
        return;
    };
    append(
        path,
        &format!(
            "{} decision={decision} kind={}\n",
            timestamp(),
            kind_label(kind)
        ),
    );
}

fn kind_label(kind: Option<ActionKind>) -> &'static str {
    match kind {
        Some(ActionKind::Tool) => "Tool",
        Some(ActionKind::Stream) => "Stream",
        None => "none",
    }
}

fn append(path: &std::path::Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// UTC timestamp, `2026-09-24T12:00:01.234Z` (same format as `key_trace`).
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.as_millis();
    let secs = (millis / 1000) as i64;
    let sub = millis % 1000;
    let (year, month, day, hour, min, sec) = civil_from_unix(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{sub:03}Z")
}

/// Days-from-civil inverse (Howard Hinnant's algorithm) — avoids a date crate
/// dependency for a diagnostic timestamp.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m as u32,
        d as u32,
        (secs_of_day / 3600) as u32,
        ((secs_of_day % 3600) / 60) as u32,
        (secs_of_day % 60) as u32,
    )
}
