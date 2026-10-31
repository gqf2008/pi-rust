//! ANSI escape code utilities and visible width calculations.
//!
//! Handles ANSI-aware string manipulation for terminal rendering.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Cursor position marker - APC sequence.
/// This is a zero-width escape sequence that terminals ignore.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Calculate the visible width of a string, ignoring ANSI escape sequences.
///
/// This is the number of terminal columns the string will occupy.
pub fn visible_width(s: &str) -> usize {
    let stripped = strip_ansi(s);
    UnicodeWidthStr::width(stripped.as_str())
}

/// Byte length of the escape sequence at the start of `s`, or 0 when `s` does
/// not start with `ESC`.
///
/// This is the single scanner shared by [`strip_ansi`] and
/// [`slice_by_column`]: both have to agree on where an escape sequence ends,
/// otherwise stripping and slicing drift apart and columns get miscounted.
fn escape_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    match chars.next() {
        // `ESC` is a single byte, so the next atom starts at offset 1 at worst.
        Some((_, '\x1b')) => {}
        _ => return 0,
    }

    match chars.next() {
        // CSI: skip until the final byte (0x40-0x7E).
        Some((_, '[')) => {
            let mut end = 2;
            for (index, c) in chars {
                end = index + c.len_utf8();
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
            end
        }
        // OSC / APC / DCS / PM: skip until BEL (0x07) or ST (ESC \). A *bare*
        // `ESC` ends the string as well — that is what terminals do (xterm
        // aborts an OSC at a stray `ESC`), and pretending otherwise would let
        // `ESC ]8;;x ESC [2J` pass as one "hyperlink" whose injected CSI is
        // then executed by the terminal.
        Some((_, ']' | '_' | 'P' | '^')) => {
            let mut end = 2;
            let mut chars = chars.peekable();
            while let Some((index, c)) = chars.next() {
                end = index + c.len_utf8();
                if c == '\x07' {
                    break;
                }
                if c == '\x1b' {
                    if matches!(chars.peek(), Some((_, '\\'))) {
                        let (index, c) = chars.next().expect("peeked char");
                        end = index + c.len_utf8();
                    }
                    break;
                }
            }
            end
        }
        // Character set designation: `ESC ( X` / `ESC ) X`.
        Some((_, '(' | ')')) => match chars.next() {
            Some((index, c)) => index + c.len_utf8(),
            None => 2,
        },
        // Other escape sequences: skip the following character.
        Some((index, c)) => index + c.len_utf8(),
        // Lone trailing `ESC`.
        None => 1,
    }
}

/// Whether `escape` may be written to the terminal from a rendered row.
///
/// Rows are built from strings ki does not own: tool output, model output,
/// extension messages, MCP payloads, file contents, pasted text. A rendered row
/// is handed to the terminal as input, so every escape it carries is executed.
/// Only the sequences ki itself produces may pass: SGR styling, OSC-8
/// hyperlinks, the inline image protocols, and ki's own APC markers. Everything
/// else — cursor movement, erasing, mode changes, OSC 52 clipboard writes,
/// window and title control, DCS — is dropped.
///
/// A sequence is allowed only when it is *terminated* within `escape` and its
/// payload holds neither a nested introducer nor a control character. Matching
/// the prefix alone is not enough: [`escape_len`] hands over everything up to
/// the string terminator, so a row ending in `ESC ]8;;x ESC [2J` (no `BEL`, no
/// `ST`) arrives as one "OSC 8" that merely *starts* like a hyperlink, and a
/// terminal aborts that OSC at the stray `ESC` and then executes the `[2J`.
fn row_escape_allowed(escape: &str) -> bool {
    // SGR is a CSI sequence terminated by `m`; only its parameters may sit
    // between, so private/mode forms like `CSI > 4 ; 2 m` stay out.
    if let Some(params) = escape
        .strip_prefix("\x1b[")
        .and_then(|rest| rest.strip_suffix('m'))
    {
        return params
            .chars()
            .all(|c| c.is_ascii_digit() || c == ';' || c == ':');
    }
    // Everything else is a string sequence, and must be terminated by BEL or ST.
    let Some(body) = escape
        .strip_suffix('\x07')
        .or_else(|| escape.strip_suffix("\x1b\\"))
    else {
        return false;
    };
    let known = body.starts_with("\x1b]8;")
        // iTerm2 inline images, and the OSC 133 prompt-zone markers the user
        // message bubble wraps itself in.
        || body.starts_with("\x1b]1337;File=")
        || body.starts_with("\x1b]133;")
        || body.starts_with("\x1b_pi:")
        || body.starts_with("\x1b_G");
    // `body[1..]` skips the leading `ESC` (which is a control character by
    // definition); anything past it that reintroduces an escape, or any other
    // control character, would be read by the terminal as a new sequence.
    known && !body[1..].contains('\x1b') && body[1..].chars().all(|c| !c.is_control())
}

