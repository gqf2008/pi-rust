//! Per-plugin capability grants — the **user's** authorization.
//!
//! The host never authorizes on a plugin's behalf: a plugin gets exactly what
//! the user recorded here, and a missing file grants nothing. The file lives at
//! `$KI_AGENT_DIR/plugins/<id>/grants.json` (default
//! `~/.ki/agent/plugins/<id>/grants.json`), so both the wasm host (which
//! enforces it) and the CLI (which edits it) share this one type without either
//! depending on the other.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool {
    !*value
}

/// What the user authorized for one plugin.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Grants {
    /// Host-mediated HTTP, with a host allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_http: Option<NetHttpGrant>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fs: bool,
    /// Running a child process, with a command allow-list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecGrant>,
    /// Interactive child processes, with a command allow-list, a cwd scope, a
    /// per-child budget and a concurrency bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc: Option<ProcGrant>,
    /// Raw TCP, with a host allow-list (same shape as `net-http`: the grant
    /// carries its policy, so "granted" and "to what" are one decision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_sockets: Option<NetSocketsGrant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetHttpGrant {
    /// Hosts the plugin may reach; `*` allows any.
    pub hosts: Vec<String>,
}

/// The default wall-clock budget for one `exec` call when the grant says
/// nothing — the value the host used before the budget was grant-carried.
pub const EXEC_TIMEOUT_DEFAULT_SECONDS: u64 = 30;

/// The largest budget a grant can ask for. The CLI rejects larger values and
/// the host clamps them, so a hand-edited file cannot buy an unbounded child.
pub const EXEC_TIMEOUT_MAX_SECONDS: u64 = 300;

/// Which root an `exec` working directory resolves against. The scope rides
/// with the grant — "granted" and "where" are one decision, the same rule the
/// command allow-list follows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecCwd {
    /// Inside `plugins/<id>/data/` — the default, and the same jail `fs` uses.
    #[default]
    PluginData,
    /// The project directory the session runs in (the host process's working
    /// directory). Needed by diagnostic plugins that run `cargo`/`clippy`
    /// against the user's tree.
    Project,
}

impl ExecCwd {
    /// The CLI spelling (`--cwd plugin-data|project`).
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "plugin-data" => Ok(ExecCwd::PluginData),
            "project" => Ok(ExecCwd::Project),
            other => Err(format!(
                "exec cwd must be `plugin-data` or `project`, got `{other}`"
            )),
        }
    }

    fn is_plugin_data(&self) -> bool {
        matches!(self, ExecCwd::PluginData)
    }
}

fn default_exec_timeout() -> u64 {
    EXEC_TIMEOUT_DEFAULT_SECONDS
}

fn is_default_exec_timeout(value: &u64) -> bool {
    *value == EXEC_TIMEOUT_DEFAULT_SECONDS
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ExecGrant {
    /// Command names (no path, no shell) the plugin may run; `*` allows any.
    pub commands: Vec<String>,
    /// Where the child may start. Absent in older grant files = `plugin-data`,
    /// so an existing grant keeps its old meaning unchanged.
    #[serde(default, skip_serializing_if = "ExecCwd::is_plugin_data")]
    pub cwd: ExecCwd,
    /// Wall-clock budget in seconds for one call; absent = the default,
    /// capped at [`EXEC_TIMEOUT_MAX_SECONDS`].
    #[serde(
        default = "default_exec_timeout",
        skip_serializing_if = "is_default_exec_timeout"
    )]
    pub timeout_seconds: u64,
}

/// The default wall-clock budget for one `proc` child — long enough for an
/// index/analysis server to do real work, and deliberately its own constant:
/// it exceeds `exec`'s 30s/300s pair, which is the point of a separate
/// capability (a session is not a bounded one-shot command).
pub const PROC_TIMEOUT_DEFAULT_SECONDS: u64 = 600;

/// The largest budget a `proc` grant can ask for (the orchestrator's subagent
/// ceiling, reused so the repo has one "very long child" number).
pub const PROC_TIMEOUT_MAX_SECONDS: u64 = 1800;

/// How many children one call may hold open by default.
pub const PROC_MAX_OPEN_DEFAULT: u32 = 100;

/// The hard ceiling on `max-open` — the resource is the machine's process
/// table, not something the sandbox can carry for the plugin.
pub const PROC_MAX_OPEN_MAX: u32 = 256;

