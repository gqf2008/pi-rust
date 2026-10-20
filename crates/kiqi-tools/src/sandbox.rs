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
        // Applying a profile from inside an existing sandbox fails: any outer
        // `deny file-write*` rule makes `sandbox_apply` return EPERM (verified
        // on macOS 26). Probe once so a nested ki, or a test run started from
        // ki's bash, falls back to the outer sandbox instead of making every
        // bash call fail to spawn.
        if !sandbox_exec_can_apply() {
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

/// Whether this process can apply a fresh `sandbox-exec` profile. False when
/// an outer sandbox is already active, and false when `sandbox-exec` itself is
/// broken; both cases fall back to the enclosing confinement.
fn sandbox_exec_can_apply() -> bool {
    std::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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

        let Some(sandbox) =
            ShellSandbox::macos_for(vec![protected.join("permissions.json"), protected.clone()])
        else {
            return; // already inside a sandbox (nested ki / sandboxed test run)
        };
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

    /// The residual this sandbox exists for: a process that writes after the
    /// shell call returned. It must inherit the profile through `nohup`.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_detached_background_writer_stays_inside_the_sandbox() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let policy = tmp.path().join("permissions.json");
        std::fs::write(&policy, "original").unwrap();
        let Some(sandbox) = ShellSandbox::macos_for(vec![policy.clone()]) else {
            return; // already inside a sandbox (nested ki / sandboxed test run)
        };
        let shell = sandbox.shell_override().unwrap();
        let script = tmp.path().join("late.sh");
        std::fs::write(
            &script,
            format!("sleep 1\nprintf late > {}\n", policy.display()),
        )
        .unwrap();

        let launched = std::process::Command::new(&shell.program)
            .args(&shell.args)
            .arg(format!(
                "nohup /bin/sh {} >/dev/null 2>&1 &",
                script.display()
            ))
            .status()
            .expect("sandbox-exec runs");
        assert!(launched.success(), "the background writer must launch");

        std::thread::sleep(std::time::Duration::from_secs(2));
        assert_eq!(
            std::fs::read_to_string(&policy).unwrap(),
            "original",
            "the detached writer must not reach the policy"
        );
    }

    /// Why [`sandbox_exec_can_apply`] probes instead of assuming: any outer
    /// `deny file-write*` rule makes a nested `sandbox_apply` fail with EPERM,
    /// even when the rule names an unrelated path.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_outer_deny_rule_blocks_nested_sandbox_apply() {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return;
        }
        let outer = "(version 1)(allow default)\
                     (deny file-write* (literal \"/nonexistent-ki-nested-probe\"))";
        let status = std::process::Command::new("/usr/bin/sandbox-exec")
            .args([
                "-p",
                outer,
                "/usr/bin/sandbox-exec",
                "-p",
                "(version 1)(allow default)",
                "/usr/bin/true",
            ])
            .status()
            .expect("sandbox-exec runs");
        assert!(!status.success(), "a nested sandbox must not apply");
    }
}
