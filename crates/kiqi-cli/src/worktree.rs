//! Git worktrees for delegated work.
//!
//! Two subagents editing the same files in one directory is how parallel work
//! goes wrong: whichever writes last wins and the other rewrite is gone. A
//! worktree gives each unit of work its own checkout on its own branch, so a
//! batch of independent tasks can actually run at the same time.
//!
//! The lifecycle is deliberately boring and auditable:
//!
//! 1. `create` adds `<repo>/.ki/worktrees/<id>` on branch `ki/wt-<id>` off the
//!    base ref (the branch this session is on, else `origin/main`, else `main`).
//! 2. The work happens there — never in the main checkout.
//! 3. `finish` rebases onto the current base, runs the verify command **inside
//!    the worktree**, and only then fast-forwards the base branch. A worktree
//!    that does not verify, or whose rebase conflicts, is left in place with a
//!    manifest saying why: the main checkout is never a half-applied state.
//! 4. Every finished worktree leaves `archive/<id>/manifest.json` plus a patch,
//!    because "the final state is right" and "we can still tell what happened"
//!    are both requirements.
//!
//! Nothing here deletes an unmerged worktree on a timer; `prune` only removes
//! worktrees whose branch is already contained in the base.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where worktrees live, relative to the repository root. `.ki/*` is ignored by
/// git, so a worktree here never shows up as untracked in the main checkout.
const WORKTREES: &str = ".ki/worktrees";
/// Finished worktrees keep their manifest + patch here.
const ARCHIVE: &str = ".ki/worktrees/archive";
/// Branch prefix. `base_ref` skips these so a worktree can never become the
/// base for the next one.
const BRANCH_PREFIX: &str = "ki/wt-";
/// An unmerged worktree older than this is reported as stale (never removed).
const STALE_DAYS: u64 = 5;

/// One worktree, as recorded on disk.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub id: String,
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
}

/// What `finish` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Rebased, verified and fast-forwarded into the base branch.
    Merged { commit: String },
    /// The worktree is left in place; the base branch is untouched.
    Halted { reason: String },
}

impl Outcome {
    pub fn describe(&self) -> String {
        match self {
            Self::Merged { commit } => format!("merged into the base branch as {commit}"),
            Self::Halted { reason } => format!("not merged: {reason}"),
        }
    }
}

/// `ki worktree <status|merge|prune|abandon>`.
pub fn run(argv: &[String]) -> i32 {
    let Some(command) = argv.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let repo = match repo_root(&match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("error: cannot read the current directory: {error}");
            return 1;
        }
    }) {
        Ok(repo) => repo,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    match command {
        "status" => {
            print!("{}", status(&repo));
            0
        }
        "merge" => match argv.get(1) {
            Some(id) => {
                let inline = argv
                    .iter()
                    .position(|arg| arg == "--verify")
                    .and_then(|index| argv.get(index + 1))
                    .cloned();
                let verify = inline.or_else(|| std::env::var("KI_WORKTREE_VERIFY").ok());
                match finish(&repo, id, verify.as_deref()) {
                    Ok(outcome) => {
                        println!("{id}: {}", outcome.describe());
                        if matches!(outcome, Outcome::Merged { .. }) {
                            0
                        } else {
                            1
                        }
                    }
                    Err(error) => {
                        eprintln!("error: {error}");
                        1
                    }
                }
            }
            None => {
                eprintln!("error: usage: ki worktree merge <id> [--verify '<command>']");
                2
            }
        },
        "prune" => {
            let removed = prune(&repo);
            for id in &removed {
                println!("removed {id} (already merged)");
            }
            if removed.is_empty() {
                println!("nothing to prune");
            }
            0
        }
        "abandon" => match argv.get(1) {
            Some(id) => match abandon(&repo, id) {
                Ok(path) => {
                    println!("archived {id} -> {}", path.display());
                    0
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    1
                }
            },
            None => {
                eprintln!("error: usage: ki worktree abandon <id>");
                2
            }
        },
        other => {
            eprintln!("error: unknown `ki worktree` command `{other}`\n\n{USAGE}");
            2
        }
    }
}