/// Rewrite `row` so it can only draw glyphs and styling, or return `None` when
/// it already can.
///
/// Control characters other than the allowed escapes are dropped: a `\r`
/// returns the carriage and repaints over the columns to its left, a `\n` breaks
/// the one-row-per-line contract the renderer's cursor math depends on, and a
/// tab — which [`visible_width`] counts as zero columns — jumps to the
/// terminal's next tab stop and desynchronizes every column after it. Components
/// expand tabs to spaces before they measure a row (`markdown`, `diff`,
/// tool output); a row that skips that loses the tab instead of shifting the
/// columns after it. Text that is measured *after* this point — `ki -p` replies,
/// extension declarations — goes through [`sanitize_external_text`], which does
/// expand tabs.
///
/// A denied escape is dropped whole, including the single printable character an
/// `ESC X` / `ESC ( X` form swallows. That never widens a row, so the
/// measurement the row was fitted with stays valid; keeping those characters
/// would push the row past the width it was laid out for.
pub fn sanitize_row(row: &str) -> Option<String> {
    let mut clean: Option<String> = None;
    let mut offset = 0;

    while offset < row.len() {
        let tail = &row[offset..];
        if tail.starts_with('\x1b') {
            let end = escape_len(tail);
            let escape = &tail[..end];
            if row_escape_allowed(escape) {
                if let Some(clean) = clean.as_mut() {
                    clean.push_str(escape);
                }
            } else {
                clean.get_or_insert_with(|| row[..offset].to_string());
            }
            offset += end;
            continue;
        }

        let end = tail.find('\x1b').unwrap_or(tail.len());
        let text = &tail[..end];
        if text.chars().any(char::is_control) {
            clean
                .get_or_insert_with(|| row[..offset].to_string())
                .extend(text.chars().filter(|c| !c.is_control()));
        } else if let Some(clean) = clean.as_mut() {
            clean.push_str(text);
        }
        offset += end;
    }

    clean
}

/// [`sanitize_row`] for an owned row: the row is returned as is when it is
/// already safe, so ki's own rows are scanned but never copied.
pub fn sanitize_row_owned(row: String) -> String {
    match sanitize_row(&row) {
        None => row,
        Some(clean) => clean,
    }
}

/// Text ki does not own, made safe to measure and display.
///
/// Escape sequences are removed rather than filtered — text from outside ki has
/// no styling to keep — and tabs are expanded to spaces so that
/// [`visible_width`] and the terminal agree on the layout *before* the text is
/// wrapped, which is what keeps a tab in tool or model output from shifting
/// every column after it.
pub fn sanitize_external_text(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    for (index, line) in strip_ansi(text).split('\n').enumerate() {
        if index > 0 {
            clean.push('\n');
        }
        for c in line.chars() {
            match c {
                '\t' => clean.push_str("    "),
                c if c.is_control() => {}
                c => clean.push(c),
            }
        }
    }
    clean
}

/// Strip ANSI escape sequences from a string.
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut offset = 0;

    while offset < s.len() {
        let tail = &s[offset..];
        if tail.starts_with('\x1b') {
            // Escape sequences are zero-width, so they are dropped whole. That
            // includes `CURSOR_MARKER`: its APC payload must not be counted as
            // text, or the hardware cursor jumps several columns.
            offset += escape_len(tail);
        } else {
            let end = tail.find('\x1b').unwrap_or(tail.len());
            result.push_str(&tail[..end]);
            offset += end;
        }
    }

    result
}

/// One zero-width escape sequence or one visible grapheme of a line.
struct Atom<'a> {
    text: &'a str,
    width: usize,
    /// Escape sequences are zero-width *and* droppable; a zero-width grapheme
    /// cluster (e.g. a lone combining mark) is still text.
    escape: bool,
}

