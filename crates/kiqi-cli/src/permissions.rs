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

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

/// Per-file write generations, bumped by the sanctioned editor (`save`).
///
/// A shell call snapshots a policy and later restores it. If a `permissions`
/// tool call edited the same file in between, restoring the snapshot would undo
/// that sanctioned edit, so the restore skips any path whose generation changed.
static POLICY_VERSIONS: Mutex<Option<HashMap<PathBuf, u64>>> = Mutex::new(None);

fn policy_version(path: &Path) -> u64 {
    POLICY_VERSIONS
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(path).copied()))
        .unwrap_or(0)
}

fn bump_policy_version(path: &Path) {
    if let Ok(mut guard) = POLICY_VERSIONS.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        *map.entry(path.to_path_buf()).or_insert(0) += 1;
    }
}

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
    bump_policy_version(path);
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
fn shell_word_targets_policy(
    policies: &[String],
    base: &Path,
    raw: &str,
    reach: PolicyReach,
) -> bool {
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
    policies.iter().any(|policy| {
        // The directory that directly holds a policy file is a write target
        // of its own: `cp permissions.json .ki` and `mv /tmp/x/* .ki` land at
        // `.ki/permissions.json` and replace it. Any other ancestor (`.`,
        // `..`, the home directory) is a harmless destination.
        let holds_policy = Path::new(policy).parent().is_some_and(|parent| {
            let parent = parent.to_string_lossy();
            if wildcard {
                glob_matches(&pattern, parent.as_ref())
            } else {
                parent.as_ref() == pattern.as_str()
            }
        });
        if wildcard {
            // A glob is compared against the policy path itself (and, for
            // deletion, its ancestors). `*` crosses separators, so both
            // `.ki/permissions.*` and `.ki/*` hit the file.
            let hit = glob_matches(&pattern, policy);
            let dir_hit = reach == PolicyReach::FileOnly && holds_policy;
            let ancestor_hit = reach == PolicyReach::Ancestor
                && ancestors(&policy).any(|value| glob_matches(&pattern, value));
            return hit || dir_hit || ancestor_hit;
        }
        if reach == PolicyReach::FileOnly {
            return *policy == pattern || holds_policy;
        }
        // Bind the result so the iterator (which borrows `policy`) is dropped
        // before `policy` goes out of scope.
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
/// each segment into arguments and redirection targets at unquoted whitespace
/// and unquoted `>`. Quotes are stripped; on Unix a backslash escapes the next
/// character (on Windows a backslash is a path separator, and PowerShell
/// escapes with a backtick, which is left in place).
///
/// Redirections are collected here, in the same pass, because only this loop
/// knows which characters were quoted. Splitting them in a second pass over the
/// finished words made the quotes invisible: `printf '%s -> '` reduced to one
/// word `%s -> `, whose `>` then read as an operator with a blank target, and a
/// blank target resolves to the working directory — an ancestor of the policy —
/// so a harmless `printf` was denied.
///
/// This is lexical only: variables, command substitution and interpreter code
/// are opaque to it (see the residual-risk note in [`policy_write_reason`]).
fn shell_segments(command: &str) -> Vec<ShellSegment> {
    let mut scan = ShellScan::new();
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        if scan.inside_quote(ch, &mut chars) {
            continue;
        }
        match ch {
            '\'' | '"' => {
                scan.quote = Some(ch);
                scan.has_word = true;
            }
            '\\' if !cfg!(windows) => {
                if let Some(escaped) = chars.next() {
                    scan.word.push(escaped);
                }
                scan.has_word = true;
            }
            '>' => scan.operator(),
            ';' | '&' | '|' | '(' | ')' | '\n' | '\r' => scan.end_segment(),
            c if c.is_whitespace() => scan.finish_word(),
            c => {
                scan.word.push(c);
                scan.has_word = true;
            }
        }
    }
    scan.finish_word();
    scan.segments
        .retain(|segment| !segment.words.is_empty() || !segment.redirects.is_empty());
    scan.segments
}

/// One command segment: its argument words and its redirection targets.
///
/// Targets are candidates whatever the command is, because a redirect onto the
/// policy truncates it just as surely as `rm` would.
#[derive(Default)]
struct ShellSegment {
    words: Vec<String>,
    redirects: Vec<String>,
}

/// The parser state behind [`shell_segments`].
#[derive(Default)]
struct ShellScan {
    segments: Vec<ShellSegment>,
    word: String,
    has_word: bool,
    quote: Option<char>,
    /// A `>` has been seen and its target is not known yet, so the next word is
    /// that target rather than an argument.
    awaiting_target: bool,
}

impl ShellScan {
    fn new() -> Self {
        Self {
            segments: vec![ShellSegment::default()],
            ..Self::default()
        }
    }

    /// Handle `ch` while a quote is open, including the escaped character after
    /// a backslash. Returns false when no quote is open.
    fn inside_quote(&mut self, ch: char, chars: &mut std::str::Chars<'_>) -> bool {
        let Some(delimiter) = self.quote else {
            return false;
        };
        if ch == delimiter {
            self.quote = None;
        } else if ch == '\\' && delimiter == '"' && !cfg!(windows) {
            if let Some(escaped) = chars.next() {
                self.word.push(escaped);
            }
        } else {
            self.word.push(ch);
        }
        self.has_word = true;
        true
    }

    /// A `>` outside quotes. `>>`, `2>` and `&>` spell the same operator, so the
    /// extra operator characters are dropped.
    fn operator(&mut self) {
        if self.awaiting_target && !self.has_word {
            return;
        }
        // `hi>x`: the word before the operator is still an argument, while `2>`
        // is a file descriptor, not a word.
        if self.has_word
            && !self
                .word
                .chars()
                .all(|c| matches!(c, '>' | '&' | '1' | '2'))
        {
            let word = std::mem::take(&mut self.word);
            self.segment().words.push(word);
        }
        self.word.clear();
        self.has_word = false;
        self.awaiting_target = true;
    }

    /// A separator ends the operator too: the next segment's first word is a
    /// command, never the previous segment's target.
    fn end_segment(&mut self) {
        self.finish_word();
        self.awaiting_target = false;
        self.segments.push(ShellSegment::default());
    }

    /// Complete the word in progress: a redirection target if an operator is
    /// waiting for one, otherwise an argument.
    ///
    /// Empty and blank words are dropped. They name no path, and resolving them
    /// lands on the working directory — which is an ancestor of every policy
    /// file inside it, so `rm -rf ''` was rejected as if it deleted the project.
    fn finish_word(&mut self) {
        if !self.has_word || self.word.trim().is_empty() {
            self.word.clear();
            self.has_word = false;
            return;
        }
        let word = std::mem::take(&mut self.word);
        self.has_word = false;
        if std::mem::take(&mut self.awaiting_target) {
            self.segment().redirects.push(word);
        } else {
            self.segment().words.push(word);
        }
    }

    fn segment(&mut self) -> &mut ShellSegment {
        self.segments
            .last_mut()
            .expect("one segment is always present")
    }
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
    "sudo", "doas", "env", "command", "nice", "nohup", "time", "xargs", "busybox", "exec",
    "stdbuf", "setsid", "ionice", "timeout", "chroot", "flock",
];

/// Wrappers whose own first positional argument is not the command:
/// `timeout 5 rm …`, `chroot /root rm …` and `flock /tmp/l rm …`.
const WRAPPERS_WITH_LEADING_POSITIONAL: &[&str] = &["chroot", "flock", "timeout"];

/// Shell reserved words that can sit in front of the command in one segment.
/// `shell_segments` splits on `;`, so `then rm …` and `do rm …` keep the
/// keyword; without skipping it the destructive verb stays invisible.
const COMMAND_PREFIX_WORDS: &[&str] = &[
    "!", "{", "if", "then", "elif", "else", "while", "until", "do",
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

/// Whether a long wrapper option takes a separate value (`--user root`).
fn long_option_takes_value(wrapper: &str, option: &str) -> bool {
    match wrapper {
        "sudo" | "doas" => matches!(
            option,
            "--user"
                | "--group"
                | "--prompt"
                | "--close-from"
                | "--chdir"
                | "--host"
                | "--login-class"
                | "--role"
                | "--type"
                | "--other-user"
                | "--command-timeout"
        ),
        "nice" => option == "--adjustment",
        "xargs" => matches!(
            option,
            "--max-args"
                | "--max-procs"
                | "--replace"
                | "--max-lines"
                | "--max-chars"
                | "--arg-file"
                | "--delimiter"
                | "--eof"
                | "--process-slot-var"
        ),
        "env" => matches!(option, "--unset" | "--chdir" | "--split-string"),
        "time" => matches!(option, "--format" | "--output"),
        "stdbuf" => matches!(option, "--input" | "--output" | "--error"),
        "chroot" => matches!(option, "--userspec" | "--groups"),
        "flock" => matches!(option, "--timeout" | "--conflict-exit-code" | "--command"),
        "timeout" => matches!(option, "--signal" | "--kill-after"),
        "ionice" => matches!(option, "--class" | "--classdata" | "--pid" | "--pgid"),
        _ => false,
    }
}

/// Whether a short wrapper option letter takes a value (`-u root`, `-uroot`).
fn short_option_takes_value(wrapper: &str, option: char) -> bool {
    match wrapper {
        "sudo" | "doas" => matches!(
            option,
            'u' | 'g' | 'p' | 'C' | 'r' | 't' | 'U' | 'D' | 'h' | 'R' | 'T'
        ),
        "nice" => option == 'n',
        "xargs" => matches!(option, 'n' | 'P' | 'I' | 'L' | 's' | 'a' | 'd' | 'E'),
        "env" => matches!(option, 'u' | 'C' | 'S'),
        "time" => matches!(option, 'f' | 'o'),
        "stdbuf" => matches!(option, 'i' | 'o' | 'e'),
        "exec" => option == 'a',
        "flock" => matches!(option, 'w' | 'E'),
        "timeout" => matches!(option, 's' | 'k'),
        "ionice" => matches!(option, 'c' | 'n' | 'p' | 'P' | 'u'),
        _ => false,
    }
}

/// How many following words a wrapper option consumes. `-u root` and
/// `-uroot` both name a user, but only the separated spelling leaves a word
/// to skip; clustered short options (`-iu root`) are walked letter by letter.
fn wrapper_option_consumes(wrapper: &str, option: &str) -> usize {
    if option.contains('=') {
        // `--name=value` carries its value inline.
        return 0;
    }
    if option.starts_with("--") {
        return usize::from(long_option_takes_value(wrapper, option));
    }
    let Some(letters) = option.strip_prefix('-') else {
        return 0;
    };
    for (index, letter) in letters.char_indices() {
        if short_option_takes_value(wrapper, letter) {
            // Anything after the value-taking letter is its value.
            return usize::from(index + letter.len_utf8() == letters.len());
        }
    }
    0
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
        // Assignments and shell keywords can precede the command.
        while let Some(word) = rest.first() {
            if is_assignment(word) || COMMAND_PREFIX_WORDS.contains(&word.as_str()) {
                rest = &rest[1..];
            } else {
                break;
            }
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
                index += 1 + wrapper_option_consumes(&name, word);
                continue;
            }
            break;
        }
        if WRAPPERS_WITH_LEADING_POSITIONAL.contains(&name.as_str()) {
            // `timeout 5 cmd`, `chroot /root cmd`: the first positional is
            // the duration or root, not the command.
            index += 1;
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
        if word == "--" {
            break;
        }
        if word == "-t" || word == "--target-directory" {
            return words.get(index + 1).map(String::as_str);
        }
        if let Some(value) = word.strip_prefix("--target-directory=") {
            return Some(value);
        }
        // `-S/--suffix` takes a value that can itself look like `-t`:
        // `mv -S -t .ki dest` renames `.ki`, it does not set a target.
        if word == "-S" || word == "--suffix" {
            index += 2;
            continue;
        }
        if word.starts_with("-S") && word.len() > 2 {
            index += 1;
            continue;
        }
        // Short options accept an attached value: `-t/tmp`.
        if let Some(value) = word.strip_prefix("-t") {
            if !value.is_empty() {
                return Some(value);
            }
        }
        index += 1;
    }
    None
}

/// Whether `sed` was asked to edit its file arguments in place.
fn sed_in_place(words: &[String]) -> bool {
    words.iter().any(|word| {
        word.starts_with("--in-place")
            || (word.starts_with('-')
                && !word.starts_with("--")
                && word.len() > 1
                && word[1..].contains('i'))
    })
}

/// A destructive path argument and how far it may reach into the policy.
struct DestructivePath<'a> {
    path: &'a str,
    reach: PolicyReach,
}

/// The path arguments that can destroy or rewrite the policy for `command`.
///
/// Empty when the command is not destructive. `mv` contributes its sources
/// (moving `.ki` away is as fatal as deleting it) and its destination (moving
/// something *onto* the policy file); `cp`/`install`/`ln` contribute only the
/// destination, including `-t`/`--target-directory`; `sed -i`, `tee` and
/// `rsync` contribute the files they rewrite; `find` contributes its roots
/// only when it will `-delete` or `-exec`; `dd` contributes its `of=`.
/// Destinations are [`PolicyReach::FileOnly`]: only the policy file itself and
/// the directory that directly holds it can be rewritten.
/// `find` is only destructive when it deletes, or runs a command that can
/// write. `-exec grep` must not be treated like `-exec rm`: the former is a
/// common read-only pipeline and was denied whenever its root was an ancestor of
/// the policy. Unknown commands stay destructive; the post-call policy restore
/// is the backstop for read-only spellings that can still write (e.g. an
/// interpreter one-liner).
fn find_exec_is_destructive(words: &[String]) -> bool {
    const READ_ONLY: &[&str] = &[
        "grep", "rg", "wc", "cat", "head", "tail", "ls", "stat", "file", "echo", "printf", "awk",
        "sed", "diff", "cmp", "sort", "uniq", "cut", "tr", "jq", "test", "[",
    ];
    let mut i = 0;
    while i + 1 < words.len() {
        if matches!(words[i].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
            let name = command_name(&words[i + 1]);
            if READ_ONLY.contains(&name.as_str()) {
                // `sed` is read-only unless the exec segment asks for in-place.
                if name == "sed"
                    && words[i..]
                        .iter()
                        .any(|word| word == "-i" || word.starts_with("-i"))
                {
                    return true;
                }
                i += 1;
                continue;
            }
            return true;
        }
        i += 1;
    }
    false
}

fn destructive_paths<'a>(command: &str, words: &'a [String]) -> Vec<DestructivePath<'a>> {
    let all = |reach: PolicyReach| -> Vec<DestructivePath<'a>> {
        path_arguments(words)
            .into_iter()
            .map(|path| DestructivePath {
                path: path.as_str(),
                reach,
            })
            .collect()
    };
    let only_last = |reach: PolicyReach| -> Vec<DestructivePath<'a>> {
        path_arguments(words)
            .into_iter()
            .last()
            .map(|path| DestructivePath {
                path: path.as_str(),
                reach,
            })
            .into_iter()
            .collect()
    };
    match command {
        "rm" | "rmdir" | "unlink" | "shred" | "truncate" | "chmod" | "chown" | "chgrp" | "del"
        | "erase" | "rd" | "remove-item" => all(PolicyReach::Ancestor),
        "mv" => {
            // Sources may be `.ki` itself; the destination is a write target.
            if let Some(target) = target_directory(words) {
                // `mv -t DIR src…`: DIR receives the sources' basenames, so
                // it is a destination like any other.
                let mut paths = all(PolicyReach::Ancestor);
                paths.retain(|path| path.path != target);
                paths.push(DestructivePath {
                    path: target,
                    reach: PolicyReach::FileOnly,
                });
                paths
            } else {
                let mut paths = all(PolicyReach::Ancestor);
                if let Some(last) = paths.last_mut() {
                    last.reach = PolicyReach::FileOnly;
                }
                paths
            }
        }
        "cp" | "install" | "ln" => match target_directory(words) {
            // `cp -t DIR src…`: DIR is the destination; the sources are read.
            Some(target) => vec![DestructivePath {
                path: target,
                reach: PolicyReach::FileOnly,
            }],
            None => only_last(PolicyReach::FileOnly),
        },
        "sed" if sed_in_place(words) => all(PolicyReach::FileOnly),
        "tee" => all(PolicyReach::FileOnly),
        "rsync" => {
            let mut paths = only_last(PolicyReach::FileOnly);
            if words
                .iter()
                .any(|word| word.starts_with("--remove-source-files"))
            {
                // The last path argument is the destination; the ones before
                // it are sources that rsync deletes after copying.
                paths.extend(path_arguments(words).into_iter().rev().skip(1).map(|path| {
                    DestructivePath {
                        path: path.as_str(),
                        reach: PolicyReach::Ancestor,
                    }
                }));
            }
            paths
        }
        "find" => {
            let destructive =
                words.iter().any(|word| word == "-delete") || find_exec_is_destructive(words);
            if destructive {
                // Global options may precede the roots: `find -L .ki -delete`.
                let mut start = 0;
                while start < words.len() {
                    match words[start].as_str() {
                        "-H" | "-L" | "-P" => start += 1,
                        "-D" => start = (start + 2).min(words.len()),
                        word if word.starts_with("-O") => start += 1,
                        _ => break,
                    }
                }
                let mut roots: Vec<DestructivePath<'a>> = words[start..]
                    .iter()
                    .take_while(|word| !word.starts_with('-') && *word != "!" && *word != "(")
                    .map(|path| DestructivePath {
                        path: path.as_str(),
                        reach: PolicyReach::Ancestor,
                    })
                    .collect();
                if roots.is_empty() {
                    // `find -delete` with no starting point defaults to `.`,
                    // which holds the policy.
                    roots.push(DestructivePath {
                        path: ".",
                        reach: PolicyReach::Ancestor,
                    });
                }
                roots
            } else {
                Vec::new()
            }
        }
        "dd" => words
            .iter()
            .filter_map(|word| word.strip_prefix("of="))
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
    let policy_files = policy_paths(project);
    let policies: Vec<String> = policy_files
        .iter()
        .map(|path| {
            resolve_tool_path(project, &path.to_string_lossy())
                .to_string_lossy()
                .to_ascii_lowercase()
        })
        .collect();
    for segment in shell_segments(command) {
        let ShellSegment { words, redirects } = segment;
        if redirects.iter().any(|target| {
            shell_word_targets_policy(&policies, &base, target, PolicyReach::Ancestor)
        }) {
            return true;
        }
        let Some((command, rest)) = unwrap_command(&words) else {
            continue;
        };
        if matches!(command.as_str(), "cd" | "pushd") {
            match path_arguments(rest).first() {
                Some(target) => base = resolve_tool_path(&base, target),
                // A bare `cd` goes to the home directory, not nowhere.
                None if command == "cd" => base = resolve_tool_path(&base, "~"),
                None => {}
            }
            continue;
        }
        if destructive_paths(&command, rest)
            .iter()
            .any(|target| shell_word_targets_policy(&policies, &base, target.path, target.reach))
        {
            return true;
        }
        if recursive_copy_touches_policy(project, &policy_files, &base, &command, rest) {
            return true;
        }
    }
    false
}

