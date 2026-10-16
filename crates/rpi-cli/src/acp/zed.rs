//! Register ki as a custom external agent in Zed (`ki acp install-zed`).
//!
//! Ported nearly verbatim from the same-owner MIT predecessor
//! (`rpi-package/packages/rpi-acp/src/zed.rs`), with ki naming. Zed stores
//! configuration in `settings.json`, which is **JSONC**: it allows `//` and
//! `/* */` comments as well as trailing commas — Zed's own default file ships a
//! comment header, and hand-edited files routinely carry trailing commas.
//! Round-tripping that through a plain JSON parser would delete the user's
//! comments and reformat the whole document, so this module instead scans for
//! the insertion point and splices in a single member, leaving every other byte
//! untouched.
//!
//! The result Zed needs is one entry:
//!
//! ```jsonc
//! "agent_servers": {
//!   "ki": { "type": "custom", "command": "<path-to-ki>", "args": ["acp", "serve"] }
//! }
//! ```
//!
//! so `ki acp install-zed` is all a user needs, with no registry submission and
//! no hand-editing. Nothing environment-specific is baked into the snippet —
//! ki reads its model credentials from its own configuration at serve time, so
//! the registration carries only the command and args (no env field).

use std::path::{Path, PathBuf};

/// Key Zed shows the agent under, and the member we own inside `agent_servers`.
pub(crate) const SERVER_ID: &str = "ki";

/// How Zed should launch the agent: the `agent_servers.<id>` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Registration {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
}

impl Registration {
    /// Pick the entry from how *this* process was launched: whatever binary is
    /// running gets `acp serve` as its args (falling back to a bare `ki` when
    /// the executable path is unavailable).
    pub(crate) fn detect() -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ki"));
        Self::for_executable(&exe)
    }

    pub(crate) fn for_executable(exe: &Path) -> Self {
        Registration {
            command: exe.display().to_string(),
            args: vec!["acp".to_owned(), "serve".to_owned()],
        }
    }

    /// The full `agent_servers` block, for `ki acp print-zed` and documentation.
    pub(crate) fn snippet(&self) -> String {
        format!(
            "{{\n  \"agent_servers\": {{\n    {}\n  }}\n}}",
            self.member(4)
        )
    }

    /// `"ki": { ... }` at indentation `pad` (later lines indented relative).
    fn member(&self, pad: usize) -> String {
        let quote = '"';
        let command = serde_json::to_string(&self.command).unwrap_or_else(|_| "\"ki\"".into());
        let args = self
            .args
            .iter()
            .filter_map(|arg| serde_json::to_string(arg).ok())
            .collect::<Vec<_>>()
            .join(", ");
        let p = |n: usize| " ".repeat(n);
        format!(
            "{q}{SERVER_ID}{q}: {{\n\
             {p2}{q}type{q}: {q}custom{q},\n\
             {p2}{q}command{q}: {command},\n\
             {p2}{q}args{q}: [{args}]\n\
             {p1}}}",
            q = quote,
            p1 = p(pad),
            p2 = p(pad + 2),
        )
    }
}

/// What `register_file` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// `settings.json` did not exist and was created.
    Created,
    /// The member was spliced in; `backup` is the pre-change copy, if kept.
    Updated { backup: Option<PathBuf> },
    /// `agent_servers.ki` was already present; nothing was written.
    AlreadyPresent,
}

/// Where Zed keeps `settings.json` on this host.
pub(crate) fn settings_path() -> Option<PathBuf> {
    let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    settings_path_from(
        std::env::consts::OS,
        get("APPDATA"),
        get("XDG_CONFIG_HOME"),
        get("HOME").or_else(|| get("USERPROFILE")),
        get("ZED_SETTINGS"),
    )
}

/// Pure path resolution, so it can be tested without touching the environment.
///
/// Per Zed's own docs (all-settings / external-agents) the settings file lives
/// at `$XDG_CONFIG_HOME/zed/settings.json` else `~/.config/zed/settings.json`
/// on **both** macOS and Linux — the `~/Library/Application Support` location
/// the rpi-acp precedent used is wrong. Windows keeps `%APPDATA%\Zed`.
fn settings_path_from(
    os: &str,
    appdata: Option<String>,
    xdg: Option<String>,
    home: Option<String>,
    explicit: Option<String>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(PathBuf::from(path));
    }
    if os == "windows" {
        return appdata.map(|dir| PathBuf::from(dir).join("Zed").join("settings.json"));
    }
    let base = xdg
        .map(PathBuf::from)
        .or_else(|| home.map(|dir| PathBuf::from(dir).join(".config")))?;
    Some(base.join("zed").join("settings.json"))
}