/// Split a line into escape sequences (width 0) and grapheme clusters (their
/// terminal width).
fn atoms(line: &str) -> Vec<Atom<'_>> {
    let mut atoms = Vec::new();
    let mut offset = 0;

    while offset < line.len() {
        let tail = &line[offset..];
        if tail.starts_with('\x1b') {
            let end = escape_len(tail);
            atoms.push(Atom {
                text: &tail[..end],
                width: 0,
                escape: true,
            });
            offset += end;
        } else {
            let end = tail.find('\x1b').unwrap_or(tail.len());
            for text in tail[..end].graphemes(true) {
                atoms.push(Atom {
                    text,
                    width: UnicodeWidthStr::width(text),
                    escape: false,
                });
            }
            offset += end;
        }
    }

    atoms
}

/// Whether `escape` is an SGR sequence (the only escapes carrying style).
fn is_sgr(escape: &str) -> bool {
    escape.starts_with("\x1b[") && escape.ends_with('m')
}

/// The parameter list of an SGR escape, or `None` for any other escape.
fn sgr_params(escape: &str) -> Option<&str> {
    if !is_sgr(escape) {
        return None;
    }
    Some(&escape[2..escape.len() - 1])
}

/// Whether these SGR parameters start by resetting everything (`ESC[m`,
/// `ESC[0m`, `ESC[0;31m`).
fn sgr_resets(params: &str) -> bool {
    matches!(params.split(';').next(), Some("" | "0"))
}

/// The payload of an OSC-8 hyperlink escape (`params;uri`), or `None` when the
/// escape is something else.
fn osc8_payload(escape: &str) -> Option<&str> {
    let rest = escape.strip_prefix("\x1b]8;")?;
    Some(
        rest.strip_suffix('\x07')
            .or_else(|| rest.strip_suffix("\x1b\\"))
            .unwrap_or(rest),
    )
}

/// The escape that closes an OSC-8 hyperlink.
const OSC8_CLOSE: &str = "\x1b]8;;\x07";

/// Fold one escape sequence into the slice's style state.
///
/// SGR sequences set or clear `active`; OSC-8 hyperlinks set or clear `link` (the
/// open sequence the slice has to replay, or the close it has to emit). Every
/// other escape — APC markers, character-set designations — carries no state.
fn fold_style(active: &mut bool, link: &mut String, escape: &str) {
    if let Some(payload) = osc8_payload(escape) {
        match payload.split_once(';') {
            // `ESC]8;;BEL` closes the link; a non-empty URI opens one.
            Some((_, uri)) if uri.is_empty() => link.clear(),
            _ => {
                link.clear();
                link.push_str(escape);
            }
        }
        return;
    }
    let Some(params) = sgr_params(escape) else {
        return;
    };
    // A leading `0` (or no parameter at all) resets, so only the parameters
    // after it decide whether a style stays on. This is what keeps the trailing
    // reset from being duplicated after an explicit `ESC[0m`.
    *active = if sgr_resets(params) {
        let mut rest = params.split(';');
        rest.next();
        rest.any(|part| part != "0")
    } else {
        true
    };
}

/// Tracks the style/hyperlink state of a slice as it is built.
#[derive(Default)]
struct SliceStyle {
    /// SGR bytes seen before the first emitted grapheme, replayed at the start.
    prefix: String,
    /// Whether a style is currently on (decides the trailing reset).
    active: bool,
    /// OSC-8 open sequence currently in effect, if any.
    link: String,
}

impl SliceStyle {
    fn fold(&mut self, escape: &str) {
        fold_style(&mut self.active, &mut self.link, escape);
        let Some(params) = sgr_params(escape) else {
            return;
        };
        // Canonicalise the replay: a reset drops everything accumulated before
        // it, so slicing a line that carries repeated resets produces the same
        // bytes as slicing one that does not (the old `accumulate_sgr` did the
        // same, and rendering asserts on those bytes).
        if sgr_resets(params) {
            self.prefix.clear();
        }
        self.prefix.push_str(escape);
    }

    /// Emit what the slice has to open with, then keep folding in place.
    fn open(&self, result: &mut String) {
        result.push_str(&self.prefix);
        result.push_str(&self.link);
    }

    /// Close whatever the slice left open so it cannot leak into the row.
    fn close(&self, result: &mut String) {
        if self.active {
            result.push_str("\x1b[0m");
        }
        if !self.link.is_empty() {
            result.push_str(OSC8_CLOSE);
        }
    }
}

