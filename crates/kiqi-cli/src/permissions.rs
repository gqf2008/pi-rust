//! Host-owned permission policy for tool calls.
//!
//! Storage and editing used to live in the compiled `kiqi-permissions` plugin,
//! which exposed only a `permissions` tool and no gate: the rules were stored
//! but nothing blocked a denied call. The host now loads, evaluates and
//! enforces the same files with the same semantics (a deny rule now blocks the
//! tool call); the plugin crate stays available as an opt-in feature for one
//! release.
//!
//! The effective file is re-read on every evaluation. The policy files are
//! tiny, and re-reading makes both an external edit and `/reload` take effect
//! without a cache to invalidate.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use kiqi_agent::agent_tool::AgentTool;
use kiqi_agent::error::AgentError;
use kiqi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use kiqi_ai::types::Tool;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Allow/deny rules in the persisted shape.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Rules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// The project rule file: the fork's `.ki` first, the pre-fork `.pi` second.
///
/// An existing `.pi/permissions.json` keeps being read (and written); a new one
/// lands where `ki` keeps its project state.
pub fn local_path(cwd: &Path) -> PathBuf {
    for dir in [".ki", ".pi"] {
        let candidate = cwd.join(dir).join("permissions.json");
        if candidate.exists() {
            return candidate;
        }
    }
    cwd.join(".ki").join("permissions.json")
}