/// Whether a recursive copy can rewrite a policy file even though its
/// destination is only an ancestor of that file.
///
/// `cp -r dist .` copies `dist` *into* `.` and is harmless, but
/// `cp -r /tmp/evil/. .` merges the source's contents and writes
/// `.ki/permissions.json`, and `rsync -a --delete build/ .` removes anything
/// the source does not contain. The destination spelling cannot show that, so
/// look for the policy's relative path inside each source tree.
fn recursive_copy_touches_policy(
    project: &Path,
    policy_files: &[PathBuf],
    base: &Path,
    command: &str,
    words: &[String],
) -> bool {
    if !matches!(command, "cp" | "rsync") {
        return false;
    }
    let recursive = words.iter().any(|word| {
        word == "--recursive"
            || word == "--archive"
            || (word.starts_with('-')
                && !word.starts_with("--")
                && word.len() > 1
                && word[1..].chars().any(|c| matches!(c, 'r' | 'R' | 'a')))
    });
    if !recursive {
        return false;
    }
    // `rsync -t` means "preserve times", not `--target-directory`.
    let target = if command == "cp" {
        target_directory(words)
    } else {
        None
    };
    let paths = path_arguments(words);
    let (destination, sources) = match (target, paths.split_last()) {
        (Some(target), _) => (target, paths.as_slice()),
        (None, Some((destination, sources))) => (destination.as_str(), sources),
        (None, None) => return false,
    };
    let destination = resolve_tool_path(base, destination);
    // `rsync --delete` removes destination entries missing from the source,
    // which includes the policy's directory.
    let delete_extras = command == "rsync"
        && words
            .iter()
            .any(|word| word == "--del" || word.starts_with("--delete"));
    policy_files.iter().any(|policy| {
        let policy = resolve_tool_path(project, &policy.to_string_lossy());
        let Ok(relative) = policy.strip_prefix(&destination) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return false;
        }
        if delete_extras {
            return true;
        }
        sources.iter().any(|source| {
            let source = source.as_str();
            // rsync merges a source's contents when the spelling ends in `/`;
            // cp merges for `.` and `/.`.
            let merged = if command == "rsync" {
                source.ends_with('/') || source.ends_with("/.")
            } else {
                source == "." || source.ends_with("/.")
            };
            let source_path = resolve_tool_path(base, source);
            if !source.contains(['*', '?', '[', ']', '{', '}']) {
                if merged {
                    return source_path.join(relative).exists();
                }
                // A literal directory is copied *into* the destination under
                // its own name; only a name on the policy's path can land on
                // it.
                let Some(name) = source_path.file_name().and_then(|name| name.to_str()) else {
                    return false;
                };
                let Ok(rest) = relative.strip_prefix(name) else {
                    return false;
                };
                return source_path.join(rest).exists();
            }
            // A glob can select the policy's directory itself
            // (`cp -r src/.* .`). Shell wildcards never match a leading dot
            // unless the pattern spells one.
            let last = source
                .trim_end_matches('/')
                .trim_end_matches("/.")
                .rsplit('/')
                .next()
                .unwrap_or(source);
            let Some(first) = relative
                .components()
                .next()
                .and_then(|part| part.as_os_str().to_str())
            else {
                return false;
            };
            if first.starts_with('.') && !last.starts_with('.') {
                return false;
            }
            if !last.contains(['*', '?', '[', ']', '{', '}']) {
                // The glob sits in an earlier component; a literal name still
                // has to be on the policy's path.
                return last.eq_ignore_ascii_case(first);
            }
            if !first.starts_with('.') {
                return true;
            }
            let pattern = last.to_ascii_lowercase();
            if !glob_matches(&pattern, &first.to_ascii_lowercase()) {
                return false;
            }
            let root = source
                .rfind('/')
                .map(|slash| resolve_tool_path(base, &source[..slash]))
                .unwrap_or_else(|| base.to_path_buf());
            root.join(relative).exists()
        })
    })
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
        // the call. The host installs a `PolicyGuard` `after_tool_call` hook
        // (session.rs) that restores any policy changed during a shell call;
        // that closes the policy-takeover class, though it does not undo the
        // command's other effects.
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