/// Slice a string by visible column positions.
///
/// Returns the part of `s` that starts at `start_col` and spans at most `width`
/// visible columns. Escape sequences are treated as zero-width atoms: they never
/// count towards a column and never get cut in half.
///
/// With `preserve_styles` the slice is coloured: it opens with the SGR state
/// (and any open OSC-8 hyperlink) accumulated before `start_col` — reset by
/// `ESC[0m` / `ESC[m` — keeps the zero-width escapes that fall inside the range
/// (cursor markers, hyperlinks) and closes with `ESC[0m` / `ESC]8;;BEL` when a
/// style or link is still on, so nothing leaks into the rest of the row. Escapes
/// that *begin* after the range are dropped rather than emitted: a hyperlink
/// opened there would otherwise swallow the rows that follow. Without
/// `preserve_styles` every escape sequence is dropped and the caller gets plain
/// text.
///
/// Wide graphemes are never split in half: a grapheme straddling an edge is
/// replaced by spaces for the columns that overlap (so the result is exactly
/// `width` columns wide whenever the source is long enough). `width == 0`
/// returns an empty string and an out-of-range `start_col` returns an empty
/// string rather than panicking.
pub fn slice_by_column(s: &str, start_col: usize, width: usize, preserve_styles: bool) -> String {
    if width == 0 {
        return String::new();
    }

    let end = start_col.saturating_add(width);
    let atoms = atoms(s);
    let mut column = 0;
    let mut started = false;
    let mut result = String::new();
    let mut style = SliceStyle::default();

    for atom in &atoms {
        if atom.escape {
            if !preserve_styles {
                continue;
            }
            // Nothing past the window may be emitted, not even a zero-width
            // escape: the row has already been filled.
            if column >= end {
                break;
            }
            style.fold(atom.text);
            if started {
                result.push_str(atom.text);
            }
            continue;
        }

        if column >= end {
            break;
        }
        let next = column + atom.width;
        if next > start_col {
            if !started {
                started = true;
                if preserve_styles {
                    style.open(&mut result);
                }
            }
            if column >= start_col && next <= end {
                result.push_str(atom.text);
            } else {
                // Straddles the start or the end of the range: pad instead of
                // emitting half a grapheme.
                let overlap = next.min(end).saturating_sub(column.max(start_col));
                result.push_str(&" ".repeat(overlap));
            }
        }
        column = next;
    }

    if preserve_styles {
        style.close(&mut result);
    }

    result
}

/// Normalize terminal output by resetting styles at line ends.
pub fn normalize_terminal_output(line: &str) -> String {
    // Reset at end of line
    if line.contains("\x1b[") && !line.ends_with("\x1b[0m") {
        format!("{}\x1b[0m", line)
    } else {
        line.to_string()
    }
}

/// Create a hyperlink using OSC 8.
pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{}\x07{}\x1b]8;;\x07", url, text)
}

/// Apply a foreground color using ANSI 256-color mode.
pub fn fg_256(color: u8, text: &str) -> String {
    format!("\x1b[38;5;{}m{}\x1b[39m", color, text)
}

/// Apply a background color using ANSI 256-color mode.
pub fn bg_256(color: u8, text: &str) -> String {
    format!("\x1b[48;5;{}m{}\x1b[49m", color, text)
}

/// Apply a foreground color using RGB.
pub fn fg_rgb(r: u8, g: u8, b: u8, text: &str) -> String {
    format!("\x1b[38;2;{};{};{}m{}\x1b[39m", r, g, b, text)
}

/// Apply a background color using RGB.
pub fn bg_rgb(r: u8, g: u8, b: u8, text: &str) -> String {
    format!("\x1b[48;2;{};{};{}m{}\x1b[49m", r, g, b, text)
}

/// Apply bold style.
pub fn bold(text: &str) -> String {
    format!("\x1b[1m{}\x1b[22m", text)
}

/// Apply italic style.
pub fn italic(text: &str) -> String {
    format!("\x1b[3m{}\x1b[23m", text)
}

/// Apply underline style.
pub fn underline(text: &str) -> String {
    format!("\x1b[4m{}\x1b[24m", text)
}

/// Apply inverse (reverse video) style.
pub fn inverse(text: &str) -> String {
    format!("\x1b[7m{}\x1b[27m", text)
}