/// Ensure `agent_servers.ki` is present in the JSONC document `text`.
///
/// Returns `Ok(None)` when it is already registered, or `Ok(Some(new_text))`
/// with only the inserted member added.
pub(crate) fn ensure_registered(
    text: &str,
    registration: &Registration,
) -> Result<Option<String>, String> {
    let bytes = text.as_bytes();
    let (root_open, root_close) = root_object(bytes)
        .ok_or("settings.json is not a JSON object (did you mean to edit it by hand?)")?;

    match member_value(bytes, root_open, root_close, "agent_servers") {
        // Present and an object: add `ki` inside unless it is already there.
        Some(value) if bytes.get(value) == Some(&b'{') => {
            let (open, close) = matching_delim(bytes, value)
                .ok_or("settings.json has an unterminated `agent_servers` object")?;
            if member_value(bytes, open, close, SERVER_ID).is_some() {
                return Ok(None);
            }
            // `agent_servers` is a top-level key, so its members sit at 4 and its
            // closing brace at 2.
            Ok(Some(splice_object(
                text,
                open,
                close,
                &registration.member(4),
                4,
                2,
            )))
        }
        // Present but not an object: refuse rather than silently rewrite it.
        Some(_) => Err(
            "settings.json already has an `agent_servers` value that is not an object".to_owned(),
        ),
        // Absent: append a new top-level member.
        None => {
            let member = registration.member(4);
            let block = format!(
                "{q}agent_servers{q}: {{\n{pad}{member}\n  }}",
                q = '"',
                pad = "    ",
            );
            Ok(Some(splice_object(
                text, root_open, root_close, &block, 2, 0,
            )))
        }
    }
}

/// Insert `member` before `close`, keeping every byte of the document except
/// the splice itself.
///
/// `member_indent` is the indentation the member's own line should get and
/// `close_indent` that of the closing brace. Takes the trailing comma of the
/// previous member into account, so both `{ a: 1 }` and `{ a: 1, }` work —
/// and so does a trailing `//` / `/* */` comment: the comma decision looks at
/// the last *live* token (a comma buried inside a trailing line comment is
/// dead and leaves the members unseparated, while a second comma after a
/// block comment is a doubled separator), and the trailing trivia stays in
/// the document, moved below the inserted member.
fn splice_object(
    text: &str,
    open: usize,
    close: usize,
    member: &str,
    member_indent: usize,
    close_indent: usize,
) -> String {
    let (last_live_end, saw_comment) = live_tail(text, open, close);
    let needs_comma = match last_live_end {
        // Comment-only body: nothing to separate from.
        None => false,
        Some(end) => !matches!(text.as_bytes()[end - 1], b'{' | b','),
    };
    let head_end = last_live_end.unwrap_or(open + 1);
    let head = &text[..head_end];
    // Trailing trivia (whitespace + comments) is preserved when it carries a
    // comment and dropped when it is whitespace only — the no-comment result
    // is byte-identical to trimming the tail before the member.
    let trivia = if saw_comment {
        &text[head_end..close]
    } else {
        ""
    };
    let suffix = &text[close..];
    let mut body = String::new();
    if needs_comma {
        body.push(',');
    }
    body.push('\n');
    body.push_str(&" ".repeat(member_indent));
    body.push_str(member);
    body.push('\n');
    body.push_str(&" ".repeat(close_indent));
    format!("{head}{body}{trivia}{suffix}")
}