/// Whether a tool runs an arbitrary shell command.
///
/// The post-call policy backstop snapshots the policy around these tools only:
/// `permissions` is the sanctioned editor, and file tools are already gated by
/// [`policy_write_reason`].
pub fn is_shell_tool(tool: &str) -> bool {
    matches!(tool, "bash" | "powershell")
}

#[derive(Clone, Debug)]
enum SavedPolicy {
    Absent,
    /// The file's bytes, its mode, and - when the policy path was a symlink -
    /// the link's target.
    ///
    /// The target is part of the saved state, not decoration. A user who shares
    /// one policy across checkouts links it in, and comparing only the bytes
    /// through the link made every such file look modified: the old check
    /// required `!is_symlink`, so a symlinked policy never matched its own
    /// snapshot. Every shell call then "restored" it - replacing the link with a
    /// regular file and flagging an untouched command as a policy rewrite.
    Content {
        bytes: Vec<u8>,
        permissions: Option<std::fs::Permissions>,
        link: Option<PathBuf>,
    },
    Unknown,
}

impl SavedPolicy {
    /// The saved state for `path`, read through the path the way the gate reads
    /// it.
    fn capture(path: &Path) -> Self {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
                match std::fs::read(path) {
                    Ok(bytes) => Self::Content {
                        bytes,
                        // `metadata` follows the link, so this is the mode of the
                        // file the policy is read from.
                        permissions: std::fs::metadata(path).ok().map(|m| m.permissions()),
                        // A link whose target cannot be read is not a state this can
                        // restore faithfully, so it degrades to "regular file".
                        link: meta
                            .file_type()
                            .is_symlink()
                            .then(|| std::fs::read_link(path))
                            .and_then(Result::ok),
                    },
                    Err(_) => Self::Unknown,
                }
            }
            Ok(_) => Self::Unknown,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Absent,
            Err(_) => Self::Unknown,
        }
    }

    /// Whether `path` is still in the saved state.
    ///
    /// A path that was a symlink is unchanged only while it is still a symlink
    /// to the same target (`read` follows the link, so the bytes are compared
    /// through it either way).
    fn matches(&self, path: &Path) -> bool {
        let is_symlink = std::fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
        match self {
            Self::Unknown => true,
            Self::Absent => {
                !is_symlink
                    && std::fs::symlink_metadata(path)
                        .map(|meta| !meta.is_dir())
                        .unwrap_or(true)
                    && std::fs::read(path).is_err()
            }
            Self::Content { bytes, link, .. } => match link {
                Some(target) => {
                    is_symlink
                        && std::fs::read_link(path).ok().as_deref() == Some(target.as_path())
                        && std::fs::read(path).ok().as_deref() == Some(bytes.as_slice())
                }
                None => {
                    !is_symlink && std::fs::read(path).ok().as_deref() == Some(bytes.as_slice())
                }
            },
        }
    }
}