/// Apply dim/faint style.
/// Wrap the columns `[from, from+width)` of a line in inverse video.
///
/// Used to draw a mouse selection: the cells keep their text and colours, and
/// only the background/foreground swap — the terminal rendering of "selected".
pub fn highlight_columns(line: &str, from: usize, width: usize) -> String {
    let total = visible_width(line);
    if width == 0 || from >= total {
        return line.to_string();
    }
    let width = width.min(total - from);
    let before = slice_by_column(line, 0, from, true);
    let mut selected = slice_by_column(line, from, width, true);
    let after = slice_by_column(line, from + width, total - from - width, true);
    // Re-assert inverse after any nested reset so a coloured span inside the
    // selection cannot punch a hole in the highlight.
    selected = selected.replace("\x1b[0m", "\x1b[0m\x1b[7m");
    format!("{before}\x1b[7m{selected}\x1b[27m{after}")
}

pub fn dim(text: &str) -> String {
    format!("\x1b[2m{}\x1b[22m", text)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_row_can_only_carry_glyphs_and_styling() {
        // Every string a component renders is written into the terminal as
        // input. Tool output, model output, extension messages, MCP payloads
        // and pasted text all reach a row, so a row must not be able to move the
        // cursor, erase, switch modes or talk to the clipboard.
        let row = "\x1b[31mred\x1b[0m\x1b[2J\x1b[1;1Hhi\rmore\x07\x1b]52;c;cGF3bg==\x07z\tt";
        let clean = super::sanitize_row(row).expect("escapes and controls are rewritten");
        assert_eq!(clean, "\x1b[31mred\x1b[0mhimorezt");
        assert_eq!(
            super::sanitize_row(&clean),
            None,
            "sanitizing is idempotent"
        );
    }

    #[test]
    fn an_allowlisted_sequence_cannot_carry_an_injection() {
        // The allowlist matches a kind of sequence, not just a prefix. A payload
        // that terminates the string early — or never terminates it — must not
        // smuggle a second escape through: terminals abort an OSC at a bare
        // `ESC` and then execute what follows.
        for row in [
            "\x1b]8;;http://x\x1b[2Jafter",
            "\x1b]8;;\x1b[2J\x07",
            "\x1b]133;A\x1b[?1049l\x07",
            "\x1b_pi:c\x1b[1;1Htail\x07",
            "\x1b_Ga=T;A\x1b[?1049l\x1b\\",
            "\x1b]1337;File=name=1:x\x1b]52;c;cGF3bg==\x07\x07",
            // Private/mode CSI forms end in `m` but are not styling.
            "\x1b[>4;2mtext",
        ] {
            let clean = super::sanitize_row(row).expect("injections are rewritten");
            assert!(
                !clean.contains("\x1b[2J")
                    && !clean.contains("\x1b[?1049l")
                    && !clean.contains("\x1b[1;1H")
                    && !clean.contains("\x1b]52;")
                    && !clean.contains("\x1b[>4;2m"),
                "{row:?} -> {clean:?}"
            );
        }
    }

    #[test]
    fn ki_printable_escapes_survive_a_row() {
        // Styling, hyperlinks, APC markers, prompt zones and the image
        // protocols are what ki itself emits into rows; dropping them would
        // blank the frame or break clickable links.
        for escape in [
            "\x1b[1;38;2;1;2;3m",
            "\x1b]8;;https://example.com\x07",
            super::CURSOR_MARKER,
            "\x1b]133;A\x07",
            "\x1b]1337;File=name=1;inline=1:AAAA\x07",
            "\x1b_Ga=T;AAAA\x1b\\",
        ] {
            let row = format!("before{escape}after");
            assert_eq!(super::sanitize_row(&row), None, "{escape:?} is ki's own");
        }
    }

    #[test]
    fn external_text_is_measured_without_escapes_or_tabs() {
        // A tab is zero columns to `visible_width` but a tab stop to the real
        // terminal, so text ki does not own is expanded before it is measured.
        let text = super::sanitize_external_text("\x1b[31mred\x1b[0m\ta\r\nb\x00c\n");
        assert_eq!(text, "red    a\nbc\n");
        assert_eq!(super::visible_width("red    a"), 8);
    }

    #[test]
    fn highlight_columns_inverts_exactly_the_selected_cells() {
        let line = "abcdefghij";
        let out = super::highlight_columns(line, 2, 3);
        assert_eq!(super::strip_ansi(&out), line, "text is preserved");
        assert!(out.contains("\x1b[7m"), "{out:?}");
        let before = &out[..out.find("\x1b[7m").unwrap()];
        assert_eq!(super::strip_ansi(before), "ab");
        // A range past the end is clamped, an empty one is a no-op.
        assert_eq!(
            super::strip_ansi(&super::highlight_columns(line, 8, 99)),
            line
        );
        assert_eq!(super::highlight_columns(line, 0, 0), line);
    }

    use super::*;

    #[test]
    fn test_visible_width() {
        assert_eq!(visible_width("hello"), 5);
        assert_eq!(visible_width("\x1b[31mhello\x1b[0m"), 5);
        assert_eq!(visible_width("你好"), 4); // Chinese characters are 2 columns each
        assert_eq!(visible_width("\x1b[1m你好\x1b[0m"), 4);
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
        assert_eq!(strip_ansi("\x1b[1;31mhello\x1b[0m"), "hello");
    }

    #[test]
    fn test_hyperlink() {
        // let link = hyperlink("https://example.com", "Example");
        // assert!(link.contains("https://example.com"));
        // assert!(link.contains("Example"));
    }

    #[test]
    fn slice_by_column_preserves_sgr_when_asked() {
        let result = slice_by_column("\x1b[31mabcdef\x1b[0m", 1, 3, true);
        assert_eq!(strip_ansi(&result), "bcd");
        assert!(result.starts_with("\x1b[31m"), "result: {result:?}");
        assert!(result.ends_with("\x1b[0m"), "result: {result:?}");
        assert_eq!(visible_width(&result), 3);
    }

    #[test]
    fn slice_by_column_drops_styles_when_not_asked() {
        let result = slice_by_column("\x1b[31mabcdef\x1b[0m", 1, 3, false);
        assert_eq!(result, "bcd");
    }

    #[test]
    fn slice_by_column_keeps_the_cursor_marker() {
        let line = format!("\x1b[32mab{CURSOR_MARKER}cd\x1b[0m");
        let result = slice_by_column(&line, 1, 2, true);
        assert!(result.contains(CURSOR_MARKER), "result: {result:?}");
        assert!(result.contains("\x1b[32m"), "result: {result:?}");
        assert_eq!(strip_ansi(&result), "bc");
        assert_eq!(visible_width(&result), 2);
    }

    #[test]
    fn slice_by_column_wide_graphemes_are_not_split() {
        let result = slice_by_column("中文", 1, 1, true);
        assert_eq!(visible_width(&result), 1);
    }

    #[test]
    fn slice_by_column_does_not_repeat_the_reset() {
        // The slice is fully inside a styled span that is already closed: the
        // trailing reset must not be added a second time, and a style reset
        // inside the window must not leave the slice "still styled".
        let result = slice_by_column("\x1b[31mab\x1b[0mcdef", 0, 4, true);
        assert_eq!(result, "\x1b[31mab\x1b[0mcd");
        assert_eq!(strip_ansi(&result), "abcd");
        assert_eq!(visible_width(&result), 4);
    }

    #[test]
    fn slice_by_column_drops_escapes_that_start_after_the_range() {
        // A hyperlink opened past the window used to be emitted anyway, which
        // made the rest of the row part of the link (`ESC[0m` does not close
        // OSC-8). Nothing past `end` may be emitted.
        let line = "abc\x1b]8;;http://example.com\x07def";
        let result = slice_by_column(line, 0, 3, true);
        assert_eq!(result, "abc");
    }

    #[test]
    fn slice_by_column_closes_an_osc8_link_it_cut() {
        // The link opens inside the window and its close falls outside: the
        // slice must close it itself (hstack calls this without the
        // `SEGMENT_RESET` fallback that layout uses).
        let line = "ab\x1b]8;;http://example.com\x07cd\x1b]8;;\x07ef";
        let result = slice_by_column(line, 0, 3, true);
        assert_eq!(strip_ansi(&result), "abc");
        assert!(result.ends_with(OSC8_CLOSE), "result: {result:?}");
        assert_eq!(result, "ab\x1b]8;;http://example.com\x07c\x1b]8;;\x07");
    }

    #[test]
    fn slice_by_column_keeps_a_link_opened_before_the_range() {
        let line = "\x1b]8;;http://example.com\x07abcd\x1b]8;;\x07";
        let result = slice_by_column(line, 1, 2, true);
        assert_eq!(strip_ansi(&result), "bc");
        assert_eq!(result, "\x1b]8;;http://example.com\x07bc\x1b]8;;\x07");
    }

    #[test]
    fn slice_by_column_drops_a_link_closed_before_the_range() {
        let line = "\x1b]8;;http://example.com\x07a\x1b]8;;\x07bcd";
        let result = slice_by_column(line, 1, 2, true);
        assert_eq!(result, "bc");
    }
}
