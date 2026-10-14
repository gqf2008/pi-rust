//! `ki plugin` — inspect and edit the user's per-plugin capability grants.
//!
//! The grant store is the user's authorization (see
//! [`kiqi_plugin::grants`]); the host enforces it and this command is how the
//! user sets it. Nothing here is wasm-specific: the same files govern a wasm
//! plugin's `HostCaps`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use kiqi_plugin::grants::{agent_dir, grants_path, installed_plugins, plugin_dir_at, Grants};

/// Entry for `ki plugin [sub]`; `args` is the slice after `plugin`.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("show") => show(args.get(1).map(String::as_str)),
        Some("grant") => grant(&args[1..]),
        Some("revoke") => revoke(&args[1..]),
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(&args[1..]),
        None | Some("--help") | Some("-h") | Some("help") => {
            print_help();
            0
        }
        Some(other) => {
            eprintln!("error: unknown `ki plugin` subcommand `{other}`");
            print_help();
            2
        }
    }
}

fn list() -> i32 {
    let plugins = installed_plugins();
    if plugins.is_empty() {
        println!("no plugins installed");
        return 0;
    }
    for id in plugins {
        let granted = Grants::load_for(&id).granted();
        if granted.is_empty() {
            println!("{id}: (nothing granted)");
        } else {
            println!("{id}: {}", granted.join(", "));
        }
    }
    0
}

fn show(id: Option<&str>) -> i32 {
    let Some(id) = id else {
        eprintln!("error: `ki plugin show <id>` needs a plugin id");
        return 2;
    };
    let grants = Grants::load_for(id);
    match grants_path(id) {
        Some(path) => println!("{id}  ({})", path.display()),
        None => println!("{id}"),
    }
    match serde_json::to_string_pretty(&grants) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("error: {error}"),
    }
    // Declarations, not grants: what the plugin claims it contributes. This is
    // the consumer for `flags`/`shortcuts`, whose runtime paths are
    // `SessionRead::cli_flag` and an `Input` event respectively — the
    // declaration itself is only ever read here.
    print_declared(id);
    0
}

/// List a plugin's declared surfaces, or say why nothing could be listed.
fn print_declared(id: &str) {
    let host = crate::plugins::compiled_plugin_hosts()
        .into_iter()
        .find(|host| host.id() == id);
    match host {
        Some(host) => println!("\n{}", declared_surfaces(host.plugin())),
        None => eprintln!(
            "note: no compiled or wasm plugin with id `{id}` is installed, so its declared \
             surfaces cannot be listed"
        ),
    }
}

/// Render the surfaces a plugin declares. Split from the printing so it can be
/// asserted on.
fn declared_surfaces(plugin: &dyn kiqi_plugin::Plugin) -> String {
    let mut out = String::new();
    let mut section = |label: &str, items: Vec<String>| {
        if items.is_empty() {
            return;
        }
        out.push_str(&format!("{label}:\n"));
        for item in items {
            out.push_str(&format!("  {item}\n"));
        }
    };
    section(
        "tools",
        plugin
            .tools()
            .iter()
            .map(|spec| spec.name.clone())
            .collect(),
    );
    section(
        "commands",
        plugin
            .commands()
            .iter()
            .map(|spec| {
                format!(
                    "/{} \u{2014} {}",
                    spec.name.trim_start_matches('/'),
                    spec.description
                )
            })
            .collect(),
    );
    section(
        "flags",
        plugin
            .flags()
            .iter()
            .map(|spec| format!("--{} \u{2014} {}", spec.name, spec.description))
            .collect(),
    );
    section(
        "shortcuts",
        plugin
            .shortcuts()
            .iter()
            .map(|spec| format!("{} \u{2014} {}", spec.key, spec.description))
            .collect(),
    );
    section("probe points", plugin.probe_points());
    out
}

