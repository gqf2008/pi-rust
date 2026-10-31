use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const RELEASE_CRATES: &[(&str, &str)] = &[
    ("kiqi-telemetry", "crates/kiqi-telemetry/Cargo.toml"),
    ("kiqi-ai", "crates/kiqi-ai/Cargo.toml"),
    ("kiqi-agent", "crates/kiqi-agent/Cargo.toml"),
    ("kiqi-host", "crates/kiqi-host/Cargo.toml"),
    ("kiqi-tools", "crates/kiqi-tools/Cargo.toml"),
    ("kiqi-harness", "crates/kiqi-harness/Cargo.toml"),
    ("kiqi-tui", "crates/kiqi-tui/Cargo.toml"),
    ("kiqi-mcp", "crates/kiqi-mcp/Cargo.toml"),
    ("kiqi-acp", "crates/kiqi-acp/Cargo.toml"),
    ("kiqi-cli", "crates/kiqi-cli/Cargo.toml"),
];

const INTERNAL_DEPENDENCIES: &[&str] = &[
    "kiqi-telemetry",
    "kiqi-ai",
    "kiqi-agent",
    "kiqi-host",
    "kiqi-tools",
    "kiqi-harness",
    "kiqi-tui",
    "kiqi-mcp",
    "kiqi-acp",
];

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    match args.as_slice() {
        [command, version] if command == "validate" => validate(version, false),
        [command, version, flag] if command == "validate" && flag == "--allow-dirty" => {
            validate(version, true)
        }
        _ => Err("usage: release-helper <validate VERSION [--allow-dirty]>".to_string()),
    }
}

fn validate(expected: &str, allow_dirty: bool) -> Result<(), String> {
    if !allow_dirty {
        let status = command_output(
            "git",
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        if !status.stdout.is_empty() {
            return Err("release requires a clean git worktree (including untracked files)".into());
        }
    }

    let metadata = command_output(
        "cargo",
        &["metadata", "--no-deps", "--format-version", "1", "--locked"],
    )
    .map_err(|error| format!("cargo metadata --locked failed: {error}"))?;
    let metadata = String::from_utf8(metadata.stdout)
        .map_err(|_| "cargo metadata returned non-UTF-8 JSON".to_string())?;
    let packages = metadata
        .split_once("\"packages\":[")
        .and_then(|(_, rest)| {
            rest.split_once("],\"workspace_members\"")
                .map(|(value, _)| value)
        })
        .ok_or_else(|| "cargo metadata returned an unexpected format".to_string())?;

    let root = read("Cargo.toml")?;
    let workspace_package = toml_section(&root, "workspace.package")
        .ok_or_else(|| "Cargo.toml has no [workspace.package] section".to_string())?;
    let workspace_version = toml_string(workspace_package, "version")
        .ok_or_else(|| "[workspace.package] has no string version".to_string())?;
    if workspace_version != expected {
        return Err(format!(
            "workspace release version {workspace_version} does not match requested RELEASE_VERSION {expected}"
        ));
    }

    let workspace_dependencies = toml_section(&root, "workspace.dependencies")
        .ok_or_else(|| "Cargo.toml has no [workspace.dependencies] section".to_string())?;
    for dependency in INTERNAL_DEPENDENCIES {
        let line = toml_assignment(workspace_dependencies, dependency)
            .ok_or_else(|| format!("missing workspace dependency {dependency}"))?;
        let version = inline_table_string(line, "version")
            .ok_or_else(|| format!("workspace dependency {dependency} has no version"))?;
        if version != expected {
            return Err(format!(
                "workspace dependency {dependency} uses {version}, expected {expected}"
            ));
        }
    }

    for (crate_name, manifest_path) in RELEASE_CRATES {
        let marker = format!("{{\"name\":\"{crate_name}\",\"version\":\"{expected}\"");
        if !packages.contains(&marker) {
            return Err(format!(
                "cargo metadata does not list {crate_name} {expected} as a workspace package"
            ));
        }
        let manifest = read(manifest_path)?;
        let package = toml_section(&manifest, "package")
            .ok_or_else(|| format!("{manifest_path} has no [package] section"))?;
        let name = toml_string(package, "name")
            .ok_or_else(|| format!("{manifest_path} has no package name"))?;
        if name != *crate_name {
            return Err(format!(
                "{manifest_path} declares package {name}, expected {crate_name}"
            ));
        }
        validate_internal_dependencies(crate_name, &manifest, expected)?;

        let inherits_version = toml_assignment(package, "version.workspace")
            .is_some_and(|line| assignment_value(line) == Some("true"));
        let explicit_version = toml_string(package, "version");
        if !inherits_version && explicit_version != Some(expected) {
            return Err(format!(
                "{crate_name} must inherit the workspace version or explicitly use {expected}"
            ));
        }
    }

    println!("Validated clean release state for kiqi-* {expected}.");
    Ok(())
}

fn validate_internal_dependencies(
    crate_name: &str,
    manifest: &str,
    expected: &str,
) -> Result<(), String> {
    for line in manifest.lines() {
        let Some((key, _)) = line.trim().split_once('=') else {
            continue;
        };
        let dependency = key.trim();
        if !INTERNAL_DEPENDENCIES.contains(&dependency) && dependency != "kiqi-cli" {
            continue;
        }
        if line.contains("workspace = true")
            || inline_table_string(line, "version") == Some(expected)
        {
            continue;
        }
        return Err(format!(
            "{crate_name} dependency {dependency} must inherit or use version {expected}"
        ));
    }
    Ok(())
}

fn read(path: impl AsRef<Path>) -> Result<String, String> {
    let path = path.as_ref();
    fs::read_to_string(path).map_err(|error| format!("could not read {}: {error}", path.display()))
}

fn toml_section<'a>(toml: &'a str, name: &str) -> Option<&'a str> {
    let marker = format!("[{name}]");
    let start = toml.find(&marker)? + marker.len();
    let rest = &toml[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    Some(&rest[..end])
}

fn toml_assignment<'a>(section: &'a str, key: &str) -> Option<&'a str> {
    section.lines().find(|line| {
        let line = line.trim();
        !line.starts_with('#')
            && line
                .split_once('=')
                .is_some_and(|(candidate, _)| candidate.trim() == key)
    })
}

fn assignment_value(line: &str) -> Option<&str> {
    line.split_once('=').map(|(_, value)| value.trim())
}

fn toml_string<'a>(section: &'a str, key: &str) -> Option<&'a str> {
    let value = assignment_value(toml_assignment(section, key)?)?;
    quoted_value(value)
}

fn inline_table_string<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let table = assignment_value(line)?;
    let marker = format!("{key} =");
    let rest = table
        .get(table.find(&marker)? + marker.len()..)?
        .trim_start();
    quoted_value(rest)
}

fn quoted_value(value: &str) -> Option<&str> {
    let value = value.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(&value[..end])
}

fn command_output(program: &str, args: &[&str]) -> Result<Output, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}
