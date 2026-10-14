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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecGrant {
    /// Command names (no path, no shell) the plugin may run; `*` allows any.
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetSocketsGrant {
    /// Hosts the plugin may open a TCP connection to; `*` allows any.
    pub hosts: Vec<String>,
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
        if self.net_sockets.is_some() {
            out.push("net-sockets");
        }
        out
    }

    /// Grant one capability. Every capability carries its policy: hosts for
    /// `net-http`/`net-sockets`, command names for `exec`, nothing for `fs`
    /// (whose jail is the policy).
    pub fn grant(&mut self, capability: &str, policy: Vec<String>) -> Result<(), String> {
        let non_empty = |what: &str| -> Result<Vec<String>, String> {
            if policy.is_empty() {
                Err(format!("{capability} needs at least one {what}"))
            } else {
                Ok(policy.clone())
            }
        };
        match capability {
            "net-http" => {
                self.net_http = Some(NetHttpGrant {
                    hosts: non_empty("--host (or --host '*'))")?,
                })
            }
            "net-sockets" => {
                self.net_sockets = Some(NetSocketsGrant {
                    hosts: non_empty("--host (or --host '*'))")?,
                })
            }
            "exec" => {
                self.exec = Some(ExecGrant {
                    commands: non_empty("--command (or --command '*')")?,
                })
            }
            "fs" => self.fs = true,
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
            "net-sockets" => self.net_sockets = None,
            other => return Err(format!("unknown capability `{other}`")),
        }
        Ok(())
    }
}

/// Exact match, or `*` for "any" — the one allow-list rule every policy uses.
fn allows(list: &[String], value: &str) -> bool {
    list.iter()
        .any(|allowed| allowed == "*" || allowed == value)
}

/// The agent data directory: `$KI_AGENT_DIR`, else `$HOME/.ki/agent`.
pub fn agent_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("KI_AGENT_DIR") {
        return Some(PathBuf::from(dir));
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
            .grant("net-http", vec!["api.langfuse.com".to_string()])
            .unwrap();
        assert!(grants.allows_host("api.langfuse.com"));
        assert!(!grants.allows_host("evil.example"));
        assert_eq!(grants.granted(), vec!["net-http"]);

        grants.grant("fs", vec![]).unwrap();
        assert_eq!(grants.granted(), vec!["net-http", "fs"]);

        grants.revoke("net-http").unwrap();
        assert!(grants.granted() == vec!["fs"]);
        assert!(!grants.allows_host("api.langfuse.com"));

        assert!(grants.grant("nope", vec![]).is_err());
        assert!(grants.revoke("nope").is_err());
    }

    #[test]
    fn wildcard_host_is_allowed() {
        let mut grants = Grants::default();
        grants.grant("net-http", vec!["*".to_string()]).unwrap();
        assert!(grants.allows_host("anything.example"));
    }

    /// Every capability carries its policy, so "granted" and "to what" are one
    /// decision — and a policy-carrying capability without one is an error
    /// rather than a silent allow-all.
    #[test]
    fn every_capability_carries_its_policy() {
        let mut grants = Grants::default();
        grants
            .grant("net-sockets", vec!["127.0.0.1".to_string()])
            .unwrap();
        grants.grant("exec", vec!["echo".to_string()]).unwrap();
        grants.grant("fs", vec![]).unwrap();

        assert!(grants.allows_socket_host("127.0.0.1"));
        assert!(!grants.allows_socket_host("example.com"));
        assert!(grants.allows_command("echo"));
        assert!(!grants.allows_command("rm"));
        assert_eq!(grants.granted(), vec!["fs", "exec", "net-sockets"]);

        assert!(grants.grant("exec", vec![]).is_err());
        assert!(grants.grant("net-sockets", vec![]).is_err());
        assert!(grants.grant("net-http", vec![]).is_err());

        grants.revoke("exec").unwrap();
        assert!(!grants.allows_command("echo"));
        assert!(!grants.granted().contains(&"exec"));
    }
}