/// Scan the object body `(open, close)` left to right, skipping strings whole
/// (so a `//` inside a string is content) and `//` / `/* */` comments, and
/// return the position just past the last live (non-trivia) byte — `None` for
/// an empty or comment-only body — plus whether any comment was skipped.
fn live_tail(text: &str, open: usize, close: usize) -> (Option<usize>, bool) {
    let b = text.as_bytes();
    let mut i = open + 1;
    let mut last_live_end = None;
    let mut saw_comment = false;
    while i < close {
        match b[i] {
            b'"' => {
                let mut j = i + 1;
                while j < close {
                    match b[j] {
                        b'\\' => j += 2,
                        b'"' => {
                            j += 1;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                i = j.min(close);
                last_live_end = Some(i);
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < close && b[i] != b'\n' {
                    i += 1;
                }
                saw_comment = true;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < close && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(close);
                saw_comment = true;
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                i += 1;
                last_live_end = Some(i);
            }
        }
    }
    (last_live_end, saw_comment)
}

// Borrowed helpers below treat the document as bytes and understand JSONC.
fn root_object(b: &[u8]) -> Option<(usize, usize)> {
    let start = skip_trivia(b, 0);
    if b.get(start) != Some(&b'{') {
        return None;
    }
    matching_delim(b, start)
}

/// Skip whitespace and `//` / `/* */` comments.
fn skip_trivia(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match (b.get(i), b.get(i + 1)) {
            (Some(b'/'), Some(b'/')) => {
                i += 2;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            (Some(b'/'), Some(b'*')) => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            _ => return i,
        }
    }
}

/// `i` must be a `"`; returns the index just past the closing quote.
fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// The `(open, close)` indices of the `{}` / `[]` starting at `open`.
fn matching_delim(b: &[u8], open: usize) -> Option<(usize, usize)> {
    let close_byte = match b.get(open) {
        Some(b'{') => b'}',
        Some(b'[') => b']',
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i = skip_string(b, i)?;
                continue;
            }
            b'/' if matches!(b.get(i + 1), Some(b'/') | Some(b'*')) => {
                i = skip_trivia(b, i);
                continue;
            }
            byte if byte == b[open] => depth += 1,
            byte if byte == close_byte => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index where the value of the top-level `key` inside `open..close` starts.
fn member_value(b: &[u8], open: usize, close: usize, key: &str) -> Option<usize> {
    let mut i = skip_trivia(b, open + 1);
    while i < close && b.get(i) == Some(&b'"') {
        let key_end = skip_string(b, i)?;
        let name = &b[i + 1..key_end - 1];
        let after = skip_trivia(b, key_end);
        if b.get(after) != Some(&b':') {
            return None;
        }
        let value = skip_trivia(b, after + 1);
        if name == key.as_bytes() {
            return Some(value);
        }
        let next = skip_trivia(b, skip_value(b, value)?);
        if b.get(next) == Some(&b',') {
            i = skip_trivia(b, next + 1);
        } else {
            break;
        }
    }
    None
}

/// Index just past the value starting at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => matching_delim(b, i).map(|(_, close)| close + 1),
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']') && !b[j].is_ascii_whitespace()
            {
                j += 1;
            }
            Some(j)
        }
    }
}

/// One-line description of an [`Outcome`], for the CLI frontend. `dry_run`
/// switches to the conditional verbs, because nothing was written.
pub(crate) fn describe(outcome: &Outcome, path: &Path, dry_run: bool) -> String {
    let display = path.display();
    match outcome {
        Outcome::AlreadyPresent => format!("already registered in {display}"),
        Outcome::Created if dry_run => format!("would create {display} with agent_servers.ki"),
        Outcome::Created => format!("created {display} with agent_servers.ki"),
        Outcome::Updated { .. } if dry_run => format!("would add ki to {display}"),
        Outcome::Updated { .. } => format!("added ki to {display}"),
    }
}

