//! Optional OS sandbox for the shell tool.
//!
//! The lexical permission guard plus the post-call policy restore cannot stop a
//! process that writes the policy **after** the shell call returns. On macOS the
//! shell child tree can be wrapped in `sandbox-exec` with a profile that denies
//! writes to the policy files and their directories; the rest of the process
//! keeps `(allow default)`, so normal builds, git and workspace edits work.
//!
//! Linux Landlock is a follow-up; [`ShellSandbox::macos_for`] returns `None`
//! there, preserving today's behavior. The design and residual analysis live in
//! `docs/design/policy-sandbox.md`.

use std::path::{Path, PathBuf};

use crate::env::ShellOverride;

/// A set of paths the shell child tree must not modify. The profile uses
/// `literal` matches: the policy files themselves, and their directories (so a
/// `mv .ki …` or `rm -rf .ki` cannot detach them). Writes to other files inside
/// those directories stay allowed.
#[derive(Clone, Debug)]
pub struct ShellSandbox {
    protected: Vec<PathBuf>,
}

impl ShellSandbox {
    /// Build a macOS sandbox, or `None` when the platform, `sandbox-exec` or the
    /// protection list is unavailable. Returning `None` is the documented
    /// fallback: the lexical guard and post-call restore still run.
    pub fn macos_for(protected: Vec<PathBuf>) -> Option<Self> {
        if !cfg!(target_os = "macos") || protected.is_empty() {
            return None;
        }
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return None;
        }
        let protected = protected
            .into_iter()
            .map(|path| {
                let absolute = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .map(|cwd| cwd.join(&path))
                        .unwrap_or(path)
                };
                // `sandbox-exec` matches the resolved vnode, so `/var/...` must
                // become `/private/var/...`. A policy file may not exist yet;
                // resolve its parent and re-attach the file name.
                if let Ok(canonical) = absolute.canonicalize() {
                    return canonical;
                }
                match (absolute.parent(), absolute.file_name()) {
                    (Some(parent), Some(name)) => match parent.canonicalize() {
                        Ok(canonical_parent) => canonical_parent.join(name),
                        Err(_) => absolute,
                    },
                    _ => absolute,
                }
            })
            .collect();
        Some(Self { protected })
    }

    /// The sandbox-exec invocation to use instead of the plain shell. The
    /// command is appended by [`ExecutionEnv`](crate::env::ExecutionEnv) as the
    /// final argv element, so the args end with `-c`.
    pub fn shell_override(&self) -> Option<ShellOverride> {
        let shell = if Path::new("/bin/bash").exists() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        Some(ShellOverride {
            program: PathBuf::from("/usr/bin/sandbox-exec"),
            args: vec![
                "-p".to_string(),
                self.profile(),
                shell.to_string(),
                "-c".to_string(),
            ],
        })
    }

    /// The deny rules. `allow default` keeps network, builds and workspace
    /// writes intact; `literal` matches the exact path, not its children.
    pub fn profile(&self) -> String {
        let mut profile = String::from("(version 1)\n(allow default)\n");
        for path in &self.protected {
            let escaped = path
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            profile.push_str(&format!("(deny file-write* (literal \"{escaped}\"))\n"));
        }
        profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_denies_each_literal() {
        let sandbox = ShellSandbox {
            protected: vec![PathBuf::from("/tmp/project/.ki/permissions.json")],
        };
        let profile = sandbox.profile();
        assert!(profile.contains("(allow default)"));
        assert!(
            profile.contains("(deny file-write* (literal \"/tmp/project/.ki/permissions.json\"))")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_protected_write_is_denied_and_a_workspace_write_is_allowed() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let protected = tmp.path().join("protected");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(protected.join("permissions.json"), "original").unwrap();

        let sandbox =
            ShellSandbox::macos_for(vec![protected.join("permissions.json"), protected.clone()])
                .expect("macOS sandbox");
        let shell = sandbox.shell_override().expect("sandbox-exec override");
        let run = |command: &str| {
            std::process::Command::new(&shell.program)
                .args(&shell.args)
                .arg(command)
                .status()
                .expect("sandbox-exec runs")
        };

        assert!(
            !run(&format!(
                "echo replaced > {}",
                protected.join("permissions.json").display()
            ))
            .success(),
            "the protected policy must not be writable"
        );
        assert_eq!(
            std::fs::read_to_string(protected.join("permissions.json")).unwrap(),
            "original"
        );
        assert!(
            !run(&format!(
                "rm -f {}",
                protected.join("permissions.json").display()
            ))
            .success(),
            "the protected policy must not be removable"
        );
        assert!(
            run(&format!(
                "echo ok > {}",
                workspace.join("out.txt").display()
            ))
            .success(),
            "the workspace must stay writable"
        );
        assert!(
            run(&format!(
                "echo ok > {}",
                protected.join("other.txt").display()
            ))
            .success(),
            "a protected directory's children must stay writable (literal, not subpath)"
        );
        assert!(
            !run(&format!(
                "mv {} {}",
                protected.display(),
                tmp.path().join("moved").display()
            ))
            .success(),
            "a protected directory must not be moved out of the way"
        );
        assert!(protected.exists());
    }
}