/// Create `link` pointing at `target`, the way the platform spells it.
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// A snapshot of every policy file the gate can read.
///
/// The shell guard is a hand-maintained lexical classifier and cannot cover
/// every spelling. This is the backstop: capture the policy files before a
/// shell call, restore any change after it returns, and flag the result. It
/// does not undo the command's other effects, and a background process that
/// writes after the call returns is still out of scope.
#[derive(Clone, Debug)]
pub struct PolicySnapshot {
    files: Vec<(PathBuf, SavedPolicy, u64)>,
}

impl PolicySnapshot {
    pub fn capture(cwd: &Path) -> Self {
        Self::capture_paths(policy_paths(cwd))
    }

    fn capture_paths(paths: Vec<PathBuf>) -> Self {
        let mut files: Vec<(PathBuf, SavedPolicy, u64)> = Vec::new();
        for path in paths {
            if files.iter().any(|(seen, _, _)| seen == &path) {
                continue;
            }
            let state = SavedPolicy::capture(&path);
            let version = policy_version(&path);
            files.push((path, state, version));
        }
        Self { files }
    }

    /// Restore every changed path. `Ok(None)` means nothing changed.
    pub fn restore_if_changed(&self) -> Result<Option<Vec<PathBuf>>, String> {
        let mut changed = Vec::new();
        for (path, saved, version) in &self.files {
            // A sanctioned `permissions` edit after the snapshot wins: restoring
            // the old bytes would silently undo the user's own change.
            if policy_version(path) != *version {
                continue;
            }
            if saved.matches(path) {
                continue;
            }
            match saved {
                SavedPolicy::Unknown => continue,
                SavedPolicy::Absent => match std::fs::symlink_metadata(path) {
                    Ok(meta) if meta.is_dir() => {
                        return Err(format!(
                            "policy path {} became a directory; refusing to remove it",
                            path.display()
                        ))
                    }
                    Ok(_) => std::fs::remove_file(path).map_err(|error| {
                        format!("could not remove new policy {}: {error}", path.display())
                    })?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "could not inspect policy {}: {error}",
                            path.display()
                        ))
                    }
                },
                SavedPolicy::Content {
                    bytes,
                    permissions,
                    link,
                } => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| {
                            format!("could not recreate {}: {error}", parent.display())
                        })?;
                    }
                    // Put the link back before writing: writing through it then
                    // restores the target's bytes, which is where the gate reads
                    // the policy from. Writing a regular file at the path instead
                    // would silently detach the user's shared policy.
                    if let Some(target) = link {
                        let intact = std::fs::read_link(path)
                            .map(|current| &current == target)
                            .unwrap_or(false);
                        if !intact {
                            if let Ok(meta) = std::fs::symlink_metadata(path) {
                                if meta.is_dir() {
                                    return Err(format!(
                                        "policy path {} became a directory; refusing to overwrite it",
                                        path.display()
                                    ));
                                }
                                std::fs::remove_file(path).map_err(|error| {
                                    format!(
                                        "could not remove {} to restore its link: {error}",
                                        path.display()
                                    )
                                })?;
                            }
                            if let Err(error) = symlink(target, path) {
                                return Err(format!(
                                    "could not restore the link at {}: {error}",
                                    path.display()
                                ));
                            }
                        }
                    }
                    if let Ok(meta) = std::fs::symlink_metadata(path) {
                        if meta.is_dir() {
                            return Err(format!(
                                "policy path {} became a directory; refusing to overwrite it",
                                path.display()
                            ));
                        }
                        if meta.file_type().is_symlink() && link.is_none() {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                    std::fs::write(path, bytes).map_err(|error| {
                        format!("could not restore policy {}: {error}", path.display())
                    })?;
                    if let Some(permissions) = permissions {
                        let _ = std::fs::set_permissions(path, permissions.clone());
                    }
                }
            }
            changed.push(path.clone());
        }
        Ok(if changed.is_empty() {
            None
        } else {
            Some(changed)
        })
    }
}