fn grant(args: &[String]) -> i32 {
    let parsed = match parse_target(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let mut grants = Grants::load_for(&parsed.id);
    if let Err(error) = grants.grant(&parsed.capability, parsed.hosts) {
        eprintln!("error: {error}");
        return 2;
    }
    if let Err(error) = Grants::save_for(&parsed.id, &grants) {
        eprintln!("error: {error}");
        return 1;
    }
    println!("granted `{}` to {}", parsed.capability, parsed.id);
    0
}

fn revoke(args: &[String]) -> i32 {
    let parsed = match parse_target(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let mut grants = Grants::load_for(&parsed.id);
    if let Err(error) = grants.revoke(&parsed.capability) {
        eprintln!("error: {error}");
        return 2;
    }
    if let Err(error) = Grants::save_for(&parsed.id, &grants) {
        eprintln!("error: {error}");
        return 1;
    }
    println!("revoked `{}` from {}", parsed.capability, parsed.id);
    0
}

struct Target {
    id: String,
    capability: String,
    hosts: Vec<String>,
}

/// `<id> <capability> [--host H]...`, shared by grant and revoke.
fn parse_target(args: &[String]) -> Result<Target, i32> {
    let mut id = None;
    let mut capability = None;
    let mut hosts = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--host" => match iter.next() {
                Some(host) => hosts.push(host.clone()),
                None => {
                    eprintln!("error: --host needs a value");
                    return Err(2);
                }
            },
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag `{other}`");
                return Err(2);
            }
            other if id.is_none() => id = Some(other.to_string()),
            other if capability.is_none() => capability = Some(other.to_string()),
            other => {
                eprintln!("error: unexpected argument `{other}`");
                return Err(2);
            }
        }
    }
    let (Some(id), Some(capability)) = (id, capability) else {
        eprintln!("error: expected `ki plugin <grant|revoke> <id> <capability> [--host H]...`");
        return Err(2);
    };
    Ok(Target {
        id,
        capability,
        hosts,
    })
}

fn print_help() {
    println!(
        "\
ki plugin — manage per-plugin capability grants

USAGE:
    ki plugin list
    ki plugin show <id>
    ki plugin install <path.wasm> [--id <id>] [--yes] [--host <host>]...
    ki plugin uninstall <id>
    ki plugin grant <id> <capability> [--host <host>]...
    ki plugin revoke <id> <capability>

CAPABILITIES:
    net-http      host-mediated HTTP; --host <host> (repeatable, `*` allows any)
    fs            filesystem, scoped to a plugin sandbox root
    exec          run a child process (host allow-list)
    net-sockets   raw TCP sockets

`install` copies the component to `plugins/<id>/plugin.wasm`, shows the
capabilities it declares, and grants only what the user approves (`--yes` for a
non-interactive run; without a terminal and without `--yes`, nothing is
granted). `uninstall` moves the directory aside instead of deleting it.

A missing grants file grants nothing; the host never authorizes for a plugin."
    );
}

/// The agent root for this invocation: an explicit one (a test, or a caller
/// handed a directory) or the environment's. Resolving it here — rather than
/// reading `KI_AGENT_DIR` deep inside — is what keeps tests from mutating the
/// process environment.
fn resolve_root(root: Option<&Path>) -> Option<PathBuf> {
    root.map(Path::to_path_buf).or_else(agent_dir)
}

/// Whether to grant the capabilities an install newly declares.
///
/// Split out because the interactive branch is invisible to CI (no terminal):
/// the decision is the part that matters, and it is testable on its own.
/// Anything short of an explicit `y`/`Y`/`yes` — an empty line, `n`, EOF — is a
/// no, so the default answer is to grant nothing.
fn approve_new_capabilities(yes: bool, interactive: bool, answer: Option<&str>) -> bool {
    if yes {
        return true;
    }
    if !interactive {
        return false;
    }
    matches!(answer.map(str::trim), Some("y" | "Y" | "yes"))
}

/// `ki plugin install <path.wasm> [--id ID] [--yes] [--host H]...`
fn install(args: &[String]) -> i32 {
    install_into(args, None)
}