fn default_proc_timeout() -> u64 {
    PROC_TIMEOUT_DEFAULT_SECONDS
}

fn is_default_proc_timeout(value: &u64) -> bool {
    *value == PROC_TIMEOUT_DEFAULT_SECONDS
}

fn default_proc_max_open() -> u32 {
    PROC_MAX_OPEN_DEFAULT
}

fn is_default_proc_max_open(value: &u32) -> bool {
    *value == PROC_MAX_OPEN_DEFAULT
}

/// The `proc` policy: which commands, in which scope, for how long, and how
/// many at once. Same shape as [`ExecGrant`] plus the concurrency bound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProcGrant {
    /// Command names (no path, no shell) the plugin may spawn; `*` allows any.
    pub commands: Vec<String>,
    /// Where a child may start — same scope rule as `exec`.
    #[serde(default, skip_serializing_if = "ExecCwd::is_plugin_data")]
    pub cwd: ExecCwd,
    /// Wall-clock budget for one child; absent = the default, capped at
    /// [`PROC_TIMEOUT_MAX_SECONDS`].
    #[serde(
        default = "default_proc_timeout",
        skip_serializing_if = "is_default_proc_timeout"
    )]
    pub timeout_seconds: u64,
    /// How many children one call may hold open at once.
    #[serde(
        default = "default_proc_max_open",
        skip_serializing_if = "is_default_proc_max_open"
    )]
    pub max_open: u32,
}

impl Default for ProcGrant {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            cwd: ExecCwd::default(),
            timeout_seconds: PROC_TIMEOUT_DEFAULT_SECONDS,
            max_open: PROC_MAX_OPEN_DEFAULT,
        }
    }
}

impl Default for ExecGrant {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            cwd: ExecCwd::default(),
            timeout_seconds: EXEC_TIMEOUT_DEFAULT_SECONDS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetSocketsGrant {
    /// Hosts the plugin may open a TCP connection to; `*` allows any.
    pub hosts: Vec<String>,
}

/// Options a grant may carry beyond its value list. Only `exec` has any; a
/// caller setting them for another capability gets an error, not a silently
/// dropped flag.
#[derive(Debug, Clone, Default)]
pub struct GrantOptions {
    /// `--cwd plugin-data|project` (exec and proc).
    pub cwd: Option<ExecCwd>,
    /// `--timeout <seconds>` (exec and proc; each has its own bounds).
    pub timeout_seconds: Option<u64>,
    /// `--max-open <n>` (proc only).
    pub max_open: Option<u32>,
}

impl Grants {
    /// Read the user's grants for `plugin_id`; a missing (or unreadable, or
    /// malformed) file means nothing is granted.
    pub fn load_for(plugin_id: &str) -> Self {
        match agent_dir() {
            Some(root) => Self::load_for_at(&root, plugin_id),
            None => Self::default(),
        }
    }