const USAGE: &str = "usage: ki worktree <command>

  status                 list live worktrees, their branch and state
  merge <id> [--verify]  rebase, verify, then fast-forward the base branch
  prune                  remove worktrees whose branch is already merged
  abandon <id>           archive the patch and drop the worktree

Worktrees live in `.ki/worktrees/<id>` (branch `ki/wt-<id>`); finished ones
keep their manifest and patch under `.ki/worktrees/archive/`.";

/// Create (or reuse) the worktree a delegated task should run in.
///
/// The `id` is a caller-chosen name; it is sanitised into a branch-safe slug and
/// suffixed with a random token so two agents given the same task name cannot
/// collide.
pub fn create(repo: &Path, id: &str) -> Result<Worktree, String> {
    let base = base_ref(repo)?;
    let slug = sanitize(id);
    let token = random_token();
    let name = format!("{slug}-{token}");
    let branch = format!("{BRANCH_PREFIX}{name}");
    let path = repo.join(WORKTREES).join(&name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--quiet", "-b"])
        .arg(&branch)
        .arg(&path)
        .arg(&base)
        .output()
        .map_err(|error| format!("run git worktree add: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let worktree = Worktree {
        id: name.clone(),
        repo: repo.to_path_buf(),
        path,
        branch,
    };
    worktree.write_manifest("running", None, None)?;
    Ok(worktree)
}

impl Worktree {
    fn manifest_path(&self) -> PathBuf {
        self.repo
            .join(WORKTREES)
            .join(&self.id)
            .join("manifest.json")
    }

    /// The manifest is the audit trail; write it as the state changes rather
    /// than only at the end, so a process that dies still leaves evidence.
    fn write_manifest(
        &self,
        state: &str,
        verify: Option<(&str, bool)>,
        note: Option<&str>,
    ) -> Result<(), String> {
        let base_commit = git(&self.path, &["rev-parse", &self.base_branch()?])
            .unwrap_or_else(|_| "unknown".to_string());
        let head = git(&self.path, &["rev-parse", "HEAD"]).unwrap_or_default();
        let files = git(
            &self.path,
            &["diff", "--name-only", &format!("{base_commit}..HEAD")],
        )
        .unwrap_or_default();
        let body = serde_json::json!({
            "id": self.id,
            "repo": self.repo,
            "path": self.path,
            "branch": self.branch,
            "state": state,
            "base_commit": base_commit,
            "head": head,
            "files": files.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>(),
            "verify": verify.map(|(command, ok)| serde_json::json!({"command": command, "ok": ok})),
            "note": note,
            "updated": timestamp(),
        });
        let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(self.manifest_path(), text)
            .map_err(|error| format!("write {}: {error}", self.manifest_path().display()))
    }

    fn base_branch(&self) -> Result<String, String> {
        Ok(base_ref(&self.repo)?.to_string())
    }
}

/// Create the worktree at exactly `name` (no random token), or reuse it.
///
/// This is the `--worktree <name>` path: the parent process picks the name and
/// has to be able to find the checkout again afterwards.
pub fn ensure_named(repo: &Path, name: &str) -> Result<Worktree, String> {
    let path = repo.join(WORKTREES).join(name);
    if path.join(".git").exists() {
        return locate(repo, name);
    }
    let base = base_ref(repo)?;
    let branch = format!("{BRANCH_PREFIX}{name}");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--quiet", "-b"])
        .arg(&branch)
        .arg(&path)
        .arg(&base)
        .output()
        .map_err(|error| format!("run git worktree add: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let worktree = Worktree {
        id: name.to_string(),
        repo: repo.to_path_buf(),
        path,
        branch,
    };
    worktree.write_manifest("running", None, None)?;
    Ok(worktree)
}

/// The worktree this process entered through `--worktree`, so `run` can land it
/// on the way out: whoever created the checkout is the one that settles it.
static PENDING: std::sync::Mutex<Option<(PathBuf, String)>> = std::sync::Mutex::new(None);

/// Enter a named worktree, creating it when it does not exist yet. The child
/// process then works there for its whole life.
pub fn enter(cwd: &Path, name: &str) -> Result<PathBuf, String> {
    // `common_root`: a worktree started from inside another worktree still
    // belongs to the one repository that owns them all.
    let repo = common_root(cwd)?;
    let worktree = ensure_named(&repo, name)?;
    std::env::set_current_dir(&worktree.path)
        .map_err(|error| format!("enter {}: {error}", worktree.path.display()))?;
    if let Ok(mut pending) = PENDING.lock() {
        *pending = Some((repo.clone(), name.to_string()));
    }
    Ok(worktree.path)
}

/// Land the worktree this process entered (`--worktree`), if it entered one.
///
/// Called once, after the session is over, so a `--worktree` run is
/// self-contained: report the outcome and let the caller turn a worktree that
/// could not land into a failing exit code. The verify command comes from
/// `KI_WORKTREE_VERIFY`, which is how a caller that spawned us states the gate.
pub fn finish_pending() -> Option<i32> {
    let (repo, name) = PENDING.lock().ok().and_then(|mut slot| slot.take())?;
    let verify = std::env::var("KI_WORKTREE_VERIFY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    match finish(&repo, &name, verify.as_deref()) {
        Ok(Outcome::Merged { commit }) => {
            eprintln!("[ki worktree] merged {name} as {}", commit.trim());
            None
        }
        Ok(Outcome::Halted { reason }) => {
            eprintln!("[ki worktree] {name} did not land: {reason}");
            Some(1)
        }
        Err(error) => {
            eprintln!("error: {error}");
            Some(1)
        }
    }
}

/// Rebase the worktree onto the current base, verify it, then fast-forward.
///
/// Order matters: the rebase happens in the worktree (a conflict there leaves
/// the main checkout untouched), and the merge is `--ff-only` so the base
/// branch can never gain a merge commit it did not ask for.
pub fn finish(repo: &Path, id: &str, verify: Option<&str>) -> Result<Outcome, String> {
    let worktree = locate(repo, id)?;
    let base = base_ref(repo)?;
    if let Err(error) = git(&worktree.path, &["rebase", &base]) {
        let _ = worktree.write_manifest(
            "needs-merge",
            verify.map(|v| (v, false)),
            Some(&format!("rebase onto {base} failed")),
        );
        return Ok(Outcome::Halted {
            reason: format!("rebase onto {base} conflicted ({error}); the worktree still has it"),
        });
    }
    if let Some(command) = verify {
        let status = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&worktree.path)
            .status()
            .map_err(|error| format!("run the verify command: {error}"))?;
        if !status.success() {
            let _ = worktree.write_manifest(
                "needs-merge",
                Some((command, false)),
                Some("verify command failed"),
            );
            return Ok(Outcome::Halted {
                reason: format!("`{command}` failed in the worktree"),
            });
        }
    }
    // A nested worktree's base is its parent's branch, and that branch is checked
    // out in the parent worktree, not here: merge where the branch lives.
    let target = checkout_of(repo, &base);
    match git(&target, &["merge", "--ff-only", &worktree.branch]) {
        Ok(text) => {
            let commit = git(&target, &["rev-parse", "HEAD"]).unwrap_or_default();
            worktree.archive(Some(&text))?;
            worktree
                .write_manifest("merged", verify.map(|v| (v, true)), None)
                .ok();
            worktree.remove()?;
            Ok(Outcome::Merged { commit })
        }
        Err(error) => {
            let _ = worktree.write_manifest(
                "needs-merge",
                verify.map(|v| (v, true)),
                Some(&format!("fast-forward failed: {error}")),
            );
            Ok(Outcome::Halted {
                reason: format!(
                    "`git merge --ff-only` refused (the base branch has diverged or is dirty): {error}"
                ),
            })
        }
    }
}

impl Worktree {
    /// Copy the patch and manifest out of a finished worktree before it is
    /// removed: the checkout is disposable, the evidence is not.
    fn archive(&self, merge_output: Option<&str>) -> Result<PathBuf, String> {
        let dir = self.repo.join(ARCHIVE).join(&self.id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
        let patch = git(
            &self.path,
            &[
                "format-patch",
                "--stdout",
                &format!("{}..HEAD", self.base_branch()?),
            ],
        )
        .unwrap_or_default();
        std::fs::write(dir.join("change.patch"), patch)
            .map_err(|error| format!("write patch: {error}"))?;
        let manifest = std::fs::read_to_string(self.manifest_path()).unwrap_or_default();
        std::fs::write(dir.join("manifest.json"), manifest)
            .map_err(|error| format!("write manifest: {error}"))?;
        if let Some(output) = merge_output {
            std::fs::write(dir.join("merge.log"), output).ok();
        }
        Ok(dir)
    }

    fn remove(&self) -> Result<(), String> {
        git(
            &self.repo,
            &[
                "worktree",
                "remove",
                "--force",
                &self.path.to_string_lossy(),
            ],
        )?;
        git(&self.repo, &["branch", "-D", &self.branch]).ok();
        Ok(())
    }
}

/// Archive a worktree without merging it, keeping whatever it changed.
pub fn abandon(repo: &Path, id: &str) -> Result<PathBuf, String> {
    let worktree = locate(repo, id)?;
    let dir = worktree.archive(None)?;
    worktree
        .write_manifest("abandoned", None, Some("abandoned by the user"))
        .ok();
    let _ = std::fs::copy(worktree.manifest_path(), dir.join("manifest.json"));
    worktree.remove()?;
    Ok(dir)
}

/// Remove worktrees whose branch is already contained in the base branch.
/// Anything unmerged is left alone — a timer is not evidence that work is done.
pub fn prune(repo: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    let root = repo.join(WORKTREES);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return removed;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "archive" || !entry.path().is_dir() {
            continue;
        }
        let Ok(worktree) = locate(repo, &name) else {
            continue;
        };
        let Ok(base) = base_ref(repo) else { continue };
        // `--is-ancestor` is the only merge test that cannot be fooled by a
        // rebase or a re-created branch.
        if git(
            repo,
            &["merge-base", "--is-ancestor", &worktree.branch, &base],
        )
        .is_ok()
        {
            if worktree.remove().is_ok() {
                removed.push(name);
            }
        }
    }
    removed
}

fn locate(repo: &Path, id: &str) -> Result<Worktree, String> {
    let path = repo.join(WORKTREES).join(id);
    if !path.join(".git").exists() {
        return Err(format!(
            "no worktree `{id}` under {}",
            repo.join(WORKTREES).display()
        ));
    }
    Ok(Worktree {
        id: id.to_string(),
        repo: repo.to_path_buf(),
        path,
        branch: format!("{BRANCH_PREFIX}{id}"),
    })
}

/// `ki worktree status`: one line per live worktree, plus stale warnings.
pub fn status(repo: &Path) -> String {
    let mut out = String::new();
    let root = repo.join(WORKTREES);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return "no worktrees\n".to_string();
    };
    let base = base_ref(repo).unwrap_or_else(|_| "main".to_string());
    let mut rows: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "archive" || !entry.path().is_dir() {
            continue;
        }
        let Ok(worktree) = locate(repo, &name) else {
            continue;
        };
        let files = git(
            &worktree.path,
            &["diff", "--name-only", &format!("{base}...HEAD")],
        )
        .unwrap_or_default()
        .lines()
        .count();
        let age = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .map(|elapsed| elapsed.as_secs() / 86_400)
            .unwrap_or(0);
        let stale = if age >= STALE_DAYS && files > 0 {
            format!("  stale: {age}d unmerged (ki worktree merge {name} | abandon {name})")
        } else {
            String::new()
        };
        rows.push(format!(
            "{name}  branch={}  files={files}{stale}",
            worktree.branch
        ));
    }
    if rows.is_empty() {
        out.push_str("no worktrees\n");
    } else {
        rows.sort();
        for row in rows {
            out.push_str(&row);
            out.push('\n');
        }
    }
    let archived = std::fs::read_dir(repo.join(ARCHIVE))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    if archived > 0 {
        out.push_str(&format!("{archived} archived in {ARCHIVE}\n"));
    }
    out
}