/// Snapshots policy files around shell calls so a lexical bypass cannot change
/// the rules for later calls.
///
/// `guarded_before` wraps the host gate; it captures only after the gate allows
/// a shell call. `after_hook` restores and flags a changed policy. Both share
/// one map keyed by tool-call id, so parallel calls do not confuse snapshots.
#[derive(Clone, Default)]
pub struct PolicyGuard {
    cwd: PathBuf,
    snapshots: Arc<Mutex<HashMap<String, PolicySnapshot>>>,
    /// Last trusted policy state. Checked before every tool call so a
    /// background writer between calls is restored before the next gate reads
    /// the rules; a sanctioned `permissions` edit becomes the new baseline.
    baseline: Arc<Mutex<Option<PolicySnapshot>>>,
}

impl PolicyGuard {
    pub fn new(cwd: PathBuf) -> Self {
        let baseline = PolicySnapshot::capture(&cwd);
        Self {
            cwd,
            snapshots: Arc::new(Mutex::new(HashMap::new())),
            baseline: Arc::new(Mutex::new(Some(baseline))),
        }
    }

    /// Restore a policy changed between calls and refresh the trusted baseline.
    fn check_baseline(&self) -> Option<Vec<PathBuf>> {
        let mut baseline = self.baseline.lock().ok()?;
        let snapshot = baseline.as_ref()?;
        let changed = match snapshot.restore_if_changed() {
            Ok(changed) => changed,
            Err(error) => {
                tracing::warn!(%error, "could not restore a policy changed between tool calls");
                return None;
            }
        };
        // Refresh after the check: a sanctioned edit bumps the file version, so
        // `restore_if_changed` skipped it and the new bytes are now trusted.
        *baseline = Some(PolicySnapshot::capture(&self.cwd));
        changed
    }

    fn refresh_baseline(&self) {
        if let Ok(mut baseline) = self.baseline.lock() {
            *baseline = Some(PolicySnapshot::capture(&self.cwd));
        }
    }

    fn capture(&self, tool_call_id: &str) {
        let snapshot = PolicySnapshot::capture(&self.cwd);
        if let Ok(mut map) = self.snapshots.lock() {
            map.insert(tool_call_id.to_string(), snapshot);
        }
    }

