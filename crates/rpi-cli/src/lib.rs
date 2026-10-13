//! `pi-cli` — a terminal coding-agent CLI built on the ki library crates.
//!
//! It provides the argument surface, provider/model resolution, harness
//! construction, output modes, and the interactive TUI that a harness-backed
//! CLI needs.
//!
//! # Surface
//!
//! - **Argument parsing** ([`args`]) — `--provider`/`--model`/`--api-key`,
//!   `--thinking`, `--print`/`-p`, `--mode {text,json}`, `-c`/`--continue`,
//!   `-r`/`--resume`, `--session`/`--session-dir`/`--no-session`, `--tools`/
//!   `-t`, `--exclude-tools`/`-xt`, `--no-tools`, `--no-builtin-tools`,
//!   `--system-prompt`, `--append-system-prompt`, `--name`/`-n`, `--verbose`,
//!   `--help`/`-h`, `--version`/`-v`, positional `messages`, and `@file`
//!   attachments.
//! - **Provider/model resolution** ([`provider`]) — API key from `--api-key`
//!   or the provider's environment variable; model pattern
//!   `provider/id[:thinking]` resolved against the provider's catalog.
//! - **Harness construction + run loop** ([`session`]) — `OsExecutionEnv` +
//!   built-in `read`/`write`/`edit`/`bash` tools, durable JSONL session storage,
//!   `AgentHarness`, and the `prompt_text → outcome` run.
//! - **Output modes** ([`modes`]) — `print` (text, single-shot) and `json`
//!   (newline-delimited harness events), plus a simple interactive REPL.
//! - **`app`** ([`app`]) — argument dispatch, model resolution, harness build,
//!   mode dispatch, exit codes.
//!
//! Resources load from `.ki/` and `~/.ki/agent/`; extensions are Rust
//! `cdylib` plugins installed and developed with `ki install` / `ki dev`.

pub mod agent_session;
pub mod app;
pub mod args;
pub mod auth;
pub mod auth_guidance;
pub mod brand;
pub mod changelog;
pub mod config;
pub mod configure;
pub mod dev_extension;
pub mod docs_tool;
pub mod events;
pub mod experimental;
pub mod export;
pub mod extension_api;
pub mod extensions_actions;
pub mod extras;
pub mod fs_watch;
pub mod install;
pub mod interactive_tui;
pub mod key_trace;
pub mod llama_command;
pub mod model_registry;
pub mod modes;
pub mod packages;
pub mod provider;
pub mod plugins;
pub mod provider_config;
pub mod remote;
pub mod resource_dirs;
pub mod resume_picker;
pub mod session;
pub mod session_cwd;
pub mod session_driver;
pub mod settings;
pub mod source_info;
pub mod timings;
mod transcript_navigation;
pub mod transcript_view;
pub mod trust;
pub mod tui_shell;
pub mod updates;

/// Crate version, surfaced by `ki --version`. Mirrors the TS `VERSION` export
/// (sourced from `package.json`; here from `env!("CARGO_PKG_VERSION")`).
///
/// Aliases [`rpi_plugin_sdk::HOST_VERSION`] so the version the CLI prints and
/// the version handed to plugins (and, through it, the version extensions stamp
/// on their own telemetry) can never disagree.
pub const VERSION: &str = rpi_plugin_sdk::HOST_VERSION;

/// The application name used in help + version output. Mirrors TS `APP_NAME`
/// (the TS bin is `"pi"`; the Rust crate publishes under the `rpi-` namespace,
/// so the binary + displayed name is `"ki"` to match).
///
/// Aliases [`rpi_plugin_sdk::HOST_NAME`] — the plugin contract owns the
/// identity, so an extension can label its output (a trace name, a tag) without
/// hardcoding a brand that a rename would silently invalidate.
pub const APP_NAME: &str = rpi_plugin_sdk::HOST_NAME;

/// The executable name, as typed on the command line (`ki`). Distinct from
/// [`APP_NAME`], the product/brand name (`kiqi`) used in the lockup and title.
pub const COMMAND_NAME: &str = "ki";