/// The branch a new worktree starts from: this checkout's branch, else
/// `origin/main`, else `main`.
///
/// A `ki/wt-*` branch is a legitimate base when it *is* this checkout's branch:
/// that is a worktree stacking on its parent, and the chain stays linear because
/// the parent merges the child and then merges upward itself. What must never
/// happen is picking some *other* worktree's branch, which is why this reads the
/// current checkout and never "the newest branch in the repository".
pub fn base_ref(repo: &Path) -> Result<String, String> {
    if let Some(env) = std::env::var("KI_WORKTREE_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        return Ok(env);
    }
    if let Ok(branch) = git(repo, &["symbolic-ref", "--short", "HEAD"]) {
        let branch = branch.trim().to_string();
        if !branch.is_empty() {
            return Ok(branch);
        }
    }
    for candidate in ["origin/main", "main"] {
        if git(repo, &["rev-parse", "--verify", "--quiet", candidate]).is_ok() {
            return Ok(candidate.to_string());
        }
    }
    Err("cannot find a base branch: no current branch, no origin/main, no main".to_string())
}

/// The checkout that has `branch` checked out, so a merge can run where that
/// branch lives: a nested worktree's base is its parent's branch, and `git merge`
/// refuses to update a branch that is checked out in another worktree.
fn checkout_of(repo: &Path, branch: &str) -> PathBuf {
    let Ok(text) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return repo.to_path_buf();
    };
    let want = format!("branch refs/heads/{branch}");
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            current = Some(rest.to_string());
        } else if line.trim() == want {
            if let Some(path) = current.take() {
                return PathBuf::from(path);
            }
        }
    }
    repo.to_path_buf()
}