/// Back `ki acp install-zed`. Returns the process exit code: 0 = registered
/// (or a clean dry run), 1 = the file could not be read/written or the
/// document shape was refused.
pub(crate) fn install_zed(rest: &[String]) -> i32 {
    let mut settings: Option<PathBuf> = None;
    let mut command: Option<String> = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--dry-run" => dry_run = true,
            "--settings" => match rest.get(index + 1) {
                Some(value) => {
                    settings = Some(PathBuf::from(value));
                    index += 1;
                }
                None => {
                    eprintln!("error: --settings needs a path");
                    return 1;
                }
            },
            "--command" => match rest.get(index + 1) {
                Some(value) => {
                    command = Some(value.clone());
                    index += 1;
                }
                None => {
                    eprintln!("error: --command needs a path");
                    return 1;
                }
            },
            other => {
                eprintln!("error: unknown option `{other}` (try --help)");
                return crate::app::EXIT_USAGE;
            }
        }
        index += 1;
    }

    let path = match settings.or_else(settings_path) {
        Some(path) => path,
        None => {
            eprintln!("error: cannot locate Zed's settings.json; pass --settings <FILE>");
            return 1;
        }
    };

    let mut registration = Registration::detect();
    if let Some(command) = command {
        registration = Registration {
            command,
            args: registration.args,
        };
    }

    match register_file(&path, &registration, dry_run) {
        Ok(outcome) => {
            println!("{}", describe(&outcome, &path, dry_run));
            if !dry_run {
                if let Outcome::Updated {
                    backup: Some(backup),
                } = &outcome
                {
                    println!("backup: {}", backup.display());
                }
            }
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

/// Ensure the file at `path` registers ki; create it if missing.
///
/// Writes atomically (temp + rename) and keeps the pre-change file as
/// `<name>.ki-bak` the first time it is modified.
pub(crate) fn register_file(
    path: &Path,
    registration: &Registration,
    dry_run: bool,
) -> Result<Outcome, String> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };

    let (outcome, new_text) = match existing {
        None => {
            let member = registration.member(4);
            let text =
                format!("// Zed settings\n{{\n  \"agent_servers\": {{\n    {member}\n  }}\n}}\n");
            (Outcome::Created, text)
        }
        Some(text) => match ensure_registered(&text, registration)? {
            None => return Ok(Outcome::AlreadyPresent),
            Some(new_text) => {
                let backup = backup_path(path);
                (Outcome::Updated { backup }, new_text)
            }
        },
    };

    if dry_run {
        return Ok(outcome);
    }
    if let Outcome::Updated {
        backup: Some(backup),
    } = &outcome
    {
        // Keep the *first* pre-change copy: a second run must not clobber it
        // with an already-modified file.
        if !backup.exists() {
            std::fs::copy(path, backup)
                .map_err(|error| format!("cannot write {}: {error}", backup.display()))?;
        }
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
    }
    let temp = sibling(path, ".ki-tmp");
    std::fs::write(&temp, &new_text).map_err(|error| {
        // A failed write can leave a partial temp behind (ENOSPC, quota,
        // sandbox deny); the rename path cleans up the same way.
        let _ = std::fs::remove_file(&temp);
        format!("cannot write {}: {error}", temp.display())
    })?;
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot replace {}: {error}", path.display())
    })?;
    Ok(outcome)
}