    /// Wrap the host gate: capture the policy after it allows a shell call.
    pub fn guarded_before(&self, inner: kiqi_agent::BeforeToolCall) -> kiqi_agent::BeforeToolCall {
        let guard = self.clone();
        Arc::new(
            move |ctx: kiqi_agent::BeforeToolCallContext<'_>, signal: CancellationToken| {
                let tool_call_id = ctx.tool_call.id.clone();
                let tool_name = ctx.tool_call.name.clone();
                let future = inner(ctx, signal);
                let guard = guard.clone();
                Box::pin(async move {
                    if let Some(paths) = guard.check_baseline() {
                        let list = paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        tracing::warn!(%list, "restored a policy changed between tool calls");
                    }
                    let result = future.await;
                    let allowed = result.as_ref().is_none_or(|result| !result.block);
                    if allowed && is_shell_tool(&tool_name) {
                        guard.capture(&tool_call_id);
                    }
                    result
                })
            },
        )
    }

    /// Verify and restore the policy after a shell call, flagging the result.
    pub fn after_hook(&self) -> kiqi_agent::AfterToolCall {
        let guard = self.clone();
        Arc::new(
            move |ctx: kiqi_agent::AfterToolCallContext<'_>, _signal: CancellationToken| {
                let tool_call_id = ctx.tool_call.id.clone();
                let mut content = ctx.result.content.clone();
                let snapshot = guard
                    .snapshots
                    .lock()
                    .ok()
                    .and_then(|mut map| map.remove(&tool_call_id));
                let guard_async = guard.clone();
                Box::pin(async move {
                    let snapshot = snapshot?;
                    let outcome = snapshot.restore_if_changed();
                    guard_async.refresh_baseline();
                    let note = match outcome {
                        Ok(None) => return None,
                        Ok(Some(paths)) => {
                            let list = paths
                                .iter()
                                .map(|path| path.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!(
                                "\n\n[permissions] the shell command modified the policy file(s) \
                                 {list}; the previous policy was restored. The command's other \
                                 effects were not undone."
                            )
                        }
                        Err(error) => format!(
                            "\n\n[permissions] the shell command modified a policy file and the \
                             restore failed: {error}. The policy may be compromised."
                        ),
                    };
                    content.push(kiqi_agent::types::TextContentOrImage::text(note));
                    Some(kiqi_agent::AfterToolCallResult {
                        content: Some(content),
                        is_error: Some(true),
                        ..Default::default()
                    })
                })
            },
        )
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

    /// A `>` inside quotes is text, not a redirection operator, and a blank
    /// word names no path.
    ///
    /// Both were false *denials*, and both came from the same shortcut: the
    /// parser stripped quotes first and a second pass looked for `>` in the
    /// finished words. `printf '%s -> '` collapsed to one word `%s -> `, whose
    /// `>` read as an operator with a blank target - and a blank target
    /// resolves to the working directory, an ancestor of every policy file
    /// inside it. `rm -rf ''` reached the same place by a different route: an
    /// empty word, resolved to the project root, read as "delete the project".
    ///
    /// The report that found this was a real harness call:
    /// `printf '%s -> ' hi` was rejected with the policy message.
    #[test]
    fn shell_quotes_hide_operators_and_blank_words_name_nothing() {
        let dir = temp_dir("shell-quoted-operator");
        write_project(&dir, "{}");
        let bash = |command: &str| blocking_reason(&dir, "bash", &json!({ "command": command }));

        // A quoted `>` is just text, whatever surrounds it.
        for command in [
            "printf '%s -> ' hi",
            "printf '%s -> ' hi; echo done",
            "printf '%s -> ' hi > out.txt",
            "echo 'x -> y'",
            "echo 'a > b'",
            "echo 'a > b' > out.txt",
            "printf 'a => b'",
            "grep '->' file",
            "awk '{print $1 -> $2}' f",
            // Quoted text that *spells* a policy directory is still just text:
            // `echo` prints it, nothing is written. This is the spelling that
            // pins quote-awareness rather than the blank-target rule below.
            //
            // (The blunt basename check above `policy_write_reason` still
            // denies anything that literally names the policy file, quoted or
            // not - `echo 'a>.ki/permissions.json'` is rejected. That is a
            // deliberate trade-off recorded on the check, not this test's
            // subject, so these spellings avoid the literal name.)
            "echo 'a>.ki'",
            "grep 'x>.ki' file",
            "printf '%s-> ' hi",
            // A blank word names nothing; it cannot be an ancestor of the
            // policy.
            "rm -rf ''",
            "rm -rf \"\"",
            "mv '' x",
            "cp x ''",
        ] {
            assert_eq!(bash(command), None, "must allow: {command}");
        }

        // The control: a genuine redirect into the policy is still a redirect,
        // including when the same command also carries a quoted `>` earlier.
        for command in [
            "echo hi > .ki/permissions.json",
            "cat x >> .ki/permissions.json",
            "echo hi 2> .ki/permissions.json",
            "echo 'a > b' > .ki/permissions.json",
            "printf '%s -> ' hi > .ki/permissions.json",
            // A separator ends the operator: `rm` here is a command, not the
            // target of the `>` in the previous segment.
            "echo hi > ; rm -rf .ki",
        ] {
            assert!(bash(command).is_some(), "must block: {command}");
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

    /// A destination *directory* that directly holds a policy file still
    /// receives the source's basename, so copying or moving a file called
    /// `permissions.json` there replaces the policy. Ordinary writers and
    /// wrappers must not hide that either.
    #[test]
    fn shell_destination_directories_and_writer_wrappers() {
        let dir = temp_dir("shell-dest-dirs");
        write_project(&dir, "{}");
        let bash = |command: &str| blocking_reason(&dir, "bash", &json!({ "command": command }));

        // A source tree that contains the policy's own relative path, which a
        // recursive copy can merge into an ancestor of the policy.
        let evil = dir.join("evil").join(".ki");
        std::fs::create_dir_all(&evil).unwrap();
        std::fs::write(evil.join("permissions.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("empty")).unwrap();

        for command in [
            // The policy's own directory is a write target, not a sibling.
            "cp notes.txt .ki",
            "cp -r src/. .ki/",
            "cp -t .ki notes.txt",
            "cp --target-directory=.ki notes.txt",
            "install -m 644 file .ki",
            "ln -sf file .ki",
            "mv file .ki",
            "mv -t .ki notes.txt",
            "mv -t.ki notes.txt",
            // Ordinary writers can rewrite the policy through a glob.
            "sed -i 's/x/y/' .ki/perm*.json",
            "tee .ki/perm*.json",
            "rsync -a /tmp/x/ .ki/",
            "rsync -a --remove-source-files .ki /tmp",
            // Wrapper options that take a value must not become the command.
            "sudo --user root rm -rf .ki",
            "sudo -D /tmp rm -rf .ki",
            "sudo -T 5 rm -rf .ki",
            "sudo -iu root rm -rf .ki",
            "sudo -uroot rm -rf .ki",
            "timeout -sKILL 5 rm -rf .ki",
            "xargs -I{} rm -rf .ki",
            "env --unset FOO rm -rf .ki",
            "nice --adjustment 5 rm -rf .ki",
            "xargs --max-args 1 rm -rf .ki",
            // Wrappers with a leading positional (or none at all).
            "timeout 5 rm -rf .ki",
            "chroot /tmp rm -rf .ki",
            "setsid rm -rf .ki",
            "exec -a x rm -rf .ki",
            "flock /tmp/l rm -rf .ki",
            // Shell keywords in front of the command.
            "! rm -rf .ki",
            "if true; then rm -rf .ki; fi",
            "for x in a; do rm -rf .ki; done",
            "{ rm -rf .ki; }",
            // `--` ends option parsing; a later operand is not a target.
            "mv -- -t .ki /tmp/x",
            // `-S` consumes `-t` as its suffix value, so `.ki` is a source.
            "mv -S -t .ki dest",
            // find's global options precede the roots, and no root means `.`.
            "find -L .ki -delete",
            "find -delete",
            // Recursive copies merge source contents into the destination.
            "cp -r evil/. .",
            "cp -r evil/.ki .",
            "rsync -a evil/ .",
            "rsync -a evil/. .",
            "rsync -a --delete empty/ .",
            "rsync -a --del empty/ .",
            // dd's `of=` prefix is not part of the path.
            "dd if=/dev/zero of=.ki/perm*.json",
        ] {
            assert!(bash(command).is_some(), "must block: {command}");
        }

        // The benign destinations the previous commit allowed stay allowed.
        for command in [
            "cp notes.txt .",
            "mv notes.txt .",
            "mv -t . notes.txt",
            "cp notes.txt ..",
            "sudo -n true",
            "xargs -0 echo",
            // A plain directory copy and a non-dot glob cannot reach `.ki`.
            "cp -r empty .",
            "cp -r evil/* .",
            "rsync -a empty/ /tmp/out",
        ] {
            assert_eq!(bash(command), None, "must allow: {command}");
        }

        // A bare `cd` lands in the home directory; a relative `.ki/agent`
        // then names the global policy's directory.
        //
        // The global policy path comes from `KI_AGENT_DIR`/`HOME`
        // ([`global_path`]), which is process-global: `session.rs`'s tests set
        // `KI_AGENT_DIR` to a temp dir under [`crate::config::test_support::env_lock`],
        // and a reader that does not take the same lock sees whichever value
        // happens to be installed. Measured on this tree: this assertion failed
        // in 4 of 10 runs of `permissions::` before the lock was taken here.
        let _global_env = crate::config::test_support::env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            bash("cd && rm -rf .ki/agent").is_some(),
            "must block: cd && rm -rf .ki/agent"
        );

        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn policy_snapshot_restores_content_absence_and_new_files() {
        let dir = temp_dir("policy-snapshot");
        let local = dir.join(".ki").join("permissions.json");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, b"A").unwrap();
        let new = dir.join(".pi").join("permissions.json");
        let snapshot = PolicySnapshot::capture_paths(vec![local.clone(), new.clone()]);

        std::fs::write(&local, b"B").unwrap();
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, b"NEW").unwrap();

        let changed = snapshot.restore_if_changed().unwrap().expect("changed");
        assert!(changed.contains(&local));
        assert!(changed.contains(&new));
        assert_eq!(std::fs::read(&local).unwrap(), b"A");
        assert!(!new.exists());
        assert!(snapshot.restore_if_changed().unwrap().is_none());

        std::fs::remove_dir_all(dir).ok();
    }

    async fn guarded_call(
        guard: &PolicyGuard,
        tool: &str,
        args: Value,
    ) -> (
        kiqi_agent::BeforeToolCall,
        kiqi_ai::types::ToolCall,
        kiqi_ai::types::AssistantMessage,
    ) {
        let inner: kiqi_agent::BeforeToolCall = Arc::new(|_ctx, _signal| Box::pin(async { None }));
        let before = guard.guarded_before(inner);
        let message = kiqi_ai::providers::faux::faux_assistant_message(
            "policy-guard",
            kiqi_ai::types::StopReason::ToolUse,
        );
        let kiqi_ai::types::Content::ToolCall(call) =
            kiqi_ai::types::Content::tool_call("policy-guard-call", tool, args)
        else {
            unreachable!()
        };
        let result = before(
            kiqi_agent::BeforeToolCallContext {
                assistant_message: &message,
                tool_call: &call,
                args: &call.arguments,
                context: &kiqi_agent::AgentContext::default(),
            },
            CancellationToken::new(),
        )
        .await;
        assert!(result.is_none(), "the inner hook allows the call");
        (before, call, message)
    }

    #[tokio::test]
    async fn a_shell_call_that_rewrites_the_policy_is_restored_and_flagged() {
        let dir = temp_dir("policy-guard-shell");
        write_project(&dir, r#"{"permissions":{}}"#);
        let policy = dir.join(".ki").join("permissions.json");
        let guard = PolicyGuard::new(dir.clone());
        let (_before, call, message) =
            guarded_call(&guard, "bash", json!({"command": "echo hi"})).await;

        // Simulate the shell command rewriting the policy behind the lexical guard.
        std::fs::write(&policy, r#"{"permissions":{"allow":["*"]}}"#).unwrap();

        let result = AgentToolResult {
            content: vec![kiqi_agent::types::TextContentOrImage::text("hi")],
            details: Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        };
        let outcome = guard.after_hook()(
            kiqi_agent::AfterToolCallContext {
                assistant_message: &message,
                tool_call: &call,
                args: &call.arguments,
                result: &result,
                is_error: false,
                context: &kiqi_agent::AgentContext::default(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the changed policy is flagged");
        assert_eq!(outcome.is_error, Some(true));
        assert_eq!(
            std::fs::read(&policy).unwrap(),
            br#"{"permissions":{}}"#.as_slice()
        );
        let text = format!("{:?}", outcome.content.unwrap());
        assert!(text.contains("[permissions]"), "{text}");

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn the_permissions_editor_is_not_snapshotted() {
        let dir = temp_dir("policy-guard-editor");
        write_project(&dir, r#"{"permissions":{}}"#);
        let policy = dir.join(".ki").join("permissions.json");
        let guard = PolicyGuard::new(dir.clone());
        let (_before, call, message) =
            guarded_call(&guard, "permissions", json!({"action": "list"})).await;

        // The sanctioned editor is allowed to change the file; the backstop must
        // not flag or restore it.
        std::fs::write(&policy, r#"{"permissions":{"deny":["Bash(rm *)"]}}"#).unwrap();
        let result = AgentToolResult {
            content: vec![kiqi_agent::types::TextContentOrImage::text("ok")],
            details: Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        };
        let outcome = guard.after_hook()(
            kiqi_agent::AfterToolCallContext {
                assistant_message: &message,
                tool_call: &call,
                args: &call.arguments,
                result: &result,
                is_error: false,
                context: &kiqi_agent::AgentContext::default(),
            },
            CancellationToken::new(),
        )
        .await;
        assert!(outcome.is_none(), "the permissions tool is not flagged");
        assert_eq!(
            std::fs::read(&policy).unwrap(),
            br#"{"permissions":{"deny":["Bash(rm *)"]}}"#.as_slice()
        );

        std::fs::remove_dir_all(dir).ok();
    }
    /// A policy the user keeps in one place and links into each checkout.
    ///
    /// The snapshot compared only the bytes read through the path and required
    /// `!is_symlink`, so a symlinked policy never matched its own snapshot: every
    /// shell call "restored" it, which replaced the link with a regular file and
    /// flagged an untouched command (`echo hi`) as a policy rewrite. The user's
    /// shared policy then silently stopped applying to that project.
    ///
    /// The link target is part of the saved state, and a restore recreates the
    /// link before writing, so writing then lands on the target's bytes.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_policy_is_not_flagged_when_nothing_changed() {
        let base = temp_dir("policy-symlink-noop");
        let project = base.join("project");
        std::fs::create_dir_all(project.join(".ki")).unwrap();
        let target = base.join("shared-policy");
        std::fs::write(&target, r#"{"permissions":{"deny":["Bash(rm *)"]}}"#).unwrap();
        let link = project.join(".ki").join("permissions.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let guard = PolicyGuard::new(project.clone());
        let (_before, call, message) =
            guarded_call(&guard, "bash", json!({"command": "echo hi"})).await;
        let outcome = guard.after_hook()(
            kiqi_agent::AfterToolCallContext {
                assistant_message: &message,
                tool_call: &call,
                args: &call.arguments,
                result: &AgentToolResult {
                    content: vec![kiqi_agent::types::TextContentOrImage::text("hi")],
                    details: Value::Null,
                    usage: None,
                    added_tool_names: Vec::new(),
                    terminate: false,
                },
                is_error: false,
                context: &kiqi_agent::AgentContext::default(),
            },
            CancellationToken::new(),
        )
        .await;
        assert!(
            outcome.is_none(),
            "an untouched command must not be flagged, and a symlinked policy must not be replaced"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false),
            "the link must survive a shell call that changed nothing"
        );

        std::fs::remove_dir_all(base).ok();
    }

    /// The protection must not be lost in the process. Each case below is a
    /// rewrite the lexical guard could miss, simulated directly on the file.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_policy_is_still_restored_and_flagged() {
        let original = r#"{"permissions":{"deny":["Bash(rm *)"]}}"#;
        let tampered = r#"{"permissions":{"allow":["*"]}}"#;
        // How the command attacked the policy:
        //   0. wrote the new bytes straight through the link;
        //   1. replaced the link with a regular file holding those bytes;
        //   2. replaced the link with a regular file holding the *same* bytes;
        //   3. repointed the link at another file holding the same bytes.
        // 2 and 3 change nothing about the policy in force, so a byte comparison
        // alone calls them "unchanged" - and then the user's shared policy is
        // silently detached: later edits to the original target stop applying.
        for case in 0..4u8 {
            let base = temp_dir("policy-symlink-restore");
            let project = base.join("project");
            std::fs::create_dir_all(project.join(".ki")).unwrap();
            let target = base.join("shared-policy");
            std::fs::write(&target, original).unwrap();
            let other = base.join("other-policy");
            std::fs::write(&other, original).unwrap();
            let link = project.join(".ki").join("permissions.json");
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let guard = PolicyGuard::new(project.clone());
            let (_before, call, message) =
                guarded_call(&guard, "bash", json!({"command": "echo hi"})).await;
            match case {
                0 => std::fs::write(&link, tampered).unwrap(),
                1 => {
                    std::fs::remove_file(&link).unwrap();
                    std::fs::write(&link, tampered).unwrap();
                }
                2 => {
                    std::fs::remove_file(&link).unwrap();
                    std::fs::write(&link, original).unwrap();
                }
                _ => {
                    std::fs::remove_file(&link).unwrap();
                    std::os::unix::fs::symlink(&other, &link).unwrap();
                }
            }

            let outcome = guard.after_hook()(
                kiqi_agent::AfterToolCallContext {
                    assistant_message: &message,
                    tool_call: &call,
                    args: &call.arguments,
                    result: &AgentToolResult {
                        content: vec![kiqi_agent::types::TextContentOrImage::text("hi")],
                        details: Value::Null,
                        usage: None,
                        added_tool_names: Vec::new(),
                        terminate: false,
                    },
                    is_error: false,
                    context: &kiqi_agent::AgentContext::default(),
                },
                CancellationToken::new(),
            )
            .await
            .expect("a policy changed through a link is flagged");
            assert_eq!(outcome.is_error, Some(true), "case={case}");
            assert!(
                std::fs::symlink_metadata(&link)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false),
                "the link is restored, case={case}"
            );
            assert_eq!(
                std::fs::read_link(&link).unwrap(),
                target,
                "the link points at the user's target again, case={case}"
            );
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                original,
                "the target's bytes are restored, case={case}"
            );
            assert_eq!(std::fs::read_to_string(&link).unwrap(), original);

            std::fs::remove_dir_all(base).ok();
        }
    }

    #[test]
    fn a_sanctioned_save_wins_over_the_snapshot() {
        let dir = temp_dir("policy-version");
        let local = dir.join(".ki").join("permissions.json");
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, b"OLD").unwrap();
        let snapshot = PolicySnapshot::capture_paths(vec![local.clone()]);
        save(
            &local,
            &Rules {
                allow: vec!["Bash(ls)".into()],
                deny: Vec::new(),
            },
        )
        .unwrap();
        let sanctioned = std::fs::read(&local).unwrap();
        assert!(snapshot.restore_if_changed().unwrap().is_none());
        assert_eq!(std::fs::read(&local).unwrap(), sanctioned);
        std::fs::remove_dir_all(dir).ok();
    }
    #[test]
    fn read_only_find_exec_is_not_destructive() {
        let dir = temp_dir("find-exec");
        write_project(&dir, "{}");
        let bash = |command: &str| blocking_reason(&dir, "bash", &json!({ "command": command }));
        for allowed in [
            "find . -name '*.rs' -exec grep -n TODO {} +",
            "find . -type f -exec wc -l {} +",
            r"find -L . -exec stat {} \;",
        ] {
            assert_eq!(bash(allowed), None, "must allow: {allowed}");
        }
        for blocked in [
            "find . -delete",
            r"find . -exec rm -rf {} \;",
            r"find . -exec sh -c 'rm -rf .ki' \;",
            r"find . -exec sed -i s/x/y/ {} \;",
        ] {
            assert!(bash(blocked).is_some(), "must block: {blocked}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_policy_changed_between_calls_is_restored_before_the_next_call() {
        let dir = temp_dir("policy-baseline");
        write_project(&dir, r#"{"permissions":{}}"#);
        let local = dir.join(".ki").join("permissions.json");
        let guard = PolicyGuard::new(dir.clone());

        // Simulate a background writer: no `save`, so the file version does not
        // change and the baseline must notice the bytes directly.
        std::fs::write(&local, r#"{"permissions":{"allow":["*"]}}"#).unwrap();
        let changed = guard.check_baseline().expect("the change is detected");
        assert!(changed.contains(&local));
        assert_eq!(
            std::fs::read(&local).unwrap(),
            br#"{"permissions":{}}"#.as_slice()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_sanctioned_edit_becomes_the_new_baseline() {
        let dir = temp_dir("policy-baseline-sanctioned");
        write_project(&dir, r#"{"permissions":{}}"#);
        let local = dir.join(".ki").join("permissions.json");
        let guard = PolicyGuard::new(dir.clone());

        save(
            &local,
            &Rules {
                allow: vec!["Bash(ls)".into()],
                deny: Vec::new(),
            },
        )
        .unwrap();
        assert!(
            guard.check_baseline().is_none(),
            "a sanctioned edit is accepted, not restored"
        );
        let sanctioned = std::fs::read(&local).unwrap();

        std::fs::write(&local, r#"{"permissions":{"allow":["*"]}}"#).unwrap();
        assert!(guard.check_baseline().is_some());
        assert_eq!(std::fs::read(&local).unwrap(), sanctioned);
        std::fs::remove_dir_all(dir).ok();
    }
}