pub fn global_path() -> PathBuf {
    // `RPI_AGENT_DIR` is the pre-fork spelling; **deliberate legacy trace** —
    // dropping it would make a configured agent dir fall back to `$HOME` and
    // read/write the wrong `permissions.json`.
    for var in ["KI_AGENT_DIR", "KI_CODING_AGENT_DIR", "RPI_AGENT_DIR"] {
        if let Some(dir) = std::env::var_os(var) {
            return PathBuf::from(dir).join("permissions.json");
        }
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    for dir in [".ki", ".pi"] {
        let candidate = home.join(dir).join("agent").join("permissions.json");
        if candidate.exists() {
            return candidate;
        }
    }
    home.join(".ki").join("agent").join("permissions.json")
}

/// Project rules win over global rules (the plugin's precedence): an existing
/// project file is the whole effective policy, not a merge.
fn pick(local: PathBuf, global: PathBuf) -> PathBuf {
    if local.exists() {
        local
    } else {
        global
    }
}

/// The file that supplies the effective rules for `cwd`.
pub fn effective_path(cwd: &Path) -> PathBuf {
    pick(local_path(cwd), global_path())
}

/// Like [`effective_path`], but an untrusted project contributes no policy.
///
/// A freshly cloned repository can ship `.ki/permissions.json`, which is
/// project-controlled input; a session that declined project trust already
/// disables the project's settings, resources and extensions, so the gate must
/// not honor its rules either. The user's global policy still applies.
pub fn effective_path_for(cwd: &Path, project_trusted: bool) -> PathBuf {
    if project_trusted {
        effective_path(cwd)
    } else {
        global_path()
    }
}

/// Human-readable JSON kind, for errors that must say what the file actually
/// contains instead of silently dropping it.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

pub fn parse_rules(value: Value) -> Result<Rules, String> {
    let root = value.get("permissions").unwrap_or(&value);
    if let Some(obj) = root.as_object() {
        let strings = |key: &str| -> Result<Vec<String>, String> {
            let Some(value) = obj.get(key) else {
                return Ok(Vec::new());
            };
            let Some(items) = value.as_array() else {
                return Err(format!(
                    "permissions.{key} must be an array of rule strings, not {}",
                    json_kind(value)
                ));
            };
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(rule) = item.as_str() else {
                    return Err(format!(
                        "permissions.{key} must contain only strings, found {}",
                        json_kind(item)
                    ));
                };
                let rule = rule.trim();
                if !rule.is_empty() {
                    out.push(rule.to_owned());
                }
            }
            Ok(out)
        };
        return Ok(Rules {
            allow: strings("allow")?,
            deny: strings("deny")?,
        });
    }
    // Compatibility with the first ki release, which stored capability
    // objects as a top-level array.
    if let Some(items) = value.as_array() {
        return Ok(Rules {
            allow: items
                .iter()
                .filter(|r| r.get("effect").and_then(Value::as_str) == Some("allow"))
                .filter_map(|r| r.get("capability").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect(),
            deny: Vec::new(),
        });
    }
    Err("permissions must be an object with allow/deny arrays".into())
}

pub fn load(path: &Path) -> Result<Rules, String> {
    if !path.exists() {
        return Ok(Rules::default());
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("read permissions: {e}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("parse permissions: {e}"))?;
    parse_rules(value)
}

pub fn save(path: &Path, rules: &Rules) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create permission directory: {e}"))?;
    }
    let text = serde_json::to_string_pretty(
        &json!({"permissions": {"allow": rules.allow, "deny": rules.deny}}),
    )
    .map_err(|e| e.to_string())?;
    write_store_atomically(path, &text)
}

/// Write `text` to `path` via a temp sibling + rename.
///
/// `fs::write` truncates and then writes in place, so a shorter document written
/// over a longer one leaves the previous tail behind, and two ki sessions
/// sharing a project can interleave their writes. Either way the rule file
/// becomes unparsable. `rename` replaces atomically on Unix and on Windows
/// (`MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`), so a reader only ever sees
/// a whole document.
/// Serializes read-modify-write cycles on the rule file. Parallel tool
/// execution can run several `permissions` calls at once; without it a grant
/// and a revoke read the same base and the later write silently drops the
/// earlier edit.
static RULES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Distinguishes concurrent temp files. The pid alone is identical for every
/// call in the process, so two calls would write the same temp path.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_store_atomically(path: &Path, text: &str) -> Result<(), String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "permissions.json".to_string());
    let tmp = path.with_file_name(format!(
        "{name}.tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, text).map_err(|e| format!("write permissions: {e}"))?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("commit permissions: {error}"));
    }
    Ok(())
}

fn split_rule(rule: &str) -> (&str, &str) {
    if let Some(open) = rule.find('(') {
        if rule.ends_with(')') && open > 0 {
            return (&rule[..open], &rule[open + 1..rule.len() - 1]);
        }
    }
    ("*", rule)
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    let (mut pi, mut vi, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    let p = pattern.as_bytes();
    let v = value.as_bytes();
    while vi < v.len() {
        if pi < p.len() && (p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            mark = vi;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            vi = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn rule_matches(rule: &str, tool: &str, target: &str) -> bool {
    let (rule_tool, pattern) = split_rule(rule);
    (rule_tool == "*" || rule_tool.eq_ignore_ascii_case(tool)) && glob_matches(pattern, target)
}

/// The verdict for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { reason: String },
}

/// Evaluate `tool`/`target` against `rules`.
///
/// Deny wins over allow. With no allow rule whose tool part matches, unmatched
/// calls are permitted (the plugin's default); once one exists, the call must
/// match an allow rule.
pub fn decide(rules: &Rules, tool: &str, target: &str) -> Decision {
    if let Some(rule) = rules
        .deny
        .iter()
        .find(|rule| rule_matches(rule, tool, target))
    {
        return Decision::Deny {
            reason: format!(
                "permission denied: tool `{tool}` matches deny rule `{rule}`{}",
                target_note(target)
            ),
        };
    }
    let has_allow_for_tool = rules.allow.iter().any(|rule| {
        let (rule_tool, _) = split_rule(rule);
        rule_tool == "*" || rule_tool.eq_ignore_ascii_case(tool)
    });
    if has_allow_for_tool
        && !rules
            .allow
            .iter()
            .any(|rule| rule_matches(rule, tool, target))
    {
        return Decision::Deny {
            reason: format!(
                "permission denied: tool `{tool}` is not matched by an allow rule{}",
                target_note(target)
            ),
        };
    }
    Decision::Allow
}

/// A block reason names the rule and the tool; a target makes it actionable
/// without dumping a whole command into the transcript.
fn target_note(target: &str) -> String {
    if target.is_empty() {
        return String::new();
    }
    let mut end = target.len().min(120);
    while end > 0 && !target.is_char_boundary(end) {
        end -= 1;
    }
    let suffix = if end < target.len() { "…" } else { "" };
    format!(" (target `{}{suffix}`)", &target[..end])
}

/// The values a rule is matched against for one call.
///
/// The key is chosen per tool, not by "first of value/command/path present".
/// Tool inputs deserialize into structs that ignore unknown keys, so a model
/// can send `{"command": "rm -rf /", "value": "git status"}`: the bash tool
/// runs `command`, and a precedence-based target would let the gate inspect
/// the harmless `value` instead. Every string the tool may act on is returned
/// and [`decide_call`] denies if any of them is denied.
fn target_candidates(tool: &str, args: &Value) -> Vec<String> {
    let lower = tool.to_ascii_lowercase();
    if lower == "permissions" {
        // Without a target the gate is blind to the tool's own effect: read
        // back the action, and the rule it would grant/revoke, so a policy can
        // name `permissions(grant)` / `permissions(revoke *)`.
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        let mut targets = vec![action.to_string()];
        if let Some(rule) = args
            .get("rule")
            .or_else(|| args.get("capability"))
            .and_then(Value::as_str)
            .filter(|rule| !rule.is_empty())
        {
            targets.push(format!("{action} {rule}"));
        }
        return targets;
    }
    if let Some(targets) = mcp_resource_candidates(&lower, args) {
        return targets;
    }
    let keys: &[&str] = match lower.as_str() {
        "bash" | "powershell" => &["command"],
        "read" | "write" | "edit" => &["path"],
        // `url` (webfetch) and `query` (websearch) are the target-bearing keys
        // of the shipped tools; without them a deny rule like
        // `webfetch(https://169.254.169.254/*)` compared against "" and never
        // matched.
        _ => &["value", "command", "path", "url", "query"],
    };
    let targets: Vec<String> = keys
        .iter()
        .filter_map(|key| args.get(*key).and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect();
    if targets.is_empty() {
        // Preserve the old "no target" behavior: a `*(*)` rule still matches.
        vec![String::new()]
    } else {
        targets
    }
}

/// `{server}__read_resource` / `{server}__watch_resource` act on a `uri` and,
/// for the watch verb, an `action`. Without these candidates a URI-scoped deny
/// rule silently fails open (the generic value/command/path keys find nothing)
/// and a URI-scoped allow rule rejects every call.
///
/// Candidates are the action, the uri, and the combined `action uri`, so a
/// policy can scope by action (`docs__watch_resource(start)`), by resource
/// (`docs__watch_resource(mem://public)`), or by both. `read` has no action
/// argument; its implicit action is `read`.
fn mcp_resource_candidates(lower: &str, args: &Value) -> Option<Vec<String>> {
    let watch = lower.ends_with(&format!("__{}", kiqi_mcp::WATCH_RESOURCE_SUFFIX));
    let read = lower.ends_with(&format!("__{}", kiqi_mcp::READ_RESOURCE_SUFFIX));
    if !watch && !read {
        return None;
    }
    let action = if read {
        Some("read")
    } else {
        args.get("action").and_then(Value::as_str)
    }
    .filter(|action| !action.is_empty());
    let uri = args
        .get("uri")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|uri| !uri.is_empty());
    let mut targets = Vec::new();
    if let Some(action) = action {
        targets.push(action.to_string());
    }
    if let Some(uri) = uri {
        targets.push(uri.to_string());
        if let Some(action) = action {
            targets.push(format!("{action} {uri}"));
        }
    }
    if targets.is_empty() {
        targets.push(String::new());
    }
    Some(targets)
}

/// Evaluate every value the call may act on.
///
/// For ordinary tools this is the strict form: every candidate must satisfy the
/// allow list, and any candidate matching a deny rule denies. The resource
/// verbs are the exception: their candidates (action, uri, `action uri`) are
/// alternative representations of the same call, so an allow rule matching ANY
/// candidate suffices (`docs__watch_resource(start)` scopes a start without
/// naming the uri). Deny rules still win if ANY candidate matches.
pub fn decide_call(rules: &Rules, tool: &str, args: &Value) -> Decision {
    let candidates = target_candidates(tool, args);
    let lower = tool.to_ascii_lowercase();
    let resource_tool = lower.ends_with(&format!("__{}", kiqi_mcp::WATCH_RESOURCE_SUFFIX))
        || lower.ends_with(&format!("__{}", kiqi_mcp::READ_RESOURCE_SUFFIX));
    if !resource_tool {
        for target in candidates {
            if let Decision::Deny { reason } = decide(rules, tool, &target) {
                return Decision::Deny { reason };
            }
        }
        return Decision::Allow;
    }
    for target in &candidates {
        if let Some(rule) = rules
            .deny
            .iter()
            .find(|rule| rule_matches(rule, tool, target))
        {
            return Decision::Deny {
                reason: format!(
                    "permission denied: tool `{tool}` matches deny rule `{rule}`{}",
                    target_note(target)
                ),
            };
        }
    }
    let has_allow_for_tool = rules.allow.iter().any(|rule| {
        let (rule_tool, _) = split_rule(rule);
        rule_tool == "*" || rule_tool.eq_ignore_ascii_case(tool)
    });
    if has_allow_for_tool
        && !candidates.iter().any(|target| {
            rules
                .allow
                .iter()
                .any(|rule| rule_matches(rule, tool, target))
        })
    {
        let target = candidates.first().map(String::as_str).unwrap_or("");
        return Decision::Deny {
            reason: format!(
                "permission denied: tool `{tool}` is not matched by an allow rule{}",
                target_note(target)
            ),
        };
    }
    Decision::Allow
}

fn normalize_path(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Resolve the spellings a tool accepts but the gate would otherwise see
/// literally: `write {"path":"@x"}` strips the attachment marker, and the OS
/// env expands `~` and `file://` before touching disk.
fn resolve_tool_path(cwd: &Path, raw: &str) -> PathBuf {
    let mut p = raw.trim().to_string();
    if let Some(rest) = p.strip_prefix('@') {
        p = rest.to_string();
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    if let Some(home) = home {
        let home = home.to_string_lossy();
        if p == "~" {
            p = home.into_owned();
        } else if let Some(rest) = p.strip_prefix("~/") {
            p = format!("{home}/{rest}");
        } else if cfg!(windows) {
            if let Some(rest) = p.strip_prefix("~\\") {
                p = format!("{home}\\{rest}");
            }
        }
    }
    if let Some(rest) = p.strip_prefix("file://") {
        // `file:///etc/shadow` and `file://localhost/etc/shadow` both name a
        // local path; percent-encoding is left for the OS to reject.
        p = rest.strip_prefix("localhost").unwrap_or(rest).to_string();
    }
    normalize_path(cwd, Path::new(&p))
}

/// Canonical form for comparing two spellings of a file. `canonicalize` fails
/// for a path that does not exist yet (a `write` that would create it), so the
/// parent is canonicalized instead and the file name is re-attached.
fn comparable_path(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Some(canonical);
    }
    let parent = std::fs::canonicalize(path.parent()?).ok()?;
    Some(parent.join(path.file_name()?))
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    // Default macOS and Windows filesystems are case-insensitive, so
    // `.KI/permissions.json` reaches the same file as `.ki/permissions.json`.
    #[cfg(any(target_os = "macos", windows))]
    {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

/// Files whose rules the gate reads. They are not writable through the
/// model's file or shell tools: otherwise a denied call can restore itself by
/// rewriting the policy that denies it.
fn policy_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        local_path(cwd),
        cwd.join(".ki").join("permissions.json"),
        cwd.join(".pi").join("permissions.json"),
        global_path(),
    ];
    paths.sort();
    paths.dedup();
    paths
}

fn targets_policy_file(cwd: &Path, raw: &str) -> bool {
    let candidate = resolve_tool_path(cwd, raw);
    let comparable_candidate = comparable_path(&candidate);
    policy_paths(cwd).iter().any(|path| {
        let policy = resolve_tool_path(cwd, &path.to_string_lossy());
        if paths_equal(&candidate, &policy) {
            return true;
        }
        match (comparable_candidate.as_deref(), comparable_path(&policy)) {
            (Some(a), Some(b)) => paths_equal(a, &b),
            _ => false,
        }
    })
}

/// Collapse every shell-glob metacharacter cluster to a single `*`.
///
/// `glob_matches` lets `*` cross path separators, so the collapse can only make
/// the pattern match *more*, never miss a spelling the original would have hit.
fn collapse_glob_metacharacters(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for ch in pattern.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '{' | '}') {
            if !out.ends_with('*') {
                out.push('*');
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// How far a shell word may reach into a policy path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PolicyReach {
    /// The word itself and every ancestor: deleting `.ki` or the project root
    /// destroys the policy inside it.
    Ancestor,
    /// Only a spelling that names the policy file itself (or a glob that can
    /// expand to it). Writing *into* a directory does not destroy it, so
    /// `cp notes.txt .` must stay allowed even though `.` is an ancestor.
    FileOnly,
}

/// Whether one shell word names a policy file, the directory that holds one, or
/// a glob that can spell either.
///
/// `project` anchors [`policy_paths`] (the gate's policy files); `base` is the
/// directory the word resolves against, which follows `cd`/`pushd`. With
/// [`PolicyReach::Ancestor`] a hit is the word resolving to the file or any of
/// its ancestors — that covers the file itself, its `.ki`/`.pi` directory, the
/// project root, the home directory and `/` — so `rm -rf .ki` and `rm -rf .`
/// are caught while `rm -rf .ki/skills/old` (a sibling, not an ancestor) stays
/// allowed. [`PolicyReach::FileOnly`] is for destinations, where an ancestor
/// such as `.` only means "write inside this directory".
fn shell_word_targets_policy(project: &Path, base: &Path, raw: &str, reach: PolicyReach) -> bool {
    let candidate = resolve_tool_path(base, raw)
        .to_string_lossy()
        .to_ascii_lowercase();
    if candidate.is_empty() {
        return false;
    }
    let wildcard = candidate.contains(['*', '?', '[', ']', '{', '}']);
    let pattern = if wildcard {
        collapse_glob_metacharacters(&candidate)
    } else {
        candidate
    };
    policy_paths(project).iter().any(|path| {
        let policy = resolve_tool_path(project, &path.to_string_lossy())
            .to_string_lossy()
            .to_ascii_lowercase();
        if wildcard {
            // A glob is compared against the policy path itself (and, for
            // deletion, its ancestors). `*` crosses separators, so both
            // `.ki/permissions.*` and `.ki/*` hit the file.
            let hit = glob_matches(&pattern, &policy);
            let ancestor_hit = reach == PolicyReach::Ancestor
                && ancestors(&policy).any(|value| glob_matches(&pattern, value));
            return hit || ancestor_hit;
        }
        if reach == PolicyReach::FileOnly {
            return policy == pattern;
        }
        let hit = ancestors(&policy).any(|value| value == pattern);
        hit
    })
}

/// `value` followed by each of its parents, down to `/`.
fn ancestors(value: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(value), |value| {
        Path::new(*value).parent().and_then(Path::to_str)
    })
}

/// Split a shell command at unquoted `;`, `&`, `|`, `(`, `)` and newlines, then
/// each segment into words at unquoted whitespace. Quotes are stripped; on Unix
/// a backslash escapes the next character (on Windows a backslash is a path
/// separator, and PowerShell escapes with a backtick, which is left in place).
///
/// This is lexical only: variables, command substitution and interpreter code
/// are opaque to it (see the residual-risk note in [`policy_write_reason`]).
fn shell_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = vec![Vec::new()];
    let mut word = String::new();
    let mut has_word = false;
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            } else if ch == '\\' && delimiter == '"' && !cfg!(windows) {
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            } else {
                word.push(ch);
            }
            has_word = true;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                has_word = true;
            }
            '\\' if !cfg!(windows) => {
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
                has_word = true;
            }
            ';' | '&' | '|' | '(' | ')' | '\n' | '\r' => {
                if has_word {
                    segments
                        .last_mut()
                        .expect("one segment is always present")
                        .push(std::mem::take(&mut word));
                    has_word = false;
                }
                segments.push(Vec::new());
            }
            c if c.is_whitespace() => {
                if has_word {
                    segments
                        .last_mut()
                        .expect("one segment is always present")
                        .push(std::mem::take(&mut word));
                    has_word = false;
                }
            }
            c => {
                word.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        segments
            .last_mut()
            .expect("one segment is always present")
            .push(word);
    }
    segments.retain(|segment| !segment.is_empty());
    segments
}

/// A redirection operator (`>`, `>>`, `2>`, `&>` …).
fn is_redirection(word: &str) -> bool {
    word.contains('>') && word.chars().all(|c| matches!(c, '>' | '&' | '1' | '2'))
}

/// Separate a segment's words from any redirection targets. Targets are
/// candidates whatever the command is, because `cmd > .ki/permissions.json`
/// truncates the policy just as surely as `rm` would.
fn split_redirects(segment: &[String]) -> (Vec<String>, Vec<String>) {
    let mut words = Vec::new();
    let mut targets = Vec::new();
    let mut index = 0;
    while index < segment.len() {
        let word = &segment[index];
        if is_redirection(word) {
            if let Some(target) = segment.get(index + 1) {
                targets.push(target.clone());
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        // `echo x>.ki/permissions.json`: the target is glued to the word.
        if let Some(open) = word.find('>') {
            let (prefix, rest) = word.split_at(open);
            let target = rest.trim_start_matches(['>', '&']);
            if !prefix.is_empty() {
                words.push(prefix.to_string());
            }
            if !target.is_empty() {
                targets.push(target.to_string());
            }
            index += 1;
            continue;
        }
        words.push(word.clone());
        index += 1;
    }
    (words, targets)
}

/// Path-valued arguments of a word list: options (and `--`) are skipped.
fn path_arguments(words: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut options_done = false;
    for word in words {
        if !options_done {
            if word == "--" {
                options_done = true;
                continue;
            }
            if word.starts_with('-') && word.len() > 1 {
                continue;
            }
        }
        out.push(word);
    }
    out
}

/// Commands whose first non-option arguments name another command. The guard
/// must look through them; otherwise `sudo rm -rf .ki` would read as "sudo".
const COMMAND_WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "command", "nice", "nohup", "time", "xargs", "busybox", "exec", "stdbuf",
];

/// A leading `NAME=value` environment assignment.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Wrapper options that consume the following word. Options spelled
/// `--name=value` carry their value inline.
fn wrapper_option_takes_value(wrapper: &str, option: &str) -> bool {
    if option.starts_with("--") || option.contains('=') {
        return false;
    }
    match wrapper {
        "sudo" | "doas" => matches!(option, "-u" | "-g" | "-p" | "-C" | "-r" | "-t" | "-U"),
        "nice" => option == "-n",
        "xargs" => matches!(
            option,
            "-n" | "-P" | "-I" | "-L" | "-s" | "-a" | "-d" | "-E"
        ),
        "env" => option == "-u",
        "time" => option == "-f",
        "stdbuf" => matches!(option, "-i" | "-o" | "-e"),
        _ => false,
    }
}

/// The lowercased file name of a command word: `/bin/rm` and `RM.EXE` both
/// name `rm`.
fn command_name(word: &str) -> String {
    let base = Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
        .to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base).to_string()
}

/// Split `words` into the command it will run and that command's arguments,
/// looking through environment assignments, absolute paths and wrappers:
/// `sudo -u root /bin/rm -rf .ki` yields `("rm", ["-rf", ".ki"])`.
fn unwrap_command(words: &[String]) -> Option<(String, &[String])> {
    let mut rest = words;
    loop {
        while rest.first().is_some_and(|word| is_assignment(word)) {
            rest = &rest[1..];
        }
        let word = rest.first()?;
        let name = command_name(word);
        let after = &rest[1..];
        if !COMMAND_WRAPPERS.contains(&name.as_str()) {
            return Some((name, after));
        }
        // Skip the wrapper's own options and assignments; the first remaining
        // word is the wrapped command.
        let mut index = 0;
        while index < after.len() {
            let word = &after[index];
            if word == "--" {
                index += 1;
                break;
            }
            if is_assignment(word) {
                index += 1;
                continue;
            }
            if word.starts_with('-') && word.len() > 1 {
                index += if wrapper_option_takes_value(&name, word) {
                    2
                } else {
                    1
                };
                continue;
            }
            break;
        }
        rest = after.get(index..).unwrap_or(&[]);
        if rest.is_empty() {
            return None;
        }
    }
}

/// The directory named by `-t DIR` / `--target-directory=DIR`, the spelling
/// that puts a copy/move destination before the sources.
fn target_directory(words: &[String]) -> Option<&str> {
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        if word == "-t" || word == "--target-directory" {
            return words.get(index + 1).map(String::as_str);
        }
        if let Some(value) = word.strip_prefix("--target-directory=") {
            return Some(value);
        }
        index += 1;
    }
    None
}

/// A destructive path argument and how far it may reach into the policy.
struct DestructivePath<'a> {
    path: &'a String,
    reach: PolicyReach,
}