    /// [`Self::load_for`] against an explicit agent root, so a caller (a test, or
    /// a CLI that was handed a directory) never has to write a process-global
    /// environment variable to redirect it.
    pub fn load_for_at(root: &std::path::Path, plugin_id: &str) -> Self {
        let path = grants_path_at(root, plugin_id);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    /// Write the grants, creating `plugins/<id>/` as needed.
    pub fn save_for(plugin_id: &str, grants: &Grants) -> Result<(), String> {
        let root = agent_dir().ok_or("cannot resolve the agent directory")?;
        Self::save_for_at(&root, plugin_id, grants)
    }

    /// [`Self::save_for`] against an explicit agent root.
    pub fn save_for_at(
        root: &std::path::Path,
        plugin_id: &str,
        grants: &Grants,
    ) -> Result<(), String> {
        let path = grants_path_at(root, plugin_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(grants).map_err(|error| error.to_string())?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|error| format!("write {}: {error}", path.display()))
    }

    /// True when HTTP to `host` is granted.
    pub fn allows_host(&self, host: &str) -> bool {
        self.net_http
            .as_ref()
            .is_some_and(|grant| allows(&grant.hosts, host))
    }

    /// True when `command` may be spawned through `proc`.
    pub fn allows_proc_command(&self, command: &str) -> bool {
        self.proc
            .as_ref()
            .is_some_and(|grant| allows(&grant.commands, command))
    }

    /// True when a TCP connection to `host` is granted.
    pub fn allows_socket_host(&self, host: &str) -> bool {
        self.net_sockets
            .as_ref()
            .is_some_and(|grant| allows(&grant.hosts, host))
    }

    /// True when `command` (a bare name, matched exactly against the list) is
    /// granted.
    pub fn allows_command(&self, command: &str) -> bool {
        self.exec
            .as_ref()
            .is_some_and(|grant| allows(&grant.commands, command))
    }

    /// The capability names currently granted, for display.
    pub fn granted(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.net_http.is_some() {
            out.push("net-http");
        }
        if self.fs {
            out.push("fs");
        }
        if self.exec.is_some() {
            out.push("exec");
        }
        if self.proc.is_some() {
            out.push("proc");
        }
        if self.net_sockets.is_some() {
            out.push("net-sockets");
        }
        out
    }

    /// Grant one capability. Every capability carries its policy: hosts for
    /// `net-http`/`net-sockets`, command names for `exec`, nothing for `fs`
    /// (whose jail is the policy). `exec` additionally accepts a cwd scope and
    /// a time budget through `options`; any other capability rejects them.
    pub fn grant(
        &mut self,
        capability: &str,
        policy: Vec<String>,
        options: GrantOptions,
    ) -> Result<(), String> {
        let non_empty = |what: &str| -> Result<Vec<String>, String> {
            if policy.is_empty() {
                Err(format!("{capability} needs at least one {what}"))
            } else {
                Ok(policy.clone())
            }
        };
        match capability {
            "net-http" | "net-sockets" | "fs" => {
                if options.cwd.is_some()
                    || options.timeout_seconds.is_some()
                    || options.max_open.is_some()
                {
                    return Err(format!(
                        "`{capability}` takes no --cwd/--timeout/--max-open (those are exec and proc options)"
                    ));
                }
                let hosts = || non_empty("--host (or --host '*'))");
                match capability {
                    "net-http" => self.net_http = Some(NetHttpGrant { hosts: hosts()? }),
                    "net-sockets" => self.net_sockets = Some(NetSocketsGrant { hosts: hosts()? }),
                    _ => self.fs = true,
                }
            }
            "exec" => {
                if options.max_open.is_some() {
                    return Err("`exec` takes no --max-open (that is a proc option)".to_string());
                }
                let commands = non_empty("--command (or --command '*')")?;
                let timeout_seconds = match options.timeout_seconds {
                    None => EXEC_TIMEOUT_DEFAULT_SECONDS,
                    Some(0) => return Err("exec --timeout must be at least 1 second".to_string()),
                    Some(seconds) if seconds > EXEC_TIMEOUT_MAX_SECONDS => {
                        return Err(format!(
                            "exec --timeout {seconds}s exceeds the {EXEC_TIMEOUT_MAX_SECONDS}s cap"
                        ))
                    }
                    Some(seconds) => seconds,
                };
                self.exec = Some(ExecGrant {
                    commands,
                    cwd: options.cwd.unwrap_or_default(),
                    timeout_seconds,
                });
            }
            "proc" => {
                let commands = non_empty("--command (or --command '*')")?;
                let timeout_seconds = match options.timeout_seconds {
                    None => PROC_TIMEOUT_DEFAULT_SECONDS,
                    Some(0) => return Err("proc --timeout must be at least 1 second".to_string()),
                    Some(seconds) if seconds > PROC_TIMEOUT_MAX_SECONDS => {
                        return Err(format!(
                            "proc --timeout {seconds}s exceeds the {PROC_TIMEOUT_MAX_SECONDS}s cap"
                        ))
                    }
                    Some(seconds) => seconds,
                };
                let max_open = match options.max_open {
                    None => PROC_MAX_OPEN_DEFAULT,
                    Some(0) => return Err("proc --max-open must be at least 1".to_string()),
                    Some(n) if n > PROC_MAX_OPEN_MAX => {
                        return Err(format!(
                            "proc --max-open {n} exceeds the {PROC_MAX_OPEN_MAX} cap"
                        ))
                    }
                    Some(n) => n,
                };
                self.proc = Some(ProcGrant {
                    commands,
                    cwd: options.cwd.unwrap_or_default(),
                    timeout_seconds,
                    max_open,
                });
            }
            other => return Err(format!("unknown capability `{other}`")),
        }
        Ok(())
    }

    /// Revoke one capability.
    pub fn revoke(&mut self, capability: &str) -> Result<(), String> {
        match capability {
            "net-http" => self.net_http = None,
            "fs" => self.fs = false,
            "exec" => self.exec = None,
            "proc" => self.proc = None,
            "net-sockets" => self.net_sockets = None,
            other => return Err(format!("unknown capability `{other}`")),
        }
        Ok(())
    }
}

/// Exact match, or `*` for "any" — the one allow-list rule every policy uses.
pub fn allows(list: &[String], value: &str) -> bool {
    list.iter()
        .any(|allowed| allowed == "*" || allowed == value)
}

/// The agent data directory: `$KI_AGENT_DIR`, else the CLI's own relocation
/// knob `$KI_CODING_AGENT_DIR` (which is what actually moves `~/.ki`), else
/// `$HOME/.ki/agent`.
///
/// Reading only `KI_AGENT_DIR` — a name nothing in the tree sets — meant an
/// isolated run (`KI_CODING_AGENT_DIR=/tmp/profile ki`) still discovered, and
/// granted, the plugins installed in the default profile.
pub fn agent_dir() -> Option<PathBuf> {
    for var in ["KI_AGENT_DIR", "KI_CODING_AGENT_DIR"] {
        if let Some(dir) = std::env::var_os(var) {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ki").join("agent"))
}

/// Where a plugin's grants live.
pub fn grants_path(plugin_id: &str) -> Option<PathBuf> {
    agent_dir().map(|root| grants_path_at(&root, plugin_id))
}

/// Where a plugin's grants live under an explicit agent root.
pub fn grants_path_at(root: &std::path::Path, plugin_id: &str) -> PathBuf {
    plugin_dir_at(root, plugin_id).join("grants.json")
}

/// A plugin's own directory (`plugins/<id>/`), which holds its `plugin.wasm`
/// and `grants.json`.
pub fn plugin_dir(plugin_id: &str) -> Option<PathBuf> {
    agent_dir().map(|root| plugin_dir_at(&root, plugin_id))
}

/// A plugin's own directory under an explicit agent root.
pub fn plugin_dir_at(root: &std::path::Path, plugin_id: &str) -> PathBuf {
    root.join("plugins").join(plugin_id)
}

/// The infix `ki plugin uninstall` renames a plugin directory with.
pub const UNINSTALLED_INFIX: &str = ".removed-";

/// True when a directory under `plugins/` is one `uninstall` moved aside.
///
/// The loader and the installer must agree: a moved-aside directory still
/// holds its `plugin.wasm` and `grants.json`, so treating it as installed
/// would keep loading (and granting) a plugin the user asked to remove.
pub fn is_uninstalled_dir_name(name: &str) -> bool {
    name.contains(UNINSTALLED_INFIX)
}

/// Plugin ids that have a directory under `plugins/`.
pub fn installed_plugins() -> Vec<String> {
    agent_dir()
        .map(|root| installed_plugins_at(&root))
        .unwrap_or_default()
}

/// Plugin ids with a directory under `root/plugins/`.
pub fn installed_plugins_at(root: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join("plugins")) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !is_uninstalled_dir_name(name))
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_grants_nothing() {
        let grants = Grants::load_for("no-such-plugin-here");
        assert_eq!(grants, Grants::default());
        assert!(grants.granted().is_empty());
        assert!(!grants.allows_host("example.com"));
    }

    #[test]
    fn grant_revoke_and_host_matching() {
        let mut grants = Grants::default();
        grants
            .grant(
                "net-http",
                vec!["api.langfuse.com".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        assert!(grants.allows_host("api.langfuse.com"));
        assert!(!grants.allows_host("evil.example"));
        assert_eq!(grants.granted(), vec!["net-http"]);

        grants.grant("fs", vec![], GrantOptions::default()).unwrap();
        assert_eq!(grants.granted(), vec!["net-http", "fs"]);

        grants.revoke("net-http").unwrap();
        assert!(grants.granted() == vec!["fs"]);
        assert!(!grants.allows_host("api.langfuse.com"));

        assert!(grants
            .grant("nope", vec![], GrantOptions::default())
            .is_err());
        assert!(grants.revoke("nope").is_err());
    }

    #[test]
    fn wildcard_host_is_allowed() {
        let mut grants = Grants::default();
        grants
            .grant("net-http", vec!["*".to_string()], GrantOptions::default())
            .unwrap();
        assert!(grants.allows_host("anything.example"));
    }

    /// Every capability carries its policy, so "granted" and "to what" are one
    /// decision — and a policy-carrying capability without one is an error
    /// rather than a silent allow-all.
    #[test]
    fn every_capability_carries_its_policy() {
        let mut grants = Grants::default();
        grants
            .grant(
                "net-sockets",
                vec!["127.0.0.1".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        grants
            .grant("exec", vec!["echo".to_string()], GrantOptions::default())
            .unwrap();
        grants.grant("fs", vec![], GrantOptions::default()).unwrap();

        assert!(grants.allows_socket_host("127.0.0.1"));
        assert!(!grants.allows_socket_host("example.com"));
        assert!(grants.allows_command("echo"));
        assert!(!grants.allows_command("rm"));
        assert_eq!(grants.granted(), vec!["fs", "exec", "net-sockets"]);

        assert!(grants
            .grant("exec", vec![], GrantOptions::default())
            .is_err());
        assert!(grants
            .grant("net-sockets", vec![], GrantOptions::default())
            .is_err());
        assert!(grants
            .grant("net-http", vec![], GrantOptions::default())
            .is_err());

        grants.revoke("exec").unwrap();
        assert!(!grants.allows_command("echo"));
        assert!(!grants.granted().contains(&"exec"));
    }

    /// The exec policy: defaults preserve the pre-scope meaning (plugin data
    /// dir, 30s), and only `exec` accepts the scope/budget options.
    #[test]
    fn exec_grant_carries_cwd_scope_and_time_budget() {
        let mut grants = Grants::default();
        grants
            .grant(
                "exec",
                vec!["cargo".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: Some(120),
                    max_open: None,
                },
            )
            .unwrap();
        let grant = grants.exec.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::Project);
        assert_eq!(grant.timeout_seconds, 120);

        // Defaults when the options are not passed.
        grants
            .grant("exec", vec!["echo".to_string()], GrantOptions::default())
            .unwrap();
        let grant = grants.exec.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::PluginData);
        assert_eq!(grant.timeout_seconds, EXEC_TIMEOUT_DEFAULT_SECONDS);

        // Validation: the budget is bounded, and only exec takes the options.
        let bad_timeout = |seconds: u64| GrantOptions {
            cwd: None,
            timeout_seconds: Some(seconds),
            max_open: None,
        };
        assert!(grants
            .grant("exec", vec!["echo".to_string()], bad_timeout(0))
            .is_err());
        assert!(grants
            .grant(
                "exec",
                vec!["echo".to_string()],
                bad_timeout(EXEC_TIMEOUT_MAX_SECONDS + 1),
            )
            .is_err());
        assert!(grants
            .grant(
                "exec",
                vec!["echo".to_string()],
                bad_timeout(EXEC_TIMEOUT_MAX_SECONDS),
            )
            .is_ok());
        assert!(grants
            .grant(
                "net-http",
                vec!["example.com".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: None,
                    max_open: None,
                },
            )
            .is_err());
    }

    /// The `proc` policy: its own defaults (600s / 100 open / plugin-data), its
    /// own bounds, and options that belong to the other capabilities are
    /// rejected rather than ignored.
    #[test]
    fn proc_grant_carries_its_own_defaults_and_bounds() {
        let mut grants = Grants::default();
        grants
            .grant(
                "proc",
                vec!["codegraph".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        let grant = grants.proc.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::PluginData);
        assert_eq!(grant.timeout_seconds, PROC_TIMEOUT_DEFAULT_SECONDS);
        assert_eq!(grant.max_open, PROC_MAX_OPEN_DEFAULT);
        assert!(grants.allows_proc_command("codegraph"));
        assert!(!grants.allows_proc_command("rm"));
        assert!(grants.granted().contains(&"proc"));

        // Explicit options ride along.
        grants
            .grant(
                "proc",
                vec!["codegraph".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: Some(900),
                    max_open: Some(8),
                },
            )
            .unwrap();
        let grant = grants.proc.as_ref().unwrap();
        assert_eq!(grant.cwd, ExecCwd::Project);
        assert_eq!(grant.timeout_seconds, 900);
        assert_eq!(grant.max_open, 8);

        // Bounds are enforced at grant time.
        let bad = |seconds: u64, open: u32| GrantOptions {
            cwd: None,
            timeout_seconds: Some(seconds),
            max_open: Some(open),
        };
        assert!(grants
            .grant("proc", vec!["x".to_string()], bad(0, 1))
            .is_err());
        assert!(grants
            .grant(
                "proc",
                vec!["x".to_string()],
                bad(PROC_TIMEOUT_MAX_SECONDS + 1, 1)
            )
            .is_err());
        assert!(grants
            .grant("proc", vec!["x".to_string()], bad(60, 0))
            .is_err());
        assert!(grants
            .grant(
                "proc",
                vec!["x".to_string()],
                bad(60, PROC_MAX_OPEN_MAX + 1)
            )
            .is_err());
        assert!(grants
            .grant(
                "proc",
                vec!["x".to_string()],
                bad(PROC_TIMEOUT_MAX_SECONDS, PROC_MAX_OPEN_MAX)
            )
            .is_ok());

        // `--max-open` is proc's alone; the cwd/timeout pair never reaches the
        // network capabilities or fs.
        assert!(grants
            .grant(
                "exec",
                vec!["echo".to_string()],
                GrantOptions {
                    cwd: None,
                    timeout_seconds: None,
                    max_open: Some(2),
                },
            )
            .is_err());
        assert!(grants
            .grant(
                "fs",
                vec![],
                GrantOptions {
                    cwd: None,
                    timeout_seconds: None,
                    max_open: Some(2),
                },
            )
            .is_err());

        // Revoking takes it away again.
        grants.revoke("proc").unwrap();
        assert!(!grants.allows_proc_command("codegraph"));
        assert!(!grants.granted().contains(&"proc"));
    }

    /// A grant file from before `proc` existed deserializes unchanged, and the
    /// new defaults stay out of the serialized form.
    #[test]
    fn proc_grant_json_is_backward_compatible_and_minimal() {
        let old = serde_json::json!({"exec": {"commands": ["echo"]}});
        let grants: Grants = serde_json::from_value(old).unwrap();
        assert!(grants.proc.is_none());
        assert!(!grants.allows_proc_command("echo"));

        let mut grants = Grants::default();
        grants
            .grant(
                "proc",
                vec!["codegraph".to_string()],
                GrantOptions::default(),
            )
            .unwrap();
        let json = serde_json::to_value(&grants).unwrap();
        assert_eq!(
            json["proc"],
            serde_json::json!({"commands": ["codegraph"]}),
            "defaults are omitted, like exec's"
        );
        let reparsed: Grants = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed, grants);
    }

    /// Grant files written before the cwd/timeout fields existed must keep
    /// their meaning: an old `{"commands":[...]}` deserializes to the defaults.
    #[test]
    fn exec_grant_json_is_backward_compatible() {
        let old = serde_json::json!({"exec": {"commands": ["cargo", "git"]}});
        let grants: Grants = serde_json::from_value(old).unwrap();
        let grant = grants.exec.as_ref().unwrap();
        assert_eq!(grant.commands, vec!["cargo", "git"]);
        assert_eq!(grant.cwd, ExecCwd::PluginData);
        assert_eq!(grant.timeout_seconds, EXEC_TIMEOUT_DEFAULT_SECONDS);

        // The default fields stay out of the file (minimal grants.json), and
        // the explicit ones use the kebab-case spellings the CLI documents.
        let minimal = serde_json::to_string(&grants).unwrap();
        assert!(!minimal.contains("cwd") && !minimal.contains("timeout"));

        let mut grants = Grants::default();
        grants
            .grant(
                "exec",
                vec!["cargo".to_string()],
                GrantOptions {
                    cwd: Some(ExecCwd::Project),
                    timeout_seconds: Some(120),
                    max_open: None,
                },
            )
            .unwrap();
        let json = serde_json::to_value(&grants).unwrap();
        assert_eq!(
            json["exec"],
            serde_json::json!({
                "commands": ["cargo"],
                "cwd": "project",
                "timeout-seconds": 120,
            })
        );
        // Round-trip back through the file form.
        let reparsed: Grants = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed, grants);

        assert_eq!(ExecCwd::parse("project").unwrap(), ExecCwd::Project);
        assert_eq!(ExecCwd::parse("plugin-data").unwrap(), ExecCwd::PluginData);
        assert!(ExecCwd::parse("/abs/path").is_err());
    }
}
