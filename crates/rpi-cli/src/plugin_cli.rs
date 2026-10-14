//! `ki plugin` — inspect and edit the user's per-plugin capability grants.
//!
//! The grant store is the user's authorization (see
//! [`kiqi_plugin::grants`]); the host enforces it and this command is how the
//! user sets it. Nothing here is wasm-specific: the same files govern a wasm
//! plugin's `HostCaps`.

use kiqi_plugin::grants::{grants_path, installed_plugins, Grants};

/// Entry for `ki plugin [sub]`; `args` is the slice after `plugin`.
pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list") => list(),
        Some("show") => show(args.get(1).map(String::as_str)),
        Some("grant") => grant(&args[1..]),
        Some("revoke") => revoke(&args[1..]),
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
    0
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
    ki plugin grant <id> <capability> [--host <host>]...
    ki plugin revoke <id> <capability>

CAPABILITIES:
    net-http      host-mediated HTTP; --host <host> (repeatable, `*` allows any)
    fs            filesystem, scoped to a plugin sandbox root
    exec          run a child process (host allow-list)
    timer         host-driven ticks
    net-sockets   raw TCP sockets

A missing grants file grants nothing; the host never authorizes for a plugin."
    );
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
}
