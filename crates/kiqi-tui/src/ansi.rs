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
        // OSC / APC / DCS / PM: skip until BEL (0x07) or ST (ESC \).
        // `CURSOR_MARKER` (`ESC _ pi:c BEL`) is an APC sequence, so it is
        // consumed whole here and never contributes any visible width.
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
                        break;
                    }
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

/// Fold one escape sequence into the accumulated SGR context.
///
/// Non-SGR escapes (OSC hyperlinks, APC markers, ...) carry no style state and
/// are ignored; `ESC[0m` / `ESC[m` are the resets that clear the context.
fn accumulate_sgr(context: &mut String, escape: &str) {
    if !(escape.starts_with("\x1b[") && escape.ends_with('m')) {
        return;
    }
    if matches!(escape, "\x1b[0m" | "\x1b[m") {
        context.clear();
    }
    context.push_str(escape);
}

/// Slice a string by visible column positions.
///
/// Returns the part of `s` that starts at `start_col` and spans at most `width`
/// visible columns. Escape sequences are treated as zero-width atoms: they never
/// count towards a column and never get cut in half.
///
/// With `preserve_styles` the slice is coloured: it opens with the SGR state
/// accumulated before `start_col` (reset by `ESC[0m` / `ESC[m`), keeps the
/// zero-width escapes that fall inside the range (cursor markers, hyperlinks),
/// and closes with `ESC[0m` when a style is still active so nothing leaks into
/// the rest of the row. Without it every escape sequence is dropped, i.e. the
/// caller gets the plain-text slice.
///
/// Wide graphemes are never split in half: a grapheme straddling an edge is
/// replaced by spaces for the columns that overlap. `width == 0` returns an
/// empty string and an out-of-range `start_col` returns an empty string rather
/// than panicking.
pub fn slice_by_column(s: &str, start_col: usize, width: usize, preserve_styles: bool) -> String {
    if width == 0 {
        return String::new();
    }

    let end = start_col.saturating_add(width);
    let atoms = atoms(s);
    let mut column = 0;
    let mut started = false;
    let mut result = String::new();
    // SGR state before the first emitted grapheme, and the state of what has
    // actually been emitted (the trailing reset is decided by the latter).
    let mut context = String::new();
    let mut style = String::new();

    for atom in &atoms {
        if atom.escape {
            if !preserve_styles {
                continue;
            }
            if started {
                accumulate_sgr(&mut style, atom.text);
                result.push_str(atom.text);
            } else {
                accumulate_sgr(&mut context, atom.text);
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
                    result.push_str(&context);
                    style.push_str(&context);
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

    if preserve_styles && !style.is_empty() {
        result.push_str("\x1b[0m");
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
pub fn dim(text: &str) -> String {
    format!("\x1b[2m{}\x1b[22m", text)
}

#[cfg(test)]
mod tests {
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
}