fn backup_path(path: &Path) -> Option<PathBuf> {
    Some(sibling(path, ".ki-bak"))
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "settings.json".to_owned());
    path.with_file_name(format!("{name}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registration() -> Registration {
        Registration {
            command: "/usr/local/bin/ki".to_owned(),
            args: vec!["acp".to_owned(), "serve".to_owned()],
        }
    }

    /// Per-test temp dir, unique per process so parallel test binaries never
    /// collide on each other's leftovers.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ki-acp-zed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Mirrors a real Zed file: comment header, trailing commas, strings that
    /// contain braces, and an `agent_servers` object that is not ours to rewrite.
    const REALISH: &str = r#"// Zed settings
//
// docs: https://zed.dev/docs/configuring-zed
{
  "terminal": {
    "dock": "bottom"
  },
  "agent": {
    "commit_message_instructions": "use <type>(<scope>): <subject> {like this}"
  },
  "theme": {
    "mode": "dark",
    "dark": "macOS Classic Dark",
  },
  "agent_servers": {
    "other": {
      "type": "custom",
      "command": "/usr/bin/other",
      "args": [],
    },
  },
}
"#;

    #[test]
    fn inserts_into_an_existing_agent_servers_object() {
        let new = ensure_registered(REALISH, &registration())
            .unwrap()
            .unwrap();

        // Our member lands inside `agent_servers`, after the existing one.
        assert!(new.contains("\"ki\": {\n      \"type\": \"custom\","));
        assert!(new.contains("\"command\": \"/usr/local/bin/ki\","));
        assert!(new.contains("\"args\": [\"acp\", \"serve\"]"));
        // Everything else is byte-for-byte intact, comments included.
        for line in REALISH.lines() {
            assert!(new.contains(line), "lost line: {line}");
        }
        assert!(new.contains("// docs: https://zed.dev/docs/configuring-zed"));
        assert!(new.contains("\"dark\": \"macOS Classic Dark\","));
        // The other agent survives.
        assert!(new.contains("\"/usr/bin/other\""));
    }

    #[test]
    fn is_idempotent() {
        let once = ensure_registered(REALISH, &registration())
            .unwrap()
            .unwrap();
        assert!(ensure_registered(&once, &registration()).unwrap().is_none());
    }

    #[test]
    fn snippet_is_indented_and_shaped() {
        let snippet = registration().snippet();
        // The `ki` line sits under `agent_servers`, not flush against column 0.
        assert!(snippet.contains("\n    \"ki\": {\n"), "{snippet}");
        assert!(snippet.contains("\n      \"args\": [\"acp\", \"serve\"]\n"));
        assert!(snippet.trim_end().ends_with('}'));
        // No env field is ever baked into the snippet: ki reads its own
        // configuration at serve time, so no secret can leak into settings.json.
        assert!(!snippet.to_ascii_lowercase().contains("env"), "{snippet}");
    }

    #[test]
    fn adds_agent_servers_when_absent() {
        let text = "{\n  \"ui_font_size\": 16,\n}\n";
        let new = ensure_registered(text, &registration()).unwrap().unwrap();
        assert!(new.contains("\"agent_servers\": {\n    \"ki\": {"));
        assert!(new.contains("\"ui_font_size\": 16,"));
        // No doubled comma from the existing trailing comma.
        assert!(!new.contains(",,"));
    }

    /// The comma decision must see the last *live* token: a separator buried
    /// inside a trailing comment is dead and leaves the document unparseable,
    /// and a second comma after a block comment is a doubled separator.
    #[test]
    fn splices_after_a_trailing_line_comment_without_burying_the_comma() {
        let text = "{\n  \"ui_font_size\": 16\n  // keep tools honest\n}\n";
        let new = ensure_registered(text, &registration()).unwrap().unwrap();
        // The separator lands on the live member, not inside the comment.
        assert!(new.contains("\"ui_font_size\": 16,\n"), "{new}");
        assert!(new.contains("// keep tools honest\n"), "{new}");
        assert!(!new.contains("honest,"), "{new}");
        // Every original line survives, and the result is valid JSON once the
        // comments (JSONC) are stripped.
        for line in text.lines() {
            assert!(new.contains(line), "lost line: {line}");
        }
        let stripped: String = new
            .lines()
            .map(|line| match line.find("//") {
                Some(at) => &line[..at],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str::<serde_json::Value>(&stripped)
            .expect("spliced document must parse as JSONC");
    }

    #[test]
    fn splices_after_a_trailing_block_comment_without_doubling_the_comma() {
        let text = "{\n  \"ui_font_size\": 16,\n  /* tune */\n}\n";
        let new = ensure_registered(text, &registration()).unwrap().unwrap();
        // The pre-existing live comma is enough; the block comment must not
        // gain a second one.
        assert!(new.contains("/* tune */\n"), "{new}");
        assert!(!new.contains("*/,"), "{new}");
        assert!(!new.contains(",\n  ,"), "{new}");
        for line in text.lines() {
            assert!(new.contains(line), "lost line: {line}");
        }
    }

    #[test]
    fn splices_a_line_comment_that_carries_a_url_string_safely() {
        // A `//` inside a string is content: the last live token is the
        // string's closing quote, and the comma still lands after it.
        let text = "{\n  \"docs\": \"https://zed.dev/docs\"\n}\n";
        let new = ensure_registered(text, &registration()).unwrap().unwrap();
        assert!(
            new.contains("\"docs\": \"https://zed.dev/docs\",\n"),
            "{new}"
        );
        serde_json::from_str::<serde_json::Value>(&new).expect("string // is content");
    }

    #[test]
    fn splices_into_a_comment_only_object_without_a_separator() {
        let text = "{ /* just a comment */ }";
        let new = ensure_registered(text, &registration()).unwrap().unwrap();
        assert!(new.contains("/* just a comment */"), "{new}");
        assert!(!new.contains(", /*"), "{new}");
        assert!(new.contains("\"agent_servers\""), "{new}");
    }

    #[test]
    fn adds_agent_servers_to_an_empty_object() {
        let new = ensure_registered("{}\n", &registration()).unwrap().unwrap();
        assert_eq!(
            new,
            "{\n  \"agent_servers\": {\n    \"ki\": {\n      \"type\": \"custom\",\n      \
             \"command\": \"/usr/local/bin/ki\",\n      \"args\": [\"acp\", \"serve\"]\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn handles_an_empty_agent_servers_object() {
        let text = "{\n  \"agent_servers\": {}\n}\n";
        let new = ensure_registered(text, &registration()).unwrap().unwrap();
        assert!(new.contains("\"agent_servers\": {\n    \"ki\": {"));
        assert!(ensure_registered(&new, &registration()).unwrap().is_none());
    }

    #[test]
    fn refuses_to_rewrite_a_non_object_agent_servers() {
        let text = "{\n  \"agent_servers\": 3\n}\n";
        let error = ensure_registered(text, &registration()).unwrap_err();
        assert!(error.contains("not an object"), "{error}");
    }

    #[test]
    fn rejects_a_document_that_is_not_an_object() {
        assert!(ensure_registered("[1, 2]", &registration()).is_err());
    }

    #[test]
    fn ignores_a_commented_out_agent_servers() {
        // A commented key must not be mistaken for a real one.
        let text = "{\n  // \"agent_servers\": {},\n  \"ui_font_size\": 16\n}\n";
        let new = ensure_registered(text, &registration()).unwrap().unwrap();
        assert!(new.contains("\n  \"agent_servers\": {\n    \"ki\": {"));
        assert!(new.starts_with("{\n  // \"agent_servers\": {},"));
    }

    #[test]
    fn detect_carries_the_acp_serve_args() {
        let registration = Registration::for_executable(Path::new("/opt/ki"));
        assert_eq!(registration.command, "/opt/ki");
        assert_eq!(
            registration.args,
            vec!["acp".to_owned(), "serve".to_owned()]
        );
    }

    #[test]
    fn settings_path_resolution_is_per_os() {
        let win = settings_path_from(
            "windows",
            Some("C:/Users/me/AppData/Roaming".into()),
            None,
            Some("C:/Users/me".into()),
            None,
        )
        .unwrap();
        assert_eq!(
            win,
            PathBuf::from("C:/Users/me/AppData/Roaming/Zed/settings.json")
        );

        // macOS follows the XDG convention (Zed docs), NOT ~/Library/Application
        // Support — the fix over the rpi-acp precedent.
        let mac = settings_path_from("macos", None, None, Some("/Users/me".into()), None).unwrap();
        assert_eq!(mac, PathBuf::from("/Users/me/.config/zed/settings.json"));

        let mac_xdg = settings_path_from(
            "macos",
            None,
            Some("/cfg".into()),
            Some("/Users/me".into()),
            None,
        )
        .unwrap();
        assert_eq!(mac_xdg, PathBuf::from("/cfg/zed/settings.json"));

        let linux = settings_path_from("linux", None, None, Some("/home/me".into()), None).unwrap();
        assert_eq!(linux, PathBuf::from("/home/me/.config/zed/settings.json"));

        let explicit = settings_path_from(
            "linux",
            None,
            None,
            Some("/home/me".into()),
            Some("/tmp/z.json".into()),
        )
        .unwrap();
        assert_eq!(explicit, PathBuf::from("/tmp/z.json"));
    }

    #[test]
    fn describe_names_each_outcome() {
        let path = Path::new("/tmp/settings.json");
        assert!(describe(&Outcome::AlreadyPresent, path, false).starts_with("already registered"));
        assert!(describe(&Outcome::Created, path, false).starts_with("created"));
        assert!(describe(&Outcome::Created, path, true).starts_with("would create"));
        assert!(describe(&Outcome::Updated { backup: None }, path, false).starts_with("added ki"));
        assert!(describe(&Outcome::Updated { backup: None }, path, true).starts_with("would add"));
    }

    #[test]
    fn register_file_creates_and_then_is_a_no_op() {
        let dir = scratch("register");
        let path = dir.join("settings.json");

        assert_eq!(
            register_file(&path, &registration(), false).unwrap(),
            Outcome::Created
        );
        let created = std::fs::read_to_string(&path).unwrap();
        assert!(created.contains("\n    \"ki\": {\n"), "{created}");
        assert!(
            created.contains("\n      \"args\": [\"acp\", \"serve\"]\n"),
            "{created}"
        );
        assert_eq!(
            register_file(&path, &registration(), false).unwrap(),
            Outcome::AlreadyPresent
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_file_backs_up_once_and_replaces_atomically() {
        let dir = scratch("backup");
        let path = dir.join("settings.json");
        std::fs::write(&path, REALISH).unwrap();

        let outcome = register_file(&path, &registration(), false).unwrap();
        let backup = match outcome {
            Outcome::Updated { backup } => backup.expect("backup path"),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), REALISH);
        assert!(!dir.join("settings.json.ki-tmp").exists());

        // A later change must not clobber the original backup.
        let modified = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"ki\"", "\"ki-x\"");
        std::fs::write(&path, &modified).unwrap();
        register_file(&path, &registration(), false).unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), REALISH);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = scratch("dry-run");
        let path = dir.join("settings.json");
        assert_eq!(
            register_file(&path, &registration(), true).unwrap(),
            Outcome::Created
        );
        assert!(!path.exists(), "dry run must not create the file");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