/// [`install`] against an explicit agent root (`None` = the environment's).
///
/// Taking the root as a parameter is why no test has to write `KI_AGENT_DIR`
/// into the process environment, which is unsafe while other threads read it.
fn install_into(args: &[String], root: Option<&Path>) -> i32 {
    let mut wasm: Option<String> = None;
    let mut id: Option<String> = None;
    let mut yes = false;
    let mut hosts: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--id" => match iter.next() {
                Some(value) => id = Some(value.clone()),
                None => {
                    eprintln!("error: --id needs a value");
                    return 2;
                }
            },
            "--yes" | "-y" => yes = true,
            "--host" => match iter.next() {
                Some(value) => hosts.push(value.clone()),
                None => {
                    eprintln!("error: --host needs a value");
                    return 2;
                }
            },
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag `{other}`");
                return 2;
            }
            other if wasm.is_none() => wasm = Some(other.to_string()),
            other => {
                eprintln!("error: unexpected argument `{other}`");
                return 2;
            }
        }
    }
    let Some(wasm) = wasm else {
        eprintln!("error: expected `ki plugin install <path.wasm> [--id <id>] [--yes] [--host <host>]...`");
        return 2;
    };
    let wasm = Path::new(&wasm);
    if !wasm.is_file() {
        eprintln!("error: {} is not a file", wasm.display());
        return 2;
    }

    let id = id.unwrap_or_else(|| {
        wasm.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("plugin")
            .to_string()
    });
    let declared = match kiqi_plugin_host::declared_capabilities(wasm) {
        Ok(caps) => caps,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };

    let Some(root) = resolve_root(root) else {
        eprintln!("error: cannot resolve the agent directory");
        return 1;
    };
    let dir = plugin_dir_at(&root, &id);
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("error: create {}: {error}", dir.display());
        return 1;
    }
    let target = dir.join("plugin.wasm");
    if let Err(error) = std::fs::copy(wasm, &target) {
        eprintln!("error: install {}: {error}", target.display());
        return 1;
    }

    // Only capabilities this plugin declares that are not already granted need a
    // decision, so an update that does not grow the set never re-prompts.
    let mut grants = Grants::load_for_at(&root, &id);
    let already = grants.granted();
    let new_caps: Vec<&String> = declared
        .iter()
        .filter(|cap| !already.contains(&cap.as_str()))
        .collect();
    let mut granted_now: Vec<String> = Vec::new();
    if !new_caps.is_empty() {
        let interactive = std::io::stdin().is_terminal();
        if !yes && interactive {
            print!(
                "grant {} to `{id}`? [y/N] ",
                new_caps
                    .iter()
                    .map(|cap| cap.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        let answer = if !yes && interactive {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok().map(|_| line)
        } else {
            None
        };
        let approve = approve_new_capabilities(yes, interactive, answer.as_deref());
        if approve {
            for cap in &new_caps {
                match grants.grant(cap, hosts.clone()) {
                    Ok(()) => granted_now.push(cap.to_string()),
                    Err(error) => eprintln!("note: {cap}: {error}"),
                }
            }
            if !granted_now.is_empty() {
                if let Err(error) = Grants::save_for_at(&root, &id, &grants) {
                    eprintln!("error: {error}");
                    return 1;
                }
            }
        }
    }

    println!("installed `{id}` -> {}", target.display());
    if !declared.is_empty() {
        println!("declared capabilities: {}", declared.join(", "));
    }
    if !granted_now.is_empty() {
        println!("granted: {}", granted_now.join(", "));
    }
    let granted = grants.granted();
    let missing: Vec<&str> = declared
        .iter()
        .map(String::as_str)
        .filter(|cap| !granted.contains(cap))
        .collect();
    if !missing.is_empty() {
        println!(
            "not granted: {} — use `ki plugin grant {id} <capability>`",
            missing.join(", ")
        );
    }
    0
}

/// `ki plugin uninstall <id>` — moves the plugin directory aside rather than
/// deleting it, so the files stay recoverable.
fn uninstall(args: &[String]) -> i32 {
    uninstall_into(args, None)
}

/// [`uninstall`] against an explicit agent root.
fn uninstall_into(args: &[String], root: Option<&Path>) -> i32 {
    let Some(id) = args.first().map(String::as_str) else {
        eprintln!("error: `ki plugin uninstall <id>` needs a plugin id");
        return 2;
    };
    let Some(root) = resolve_root(root) else {
        eprintln!("error: cannot resolve the agent directory");
        return 1;
    };
    let dir = plugin_dir_at(&root, id);
    if !dir.is_dir() {
        eprintln!("error: `{id}` is not installed");
        return 1;
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let backup = dir.with_file_name(format!("{id}.removed-{timestamp}"));
    if let Err(error) = std::fs::rename(&dir, &backup) {
        eprintln!("error: move {}: {error}", dir.display());
        return 1;
    }
    println!("uninstalled `{id}` (moved to {})", backup.display());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_without_a_capability_is_a_usage_error() {
        assert_eq!(run(&["grant".into(), "some-plugin".into()]), 2);
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        assert_eq!(run(&["nope".into()]), 2);
    }

    #[test]
    fn host_flag_without_a_value_is_a_usage_error() {
        let args = vec![
            "p".to_string(),
            "net-http".to_string(),
            "--host".to_string(),
        ];
        assert!(parse_target(&args).is_err());
    }

    /// Installing copies the component, grants `--yes` only what the component
    /// declares, and uninstalling moves the directory aside.
    ///
    /// Runs against an explicit root rather than by writing `KI_AGENT_DIR`:
    /// mutating the process environment is unsafe while other test threads read
    /// it, and a lock only serializes writers, not readers.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn install_and_uninstall_round_trip() {
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = workspace.join("plugins/wasm/kiqi-conformance-guest/Cargo.toml");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .args(["--target", "wasm32-wasip2", "--release"])
            .status()
            .expect("build the conformance guest");
        assert!(status.success(), "is the `wasm32-wasip2` target installed?");
        let wasm =
            workspace.join("plugins/wasm/target/wasm32-wasip2/release/kiqi_conformance_guest.wasm");

        let agent = tempfile::tempdir().unwrap();
        let root = agent.path();

        let code = install_into(
            &[
                wasm.display().to_string(),
                "--id".into(),
                "conformance".into(),
                "--yes".into(),
                "--host".into(),
                "127.0.0.1".into(),
            ],
            Some(root),
        );
        assert_eq!(code, 0);
        assert!(root.join("plugins/conformance/plugin.wasm").is_file());
        let grants = Grants::load_for_at(root, "conformance");
        assert_eq!(grants.granted(), vec!["net-http"]);
        assert!(grants.allows_host("127.0.0.1"));

        let code = uninstall_into(&["conformance".into()], Some(root));
        assert_eq!(code, 0);
        assert!(!root.join("plugins/conformance").exists());
        let moved = std::fs::read_dir(root.join("plugins"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map(|name| name.starts_with("conformance.removed-"))
                    .unwrap_or(false)
            });
        assert!(moved, "uninstall keeps the files as a backup");
    }

    /// The interactive answer path — unreachable in CI, which has no terminal —
    /// decided on its own. Only an explicit yes grants; the default is no.
    #[test]
    fn install_grants_only_on_an_explicit_yes() {
        assert!(approve_new_capabilities(true, false, None), "--yes");
        assert!(
            !approve_new_capabilities(false, false, None),
            "headless without --yes"
        );
        for answer in ["y\n", "Y\n", "yes\n", "  y  "] {
            assert!(
                approve_new_capabilities(false, true, Some(answer)),
                "{answer:?}"
            );
        }
        for answer in ["\n", "n\n", "no\n", "nah\n"]
            .into_iter()
            .map(Some)
            .chain([None])
        {
            assert!(!approve_new_capabilities(false, true, answer), "{answer:?}");
        }
    }

    /// `ki plugin show` is the consumer for the `flags`/`shortcuts`
    /// declarations: they have no runtime path of their own (a flag's value is
    /// read through `cli_flag`, a key arrives as an `Input` event), so this is
    /// where they become observable.
    #[cfg(feature = "plugin-wasm")]
    #[test]
    fn declared_surfaces_include_the_guests_flags_and_shortcuts() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifest = root.join("plugins/wasm/kiqi-conformance-guest/Cargo.toml");
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .args(["--target", "wasm32-wasip2", "--release"])
            .status()
            .expect("build the conformance guest");
        assert!(status.success(), "is the `wasm32-wasip2` target installed?");
        let wasm =
            root.join("plugins/wasm/target/wasm32-wasip2/release/kiqi_conformance_guest.wasm");
        let plugin = kiqi_plugin_host::WasmPlugin::load_with_grants(
            "conformance",
            &wasm,
            kiqi_plugin::grants::Grants::default(),
        )
        .expect("load the conformance guest");

        let text = declared_surfaces(&plugin);
        for expected in [
            "commands:",
            "flags:",
            "--conformance-flag",
            "shortcuts:",
            "f8",
            "probe points:",
            "before-tool-call",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
    }
}