/// The path arguments that can destroy or rewrite the policy for `command`.
///
/// Empty when the command is not destructive. `mv` contributes its sources
/// (moving `.ki` away is as fatal as deleting it) and its destination (moving
/// something *onto* the policy file); `cp`/`install`/`ln` contribute only the
/// destination; `find` contributes its roots only when it will `-delete` or
/// `-exec`; `dd` contributes its `of=`. Destinations are
/// [`PolicyReach::FileOnly`]: copying or linking *into* a directory cannot
/// modify the policy inside it.
fn destructive_paths<'a>(command: &str, words: &'a [String]) -> Vec<DestructivePath<'a>> {
    let all = |reach: PolicyReach| -> Vec<DestructivePath<'a>> {
        path_arguments(words)
            .into_iter()
            .map(|path| DestructivePath { path, reach })
            .collect()
    };
    let only_last = |reach: PolicyReach| -> Vec<DestructivePath<'a>> {
        path_arguments(words)
            .into_iter()
            .last()
            .map(|path| DestructivePath { path, reach })
            .into_iter()
            .collect()
    };
    match command {
        "rm" | "rmdir" | "unlink" | "shred" | "truncate" | "chmod" | "chown" | "chgrp" | "del"
        | "erase" | "rd" | "remove-item" => all(PolicyReach::Ancestor),
        "mv" => {
            // Sources may be `.ki` itself; only the destination is a write
            // target.
            if let Some(target) = target_directory(words) {
                // `mv -t DIR src…`: DIR is a directory (it cannot be the
                // policy file), so every other path argument is a source.
                let mut paths = all(PolicyReach::Ancestor);
                paths.retain(|path| path.path.as_str() != target);
                paths
            } else {
                let mut paths = all(PolicyReach::Ancestor);
                if let Some(last) = paths.last_mut() {
                    last.reach = PolicyReach::FileOnly;
                }
                paths
            }
        }
        "cp" | "install" | "ln" => only_last(PolicyReach::FileOnly),
        "find" => {
            let destructive = words
                .iter()
                .any(|word| word == "-delete" || word.starts_with("-exec"));
            if destructive {
                words
                    .iter()
                    .take_while(|word| !word.starts_with('-') && *word != "!" && *word != "(")
                    .map(|path| DestructivePath {
                        path,
                        reach: PolicyReach::Ancestor,
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        "dd" => words
            .iter()
            .filter(|word| word.starts_with("of="))
            .map(|path| DestructivePath {
                path,
                reach: PolicyReach::Ancestor,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a shell command can reach the permission policy through a literal,
/// directory or glob spelling. Strictly additive over the basename substring
/// check: it only ever adds denials.
fn shell_targets_policy_paths(project: &Path, command: &str) -> bool {
    let mut base = project.to_path_buf();
    for segment in shell_segments(command) {
        let (words, redirects) = split_redirects(&segment);
        if redirects
            .iter()
            .any(|target| shell_word_targets_policy(project, &base, target, PolicyReach::Ancestor))
        {
            return true;
        }
        let Some((command, rest)) = unwrap_command(&words) else {
            continue;
        };
        if matches!(command.as_str(), "cd" | "pushd") {
            if let Some(target) = path_arguments(rest).first() {
                base = resolve_tool_path(&base, target);
            }
            continue;
        }
        if destructive_paths(&command, rest)
            .iter()
            .any(|target| shell_word_targets_policy(project, &base, target.path, target.reach))
        {
            return true;
        }
    }
    false
}

fn policy_write_reason(cwd: &Path, tool: &str, args: &Value) -> Option<String> {
    let tool = tool.to_ascii_lowercase();
    if matches!(tool.as_str(), "write" | "edit") {
        let path = args.get("path").and_then(Value::as_str)?;
        if targets_policy_file(cwd, path) {
            return Some(format!(
                "permission denied: `{path}` is a permission policy file; use the `permissions` tool (grant/revoke) or edit it outside the session"
            ));
        }
        return None;
    }
    if matches!(tool.as_str(), "bash" | "powershell") {
        let command = args.get("command").and_then(Value::as_str)?;
        let denied = || {
            "permission denied: the shell may not touch permission policy files; use the `permissions` tool (grant/revoke) or edit them outside the session".to_string()
        };
        // A shell command can reach the file without naming a path the host
        // can resolve, so match the basename rather than trying to classify
        // the command. Case-insensitive: default macOS/Windows filesystems are,
        // so `PERMISSIONS.JSON` names the same file.
        if command.to_ascii_lowercase().contains("permissions.json") {
            return Some(denied());
        }
        // The basename check misses the directory and glob spellings that
        // destroy or rewrite the same file: `rm -rf .ki`, `rm -rf .`, a glob
        // like `rm .ki/permissions.*`, `mv .ki /tmp`, `cd .ki && rm perm*`,
        // `find .ki -delete`, or a redirection into the policy. `shell_*`
        // below resolves each destructive word against its directories (it
        // follows `cd`/`pushd`, strips `NAME=value` prefixes, looks through
        // `sudo`/`env`/`xargs`-style wrappers and absolute command paths) and,
        // for rm-class words, matches any ancestor of a policy path.
        // Destinations (`cp`/`install`/`ln`/`mv`) match only the policy file
        // itself, because writing into a directory does not destroy it.
        //
        // Still best-effort, and deliberately not sound: variable indirection
        // (`d=.ki; rm -rf $d`), command substitution/`eval`, interpreter
        // one-liners (`python -c 'shutil.rmtree(".ki")'`), scripts invoked by
        // name, make/npm lifecycle hooks, `git clean -fdx` (`.ki` is
        // gitignored) and PowerShell backtick escaping all get through. The
        // real backstop is sandboxing the shell or restoring the policy after
        // the call (`after_tool_call`), both larger than this guard.
        if shell_targets_policy_paths(cwd, command) {
            return Some(denied());
        }
    }
    None
}

/// The host gate used by `before_tool_call`: `None` permits, `Some` blocks.
///
/// A policy that exists but cannot be read or parsed blocks too. Silently
/// ignoring it would drop a restriction the user believes is in force.
pub fn blocking_reason(cwd: &Path, tool: &str, args: &Value) -> Option<String> {
    blocking_reason_for(cwd, true, tool, args)
}

/// [`blocking_reason`] with project trust made explicit.
pub fn blocking_reason_for(
    cwd: &Path,
    project_trusted: bool,
    tool: &str,
    args: &Value,
) -> Option<String> {
    let path = effective_path_for(cwd, project_trusted);
    let rules = match load(&path) {
        Ok(rules) => rules,
        Err(error) => {
            // Repair path: a malformed file must not brick the session. Reads
            // stay available to inspect it, and write/edit may replace the
            // policy file itself; every other call fails closed.
            let repairable = match tool.to_ascii_lowercase().as_str() {
                "read" => true,
                "write" | "edit" => args
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|path| targets_policy_file(cwd, path)),
                _ => false,
            };
            if repairable {
                return None;
            }
            return Some(format!(
                "permission policy {} could not be loaded: {error} (a `write`/`edit` of that file, or `read`, can repair it)",
                path.display()
            ));
        }
    };
    if let Some(reason) = policy_write_reason(cwd, tool, args) {
        return Some(reason);
    }
    match decide_call(&rules, tool, args) {
        Decision::Allow => None,
        Decision::Deny { reason } => Some(format!("{reason} [policy: {}]", path.display())),
    }
}

fn project_root(default_cwd: &Path, params: &Value) -> PathBuf {
    params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_cwd.to_path_buf())
}

/// The `permissions` tool's actions, shared with the compiled plugin's
/// semantics: `check`, `grant`, `revoke`, `list`.
pub fn permissions(default_cwd: &Path, p: &Value) -> Result<String, String> {
    let _rules_guard = RULES_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let local = local_path(&project_root(default_cwd, p));
    let file = if local.exists() {
        local.clone()
    } else {
        global_path()
    };
    let mut rules = load(&file)?;
    let action = p.get("action").and_then(Value::as_str).unwrap_or("list");
    match action {
        "list" => Ok(
            json!({"action":"list","rules":{"allow":rules.allow,"deny":rules.deny},"path":file})
                .to_string(),
        ),
        "check" => {
            let tool = p.get("tool").and_then(Value::as_str).unwrap_or("*");
            let target = p
                .get("value")
                .or_else(|| p.get("capability"))
                .and_then(Value::as_str)
                .ok_or("value (or capability) is required")?;
            let denied = rules.deny.iter().any(|r| rule_matches(r, tool, target));
            let has_allow_for_tool = rules
                .allow
                .iter()
                .any(|r| split_rule(r).0 == "*" || split_rule(r).0.eq_ignore_ascii_case(tool));
            let allowed = !denied
                && (!has_allow_for_tool
                    || rules.allow.iter().any(|r| rule_matches(r, tool, target)));
            Ok(json!({"action":"check","tool":tool,"value":target,"allowed":allowed,"reason":if denied{"deny rule"}else if !allowed{"not in allow rules"}else{"allowed"},"default":"allow-unless-allow-rules-exist"}).to_string())
        }
        "grant" | "revoke" => {
            let capability = p
                .get("rule")
                .or_else(|| p.get("capability"))
                .and_then(Value::as_str)
                .ok_or("capability is required")?
                .trim();
            if capability.is_empty() || capability.len() > 200 {
                return Err("capability must contain 1-200 characters".into());
            }
            let effect = p.get("effect").and_then(Value::as_str).unwrap_or("allow");
            if effect != "allow" && effect != "deny" {
                return Err("effect must be allow or deny".into());
            }
            rules.allow.retain(|r| r != capability);
            rules.deny.retain(|r| r != capability);
            if action == "grant" {
                if effect == "allow" {
                    rules.allow.push(capability.to_owned());
                } else {
                    rules.deny.push(capability.to_owned());
                }
            }
            // Write back to the file the rules came from: revoking a global
            // rule must not leave it in force in every other project while
            // silently copying the whole global ruleset into this project
            // (where it would shadow all future global edits). Only when no
            // file exists yet is the new ruleset project-local.
            let target = if file.exists() {
                file.clone()
            } else {
                local.clone()
            };
            save(&target, &rules)?;
            Ok(
                json!({"action":action,"rule":capability,"effect":effect,"allowed":action=="grant","path":target})
                    .to_string(),
            )
        }
        _ => Err("action must be one of: check, grant, revoke, list".into()),
    }
}

const SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["check","grant","revoke","list"]},"tool":{"type":"string","description":"Pi tool name, such as Bash, Read, Write, or Edit"},"value":{"type":"string","description":"Command or path to evaluate"},"rule":{"type":"string","description":"Rule such as Bash(git push *)"},"capability":{"type":"string","description":"Legacy alias for value/rule"},"effect":{"type":"string","enum":["allow","deny"]},"cwd":{"type":"string"}},"required":["action"]}"#;

/// Host-built-in `permissions` tool.
///
/// It has the same name and semantics as the compiled plugin's tool. Both can
/// be present: `build_tools` installs this one as a built-in and the compiled
/// plugin's registry merge replaces a same-named trusted tool, so the compiled
/// plugin keeps taking precedence when its feature is enabled.
pub struct PermissionsTool {
    schema: Tool,
    cwd: PathBuf,
}

impl PermissionsTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            schema: Tool {
                name: "permissions".to_string(),
                description: "Check and manage ki's host allow/deny rules for tool calls."
                    .to_string(),
                parameters: kiqi_ai::types::Schema::new(
                    serde_json::from_str(SCHEMA).expect("permissions schema is valid JSON"),
                ),
                constrained_sampling: None,
            },
        }
    }
}