/// The repository that owns the worktrees: the main checkout, even when `cwd`
/// is inside one of them. A nested agent's worktree must land beside its
/// parent's, not inside it, or `locate`/`prune` stop seeing it.
pub fn common_root(cwd: &Path) -> Result<PathBuf, String> {
    let common = git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common.trim());
    if common
        .file_name()
        .map(|name| name == ".git")
        .unwrap_or(false)
    {
        return Ok(common.parent().map(Path::to_path_buf).unwrap_or(common));
    }
    Ok(common)
}

/// The repository root that contains `cwd`.
pub fn repo_root(cwd: &Path) -> Result<PathBuf, String> {
    let text = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(format!("{} is not inside a git repository", cwd.display()));
    }
    Ok(PathBuf::from(trimmed))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn sanitize(id: &str) -> String {
    let mut slug: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    };
    slug.chars().take(40).collect()
}

fn random_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.subsec_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    format!("{:x}{:x}", pid, nanos)
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    format!("{seconds}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ki-worktree-{name}-{}", random_token()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "--quiet", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", "one"]);
        dir
    }

    #[test]
    fn a_worktree_starts_from_the_current_branch_and_merges_back() {
        let dir = repo("merge");
        let worktree = create(&dir, "Review the plan!").unwrap();
        assert_eq!(base_ref(&dir).unwrap(), "main");
        assert!(worktree.branch.starts_with("ki/wt-"));
        std::fs::write(worktree.path.join("b.txt"), "two\n").unwrap();
        assert!(git(&worktree.path, &["add", "."]).is_ok());
        assert!(git(&worktree.path, &["commit", "-m", "two"]).is_ok());

        let outcome = finish(&dir, &worktree.id, None).unwrap();
        assert!(matches!(outcome, Outcome::Merged { .. }), "{outcome:?}");
        assert!(dir.join("b.txt").is_file());
        assert!(!worktree.path.exists());
        // The evidence outlives the checkout.
        let archived = dir.join(ARCHIVE).join(&worktree.id);
        assert!(archived.join("manifest.json").is_file());
        assert!(archived.join("change.patch").is_file());
    }

    #[test]
    fn a_failing_verify_leaves_the_worktree_and_the_base_alone() {
        let dir = repo("verify");
        let worktree = create(&dir, "task").unwrap();
        std::fs::write(worktree.path.join("b.txt"), "two\n").unwrap();
        git(&worktree.path, &["add", "."]).unwrap();
        git(&worktree.path, &["commit", "-m", "two"]).unwrap();

        let outcome = finish(&dir, &worktree.id, Some("false")).unwrap();
        assert!(matches!(outcome, Outcome::Halted { .. }));
        assert!(
            worktree.path.exists(),
            "a failed verify must not lose the work"
        );
        assert!(
            !dir.join("b.txt").exists(),
            "the base branch must stay clean"
        );
    }

    #[test]
    fn a_conflicting_rebase_halts_instead_of_touching_the_base() {
        let dir = repo("conflict");
        let worktree = create(&dir, "task").unwrap();
        std::fs::write(worktree.path.join("a.txt"), "from the worktree\n").unwrap();
        git(&worktree.path, &["add", "."]).unwrap();
        git(&worktree.path, &["commit", "-m", "wt"]).unwrap();
        // Move the base under it.
        std::fs::write(dir.join("a.txt"), "from main\n").unwrap();
        git(&dir, &["add", "."]).unwrap();
        git(&dir, &["commit", "-m", "main"]).unwrap();

        let outcome = finish(&dir, &worktree.id, None).unwrap();
        assert!(matches!(outcome, Outcome::Halted { .. }), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "from main\n"
        );
        assert!(worktree.path.exists());
    }

    #[test]
    fn prune_only_takes_worktrees_already_in_the_base() {
        let dir = repo("prune");
        let merged = create(&dir, "merged").unwrap();
        std::fs::write(merged.path.join("b.txt"), "two\n").unwrap();
        git(&merged.path, &["add", "."]).unwrap();
        git(&merged.path, &["commit", "-m", "two"]).unwrap();
        assert!(matches!(
            finish(&dir, &merged.id, None).unwrap(),
            Outcome::Merged { .. }
        ));

        let open = create(&dir, "open").unwrap();
        std::fs::write(open.path.join("c.txt"), "three\n").unwrap();
        git(&open.path, &["add", "."]).unwrap();
        git(&open.path, &["commit", "-m", "three"]).unwrap();

        prune(&dir);
        assert!(open.path.exists(), "an unmerged worktree is never pruned");
        assert!(!merged.path.exists());
    }

    #[test]
    fn base_ref_follows_this_checkout_and_ignores_other_worktrees() {
        let dir = repo("base");
        git(&dir, &["checkout", "--quiet", "-b", "feature"]).unwrap();
        assert_eq!(base_ref(&dir).unwrap(), "feature");
        let tree = create(&dir, "task").unwrap();
        assert!(tree.branch.starts_with(BRANCH_PREFIX));
        // Another worktree's branch must never become the base, however new it
        // is: reading the checkout we are in is what keeps the chain linear.
        assert_eq!(base_ref(&dir).unwrap(), "feature");
        assert!(tree.branch.starts_with(BRANCH_PREFIX));
        git(
            &dir,
            &[
                "worktree",
                "remove",
                "--force",
                &tree.path.to_string_lossy(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn a_base_branch_is_merged_where_it_is_checked_out() {
        let dir = repo("checkout");
        let tree = create(&dir, "task").unwrap();
        assert_eq!(
            checkout_of(&dir, "main").canonicalize().unwrap(),
            dir.canonicalize().unwrap()
        );
        // A nested worktree bases on its parent's branch, and `git merge`
        // refuses to update a branch that another checkout has: the merge has
        // to happen in the worktree that owns it.
        assert_eq!(
            checkout_of(&dir, &tree.branch).canonicalize().unwrap(),
            tree.path.canonicalize().unwrap()
        );
        std::fs::write(tree.path.join("b.txt"), "two\n").unwrap();
        git(&tree.path, &["add", "."]).unwrap();
        git(&tree.path, &["commit", "-m", "two"]).unwrap();
        assert!(matches!(
            finish(&dir, &tree.id, None).unwrap(),
            Outcome::Merged { .. }
        ));
    }

    #[test]
    fn a_worktree_started_from_a_worktree_lands_in_the_same_repository() {
        let dir = repo("nesting");
        let parent = create(&dir, "parent").unwrap();
        // The child inherits the parent's checkout as its cwd.
        assert_eq!(
            common_root(&parent.path).unwrap().canonicalize().unwrap(),
            dir.canonicalize().unwrap()
        );
        assert_eq!(
            base_ref(&parent.path).unwrap(),
            parent.branch,
            "a nested worktree stacks on the branch that spawned it"
        );
    }
}