pub fn create_permissions_tool(cwd: impl Into<PathBuf>) -> Arc<dyn AgentTool> {
    Arc::new(PermissionsTool::new(cwd))
}

#[async_trait]
impl AgentTool for PermissionsTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "permissions"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if signal.is_cancelled() {
            return Err(AgentError::Tool("permissions cancelled".into()));
        }
        let text = permissions(&self.cwd, &params).map_err(AgentError::Tool)?;
        Ok(AgentToolResult::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kiqi-host-permissions-{}-{nanos}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_project(dir: &Path, text: &str) {
        let file = dir.join(".ki").join("permissions.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    fn result_text(result: &AgentToolResult) -> String {
        match &result.content[0] {
            kiqi_agent::types::TextContentOrImage::Text(text) => text.text.clone(),
            _ => panic!("permissions returned an image"),
        }
    }

    #[test]
    fn generic_allow_lists_still_require_every_candidate() {
        // The any-candidate allow rule is scoped to the resource verbs; an
        // unknown tool must not let a harmless `value` shadow a dangerous
        // `command`.
        let rules = parse_rules(json!({"allow": ["custom(git *)"]})).unwrap();
        let decision = decide_call(
            &rules,
            "custom",
            &json!({"command": "rm -rf /", "value": "git status"}),
        );
        assert!(matches!(decision, Decision::Deny { .. }));
    }

    #[test]
    fn watch_resource_uri_and_action_are_rule_targets() {
        let rules = parse_rules(json!({
            "permissions": { "deny": ["docs__watch_resource(mem://secret)"] }
        }))
        .unwrap();
        assert!(matches!(
            decide_call(
                &rules,
                "docs__watch_resource",
                &json!({ "action": "start", "uri": "mem://secret" })
            ),
            Decision::Deny { .. }
        ));
        assert_eq!(
            decide_call(
                &rules,
                "docs__watch_resource",
                &json!({ "action": "start", "uri": "mem://public" })
            ),
            Decision::Allow
        );
    }

    #[test]
    fn watch_resource_can_allow_start_but_deny_stop() {
        let rules = parse_rules(json!({
            "permissions": { "allow": ["docs__watch_resource(start)"] }
        }))
        .unwrap();
        assert_eq!(
            decide_call(
                &rules,
                "docs__watch_resource",
                &json!({ "action": "start", "uri": "mem://x" })
            ),
            Decision::Allow
        );
        assert!(matches!(
            decide_call(
                &rules,
                "docs__watch_resource",
                &json!({ "action": "stop", "uri": "mem://x" })
            ),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn read_resource_uri_is_a_rule_target() {
        let rules = parse_rules(json!({
            "permissions": { "deny": ["docs__read_resource(file:///secret)"] }
        }))
        .unwrap();
        assert!(matches!(
            decide_call(
                &rules,
                "docs__read_resource",
                &json!({ "uri": "file:///secret" })
            ),
            Decision::Deny { .. }
        ));
        assert_eq!(
            decide_call(
                &rules,
                "docs__read_resource",
                &json!({ "uri": "file:///public" })
            ),
            Decision::Allow
        );
    }

    #[test]
    fn default_allows_without_allow_list() {
        let rules = Rules::default();
        // With no allow list, native pi-permissions permits unspecified calls;
        // an explicit deny rule is what blocks a dangerous operation.
        assert_eq!(decide(&rules, "Bash", "rm -rf /"), Decision::Allow);
    }

    #[test]
    fn deny_rule_wins_over_allow() {
        let rules = parse_rules(json!({
            "permissions": {
                "allow": ["Bash(git *)"],
                "deny": ["Bash(git push *)"]
            }
        }))
        .unwrap();
        let decision = decide(&rules, "bash", "git push origin main");
        match decision {
            Decision::Deny { reason } => {
                assert!(reason.contains("Bash(git push *)"), "got {reason}");
                assert!(reason.contains("bash"), "got {reason}");
            }
            Decision::Allow => panic!("deny must win over allow"),
        }
    }

    #[test]
    fn allow_list_restricts_only_its_tool() {
        let rules = parse_rules(json!({"allow": ["Bash(git *)"]})).unwrap();
        assert_eq!(decide(&rules, "bash", "git status"), Decision::Allow);
        assert!(matches!(
            decide(&rules, "bash", "rm -rf /"),
            Decision::Deny { .. }
        ));
        // No allow rule names `read`, so it keeps the default-allow behavior.
        assert_eq!(decide(&rules, "read", "/etc/passwd"), Decision::Allow);
    }

    #[test]
    fn glob_and_rule_semantics_match_the_plugin() {
        assert!(glob_matches("git *", "git push origin main"));
        assert!(glob_matches("*", ""));
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("git *", "npm install"));
        assert_eq!(split_rule("Bash(git push *)"), ("Bash", "git push *"));
        assert_eq!(split_rule("Bash"), ("*", "Bash"));
        assert_eq!(split_rule("*(*)"), ("*", "*"));
        assert!(rule_matches("Bash(git *)", "bash", "git status"));
        assert!(rule_matches("*", "read", "anything"));
    }

    #[test]
    fn parses_nested_bare_and_legacy_shapes() {
        let nested = parse_rules(json!({
            "permissions": {"allow": [" Bash(ls) ", "  "], "deny": ["Bash(rm *)"]}
        }))
        .unwrap();
        assert_eq!(nested.allow, vec!["Bash(ls)"]);
        assert_eq!(nested.deny, vec!["Bash(rm *)"]);

        let bare = parse_rules(json!({"allow": ["Read(*)"]})).unwrap();
        assert_eq!(bare.allow, vec!["Read(*)"]);

        let legacy = parse_rules(json!([
            {"effect": "allow", "capability": "Bash(ls)"},
            {"effect": "deny", "capability": "Bash(rm *)"}
        ]))
        .unwrap();
        assert_eq!(legacy.allow, vec!["Bash(ls)"]);
        assert!(legacy.deny.is_empty());

        assert!(parse_rules(json!("not rules")).is_err());
    }

    #[test]
    fn target_comes_from_the_argument_the_tool_actually_uses() {
        assert_eq!(
            target_candidates("bash", &json!({"command": "ls"})),
            vec!["ls"]
        );
        assert_eq!(
            target_candidates("read", &json!({"path": "a.rs"})),
            vec!["a.rs"]
        );
        // An extra key must not shadow the key the tool reads.
        assert_eq!(
            target_candidates(
                "bash",
                &json!({"value": "git status", "command": "rm -rf /"})
            ),
            vec!["rm -rf /"]
        );
        assert_eq!(
            target_candidates(
                "write",
                &json!({"value": "x", "path": ".ki/permissions.json"})
            ),
            vec![".ki/permissions.json"]
        );
        // Unknown tools fall back to every candidate.
        assert_eq!(
            target_candidates("mcp_thing", &json!({"topic": "guide"})),
            vec![""]
        );
        // The permissions tool exposes its action and the rule it mutates.
        assert_eq!(
            target_candidates("permissions", &json!({"action": "list"})),
            vec!["list"]
        );
        assert_eq!(
            target_candidates(
                "permissions",
                &json!({"action": "revoke", "rule": "Bash(git push *)"})
            ),
            vec!["revoke", "revoke Bash(git push *)"]
        );
    }

    #[test]
    fn extra_argument_keys_cannot_shadow_the_deny_target() {
        let rules = parse_rules(json!({"deny": ["Bash(rm *)"]})).unwrap();
        let decision = decide_call(
            &rules,
            "bash",
            &json!({"command": "rm -rf /", "value": "git status"}),
        );
        assert!(matches!(decision, Decision::Deny { .. }));

        let rules = parse_rules(json!({"allow": ["Bash(git *)"]})).unwrap();
        let decision = decide_call(
            &rules,
            "bash",
            &json!({"command": "rm -rf /", "value": "git status"}),
        );
        assert!(matches!(decision, Decision::Deny { .. }));
    }

    #[test]
    fn gate_matches_the_permissions_tool_by_action_and_rule() {
        let dir = temp_dir("tool-grant");
        write_project(&dir, r#"{"permissions":{"deny":["permissions(grant)"]}}"#);
        let reason = blocking_reason(
            &dir,
            "permissions",
            &json!({"action": "grant", "rule": "Bash(*)"}),
        )
        .expect("grant is gated");
        assert!(reason.contains("permissions(grant)"), "got {reason}");

        let dir2 = temp_dir("tool-revoke");
        write_project(
            &dir2,
            r#"{"permissions":{"deny":["permissions(revoke *)"]}}"#,
        );
        let reason = blocking_reason(
            &dir2,
            "permissions",
            &json!({"action": "revoke", "rule": "Bash(git push *)"}),
        )
        .expect("revoke is gated");
        assert!(reason.contains("permissions(revoke *)"), "got {reason}");
        // A rule scoped to another action leaves list alone.
        assert_eq!(
            blocking_reason(&dir2, "permissions", &json!({"action": "list"})),
            None
        );

        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(dir2).ok();
    }

    #[test]
    fn policy_files_cannot_be_rewritten_by_tools() {
        let dir = temp_dir("protect");
        write_project(&dir, r#"{"permissions":{"deny":["Bash(rm *)"]}}"#);

        let reason = blocking_reason(
            &dir,
            "write",
            &json!({"path": ".ki/permissions.json", "content": "{\"permissions\":{}}"}),
        )
        .expect("write to the policy is blocked");
        assert!(reason.contains("policy file"), "got {reason}");

        let reason = blocking_reason(
            &dir,
            "bash",
            &json!({"command": "echo '{}' > .ki/permissions.json"}),
        )
        .expect("shell write to the policy is blocked");
        assert!(reason.contains("policy"), "got {reason}");

        // Reading stays possible, and an unrelated write is governed by the
        // rules rather than the path guard.
        assert_eq!(
            blocking_reason(&dir, "read", &json!({"path": ".ki/permissions.json"})),
            None
        );
        assert_eq!(
            blocking_reason(
                &dir,
                "write",
                &json!({"path": "src/main.rs", "content": "x"})
            ),
            None
        );

        std::fs::remove_dir_all(dir).ok();
    }

    /// The basename substring check alone lets the shell delete the policy's
    /// directory or glob the file away. An empty policy keeps the rule engine
    /// out of the way so only the path guard can produce the denial.
    #[test]
    fn shell_cannot_delete_the_policy_directory_or_glob_it() {
        let dir = temp_dir("shell-policy");
        write_project(&dir, "{}");
        let bash = |command: &str| blocking_reason(&dir, "bash", &json!({ "command": command }));

        for command in [
            "rm -rf .ki",
            "rm -rf .KI",
            "rm -rf .pi",
            "rm -f .ki/permissions.*",
            "mv .ki /tmp/moved",
            "cd .ki && rm -rf permissions.*",
            "find .ki -delete",
            "rm -rf .",
            "echo '{}' > .ki/permissions.json",
            "rm -rf .ki>/dev/null",
        ] {
            assert!(bash(command).is_some(), "must block: {command}");
        }
        assert!(
            blocking_reason(
                &dir,
                "powershell",
                &json!({ "command": "Remove-Item -Recurse -Force .ki" })
            )
            .is_some(),
            "must block the PowerShell spelling"
        );

        // Siblings of the policy stay reachable; only its ancestors are sealed.
        for command in [
            "ls .ki/skills",
            "rm -rf .ki/skills/old",
            "rm -rf target/debug",
        ] {
            assert_eq!(bash(command), None, "must allow: {command}");
        }

        std::fs::remove_dir_all(dir).ok();
    }

    /// Destinations are write targets, not deletions: copying or linking into
    /// the project root cannot rewrite the policy, while naming the policy file
    /// still can. Wrapper prefixes and absolute command paths must not hide a
    /// destructive verb.
    #[test]
    fn shell_destinations_and_command_prefixes() {
        let dir = temp_dir("shell-prefixes");
        write_project(&dir, "{}");
        let bash = |command: &str| blocking_reason(&dir, "bash", &json!({ "command": command }));

        // Writing *into* `.`/`..`/home is ordinary and must not be blocked.
        for command in [
            "cp notes.txt .",
            "cp -r dist .",
            "cp notes.txt ..",
            "cp notes.txt ./notes-copy.txt",
            "mv notes.txt .",
            "mv file ./moved",
            "mv -t . notes.txt",
            "ln -s target .",
            "install -m 644 file .",
            "sudo ls",
            "env FOO=1 ls",
        ] {
            assert_eq!(bash(command), None, "must allow: {command}");
        }

        // Naming the policy file as the destination still blocks.
        for command in [
            "cp notes.txt .ki/permissions.json",
            "cp -r src .ki/permissions.*",
            "install -m 644 file .ki/permissions.json",
            "ln -sf file .ki/permissions.json",
            "mv file .ki/permissions.json",
            "mv -t /tmp .ki",
            "mv --target-directory=/tmp .ki",
        ] {
            assert!(bash(command).is_some(), "must block: {command}");
        }

        // Assignments, wrappers and absolute paths are plain shell spellings.
        for command in [
            "sudo rm -rf .ki",
            "sudo -u root rm -rf .ki",
            "doas rm -rf .ki",
            "env FOO=1 rm -rf .ki",
            "env -u FOO rm -rf .ki",
            "X=1 Y=2 rm -rf .ki",
            "/bin/rm -rf .ki",
            "nohup /bin/rm -rf .ki",
            "command rm -rf .ki",
            "nice -n 5 rm -rf .ki",
            "time rm -rf .ki",
            "xargs rm -rf .ki",
            "busybox rm -rf .ki",
        ] {
            assert!(bash(command).is_some(), "must block: {command}");
        }

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn local_path_prefers_ki_then_the_legacy_pi_dir() {
        let dir = temp_dir("paths");
        assert_eq!(local_path(&dir), dir.join(".ki").join("permissions.json"));

        std::fs::create_dir_all(dir.join(".pi")).unwrap();
        std::fs::write(dir.join(".pi").join("permissions.json"), "{}").unwrap();
        assert_eq!(local_path(&dir), dir.join(".pi").join("permissions.json"));

        write_project(&dir, "{}");
        assert_eq!(local_path(&dir), dir.join(".ki").join("permissions.json"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn project_rules_win_over_global_rules() {
        let project = temp_dir("project");
        let global = temp_dir("global");
        let project_file = project.join(".ki").join("permissions.json");
        let global_file = global.join("permissions.json");
        write_project(&project, r#"{"permissions":{"deny":["Bash(project *)"]}}"#);
        std::fs::write(
            &global_file,
            r#"{"permissions":{"deny":["Bash(global *)"]}}"#,
        )
        .unwrap();

        let chosen = pick(local_path(&project), global_file.clone());
        assert_eq!(chosen, project_file);
        let rules = load(&chosen).unwrap();
        assert!(matches!(
            decide(&rules, "bash", "project now"),
            Decision::Deny { .. }
        ));
        assert_eq!(decide(&rules, "bash", "global now"), Decision::Allow);

        // With no project file, the global file is the effective one.
        let empty = temp_dir("empty");
        assert_eq!(pick(local_path(&empty), global_file.clone()), global_file);

        std::fs::remove_dir_all(project).ok();
        std::fs::remove_dir_all(global).ok();
        std::fs::remove_dir_all(empty).ok();
    }

    #[test]
    fn gate_re_reads_the_file_on_every_call() {
        let dir = temp_dir("reload");
        write_project(&dir, r#"{"permissions":{}}"#);
        let args = json!({"command": "rm -rf build/out"});
        assert_eq!(blocking_reason(&dir, "bash", &args), None);

        // Simulates an external edit and the effect of `/reload`: the next call
        // sees the new rule because the hook reads the policy per call.
        write_project(&dir, r#"{"permissions":{"deny":["Bash(rm *)"]}}"#);
        let reason = blocking_reason(&dir, "bash", &args).expect("the new rule blocks");
        assert!(reason.contains("Bash(rm *)"), "got {reason}");
        assert!(reason.contains("bash"), "got {reason}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn malformed_policy_blocks_tools_but_leaves_a_repair_path() {
        let dir = temp_dir("broken");
        write_project(&dir, "{ not json");
        let reason = blocking_reason(&dir, "bash", &json!({"command": "echo hi"}))
            .expect("an unreadable policy must fail closed");
        assert!(reason.contains("could not be loaded"), "got {reason}");

        // The agent can inspect and repair the file it must fix.
        assert_eq!(
            blocking_reason(&dir, "read", &json!({"path": "a.rs"})),
            None
        );
        assert_eq!(
            blocking_reason(
                &dir,
                "write",
                &json!({"path": ".ki/permissions.json", "content": "{\"permissions\":{}}"})
            ),
            None
        );
        // A write somewhere else is not a repair.
        assert!(blocking_reason(
            &dir,
            "write",
            &json!({"path": "src/main.rs", "content": "x"})
        )
        .is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn tool_actions_mutate_the_project_file() {
        let dir = temp_dir("tool");
        write_project(&dir, r#"{"permissions":{}}"#);
        let call = |params: Value| {
            let cwd = dir.clone();
            async move {
                let tool = PermissionsTool::new(cwd);
                let result = tool
                    .execute("call", params, CancellationToken::new(), Arc::new(|_| {}))
                    .await
                    .unwrap();
                result_text(&result)
            }
        };

        let granted: Value =
            serde_json::from_str(&call(json!({"action":"grant","rule":"Bash(git *)"})).await)
                .unwrap();
        assert_eq!(granted["action"], "grant");
        assert_eq!(granted["allowed"], true);
        assert_eq!(
            granted["path"],
            dir.join(".ki/permissions.json").to_string_lossy().as_ref()
        );

        let listed: Value = serde_json::from_str(&call(json!({"action":"list"})).await).unwrap();
        assert_eq!(listed["rules"]["allow"], json!(["Bash(git *)"]));

        let checked: Value = serde_json::from_str(
            &call(json!({"action":"check","tool":"Bash","value":"git status"})).await,
        )
        .unwrap();
        assert_eq!(checked["allowed"], true);

        let checked: Value = serde_json::from_str(
            &call(json!({"action":"check","tool":"Bash","value":"rm -rf /"})).await,
        )
        .unwrap();
        assert_eq!(checked["allowed"], false);

        let revoked: Value =
            serde_json::from_str(&call(json!({"action":"revoke","rule":"Bash(git *)"})).await)
                .unwrap();
        assert_eq!(revoked["action"], "revoke");
        let listed: Value = serde_json::from_str(&call(json!({"action":"list"})).await).unwrap();
        assert_eq!(listed["rules"]["allow"], json!([]));

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn tool_grant_creates_the_project_file_when_no_policy_exists() {
        // The global lookup is isolated so a developer's real
        // `~/.ki/agent/permissions.json` cannot receive this write.
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let previous = std::env::var_os("KI_AGENT_DIR");
        let global = temp_dir("global-empty");
        std::env::set_var("KI_AGENT_DIR", &global);

        let project = temp_dir("tool-new");
        let result = permissions(
            &project,
            &json!({"action":"grant","rule":"Read(*)","effect":"deny"}),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            value["path"],
            project
                .join(".ki/permissions.json")
                .to_string_lossy()
                .as_ref()
        );
        let saved = std::fs::read_to_string(project.join(".ki/permissions.json")).unwrap();
        assert!(saved.contains("Read(*)"));

        match previous {
            Some(value) => std::env::set_var("KI_AGENT_DIR", value),
            None => std::env::remove_var("KI_AGENT_DIR"),
        }
        std::fs::remove_dir_all(project).ok();
        std::fs::remove_dir_all(global).ok();
    }

    /// Run `test` with `KI_AGENT_DIR` pointed at `dir`, restoring the previous
    /// value afterwards. Callers hold the env lock.
    fn with_agent_dir<T>(dir: &Path, test: impl FnOnce() -> T) -> T {
        let previous = std::env::var_os("KI_AGENT_DIR");
        std::env::set_var("KI_AGENT_DIR", dir);
        let result = test();
        match previous {
            Some(value) => std::env::set_var("KI_AGENT_DIR", value),
            None => std::env::remove_var("KI_AGENT_DIR"),
        }
        result
    }

    #[test]
    fn untrusted_project_policy_is_not_enforced() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let global = temp_dir("trust-global");
        let project = temp_dir("trust-project");
        let (trusted, untrusted) = with_agent_dir(&global, || {
            write_project(&project, r#"{"permissions":{"deny":["Bash(*)"]}}"#);
            (
                blocking_reason(&project, "bash", &json!({"command": "echo hi"})).is_some(),
                blocking_reason_for(&project, false, "bash", &json!({"command": "echo hi"})),
            )
        });
        assert!(trusted, "a trusted project policy is enforced");
        assert_eq!(untrusted, None, "an untrusted project policy is ignored");
        std::fs::remove_dir_all(project).ok();
        std::fs::remove_dir_all(global).ok();
    }

    #[test]
    fn project_precedence_holds_through_the_gate() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let global = temp_dir("precedence-global");
        let project = temp_dir("precedence-project");
        std::fs::write(
            global.join("permissions.json"),
            r#"{"permissions":{"deny":["Bash(global *)"]}}"#,
        )
        .unwrap();
        let (global_rule, project_rule, effective) = with_agent_dir(&global, || {
            write_project(&project, r#"{"permissions":{"deny":["Bash(project *)"]}}"#);
            (
                blocking_reason(&project, "bash", &json!({"command": "global now"})),
                blocking_reason(&project, "bash", &json!({"command": "project now"})).is_some(),
                effective_path(&project),
            )
        });
        assert_eq!(
            global_rule, None,
            "the project file replaces the global policy"
        );
        assert!(project_rule, "the project rule is enforced");
        assert_eq!(
            effective,
            project.join(".ki").join("permissions.json"),
            "effective_path must pick the project file"
        );
        std::fs::remove_dir_all(project).ok();
        std::fs::remove_dir_all(global).ok();
    }

    #[test]
    fn revoke_edits_the_global_file_when_no_project_policy_exists() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let global = temp_dir("global-revoke");
        std::fs::write(
            global.join("permissions.json"),
            r#"{"permissions":{"deny":["Bash(danger *)"]}}"#,
        )
        .unwrap();
        let project = temp_dir("global-revoke-project");
        let (path, project_file) = with_agent_dir(&global, || {
            let result = permissions(
                &project,
                &json!({"action":"revoke","rule":"Bash(danger *)"}),
            )
            .unwrap();
            let value: Value = serde_json::from_str(&result).unwrap();
            (
                value["path"].as_str().unwrap().to_string(),
                project.join(".ki").join("permissions.json").exists(),
            )
        });
        assert_eq!(Path::new(&path), global.join("permissions.json"));
        assert!(!project_file, "the global policy is edited in place");
        let saved = std::fs::read_to_string(global.join("permissions.json")).unwrap();
        assert!(!saved.contains("Bash(danger *)"), "got {saved}");
        std::fs::remove_dir_all(project).ok();
        std::fs::remove_dir_all(global).ok();
    }

    #[test]
    fn concurrent_grants_do_not_lose_rules() {
        let _guard = crate::config::test_support::env_lock().lock().unwrap();
        let global = temp_dir("parallel-global");
        let project = temp_dir("parallel-project");
        with_agent_dir(&global, || {
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let project = project.clone();
                    std::thread::spawn(move || {
                        permissions(
                            &project,
                            &json!({"action":"grant","rule":format!("Bash(cmd-{i} *)")}),
                        )
                        .unwrap();
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        let rules = load(&project.join(".ki").join("permissions.json")).unwrap();
        for i in 0..8 {
            assert!(
                rules
                    .allow
                    .iter()
                    .any(|rule| rule == &format!("Bash(cmd-{i} *)")),
                "lost rule {i}: {:?}",
                rules.allow
            );
        }
        std::fs::remove_dir_all(project).ok();
        std::fs::remove_dir_all(global).ok();
    }
}
