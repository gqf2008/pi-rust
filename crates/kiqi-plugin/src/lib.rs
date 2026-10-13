//! kiqi plugin SDK — the **compile-time** plugin contract.
//!
//! A plugin is a Rust crate that implements [`Plugin`] and is compiled into the
//! `ki` binary behind a cargo feature. There is no dynamic ABI, no `unsafe`,
//! no `dlopen`, and no ABI version to break: the compiler checks the whole
//! plugin surface at build time.
//!
//! # Why not the wasm component model
//!
//! An earlier draft defined this contract in WIT. It was dropped because the
//! constraints that shape WIT — cross-language guests, no stable ABI, a
//! canonical ABI that cannot express recursion — do not apply to a contract
//! that only Rust implements. Tools that stream partial output, events that can
//! be claimed, and 23 typed runtime actions all become awkward in WIT and
//! trivial here.
//!
//! The wasm path is not gone, it is **deferred**: the same [`Plugin`] trait is
//! the seam a future `WasmPlugin` adapter would implement, for the one case
//! this design cannot serve — running *untrusted third-party* plugins in a
//! sandbox.
//!
//! # What a plugin can reach
//!
//! Nothing it is not handed. The host passes a [`HostCaps`] per call, built
//! from exactly the capabilities that plugin is allowed to use. This mirrors
//! the wasm model's "an import that is not linked is not reachable", expressed
//! as Rust types instead of a linker.
//!
//! # Native plugins are still supported
//!
//! The `cdylib` (`rpi-plugin-sdk`) channel stays for the **development loop**
//! (`ki dev` + watch + `/reload`): compile-time plugins cannot be hot-swapped,
//! so the dylib channel keeps serving iteration while first-party plugins ship
//! compiled in.

use serde_json::Value;

/// A tool the plugin contributes to the model.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: Value,
}

/// One block of tool output.
#[derive(Debug, Clone)]
pub enum Block {
    Text(String),
    /// A non-text result (already-shaped JSON the host wraps as needed).
    Json(Value),
}

/// The terminal result of a tool call.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub blocks: Vec<Block>,
    pub is_error: bool,
    /// Structured payload the host persists with the tool result and hands
    /// back in [`Event::ToolResult`].
    ///
    /// This is not decoration: `rpi-todo` keeps its whole task list here, so
    /// the state rides the session branch and survives fork/switch. Without a
    /// details channel a plugin has to invent its own side file and loses the
    /// branch-scoped semantics.
    pub details: Value,
}

impl ToolOutcome {
    /// A successful text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![Block::Text(text.into())],
            is_error: false,
            details: Value::Null,
        }
    }

    /// A failed call; the runtime reports it to the model as a tool error
    /// rather than a crash.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            blocks: vec![Block::Text(text.into())],
            is_error: true,
            details: Value::Null,
        }
    }
}

/// Partial output pushed **while** a tool is still running.
///
/// The native ABI expressed this with a host-provided callback function
/// pointer; here it is a sink the plugin writes to, which is the same thing
/// without the FFI.
pub enum Progress {
    /// Append a line the user should see now.
    Text(String),
    /// Structured progress (counts, percentage, stage name).
    Json(Value),
}

/// Where a running tool writes its partial output.
pub trait ProgressSink {
    fn progress(&self, progress: Progress);
}

/// A no-op sink, for callers that do not want progress.
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn progress(&self, _progress: Progress) {}
}

/// A slash command the plugin contributes (e.g. `/todos`).
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
}

/// A plugin failure.
///
/// Distinct from [`ToolOutcome::error`]: this is "the plugin could not do its
/// job", not "the tool ran and reported a failure the model should read".
#[derive(Debug)]
pub enum PluginError {
    /// The model invoked a tool this plugin never declared.
    UnknownTool(String),
    /// The arguments did not match the tool's schema.
    InvalidArguments(String),
    /// The plugin reached for a capability it was not granted.
    Denied(String),
    /// Anything else.
    Failed(String),
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool(name) => write!(f, "unknown tool `{name}`"),
            Self::InvalidArguments(detail) => write!(f, "invalid arguments: {detail}"),
            Self::Denied(detail) => write!(f, "denied: {detail}"),
            Self::Failed(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for PluginError {}

/// One runtime event delivered to plugins.
///
/// Mirrors the host's event tags. Kinds the host adds later arrive as
/// [`Event::Other`] so an old plugin never fails on a new event.
#[derive(Debug, Clone)]
pub enum Event {
    SessionStart,
    /// The session tree changed (fork / navigate / switch). `rpi-todo` restores
    /// its branch-scoped state here, so it cannot be folded into `SessionStart`.
    SessionTree,
    SessionShutdown,
    Message,
    ToolCall,
    /// A tool finished. Carries what a plugin needs to re-derive its state:
    /// which tool, the result, and the `details` that tool returned.
    ToolResult {
        tool: String,
        result: Value,
        details: Value,
    },
    Error,
    /// Forward compatibility hatch, as in the native event ABI.
    Other {
        kind: String,
        payload: Value,
    },
}

/// What a plugin decided to do about an event.
///
/// The native ABI returned an `i32` with `CONTINUE` / `CLAIMED`; this is the
/// same two outcomes with the magic numbers replaced by a type.
#[derive(Debug, Clone, PartialEq)]
pub enum EventAction {
    /// Observed; pass the event on to the next plugin and to normal handling.
    Continue,
    /// Consumed: no later plugin sees it and normal handling is skipped.
    Claimed,
    /// Handled, but replaced the payload for whatever runs next.
    Replace(Value),
}

/// A permission gate's verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// No opinion; let the action proceed.
    Continue,
    /// Rewrite the payload before it proceeds (e.g. sanitize a command).
    Replace(Value),
    /// Deny; `reason` is surfaced to the model or the user.
    Block { reason: String },
}

// ---------------------------------------------------------------------------
// Host capabilities — the plugin's whole reachable surface
// ---------------------------------------------------------------------------

/// Read-only session state.
///
/// `RuntimeActionId::{GetActiveTools, GetThinkingLevel, GetSystemPrompt,
/// GetCliFlag, GetSessionBranch}`.
pub trait SessionRead: Send + Sync {
    fn active_tools(&self) -> Vec<String>;
    fn system_prompt(&self) -> String;
    fn thinking_level(&self) -> String;
    fn cli_flag(&self, name: &str) -> Option<String>;
    fn session_branch(&self) -> Value;
}

/// Mutations of session state.
///
/// `RuntimeActionId::{SetSessionName, SetActiveTools, SetModel,
/// SetThinkingLevel, Compact, NewSession, Fork, NavigateTree, SwitchSession,
/// Reload}`.
pub trait SessionControl: Send + Sync {
    fn set_session_name(&self, name: &str) -> Result<(), PluginError>;
    fn set_active_tools(&self, tools: &[String]) -> Result<(), PluginError>;
    fn set_model(&self, model: &str) -> Result<(), PluginError>;
    fn set_thinking_level(&self, level: &str) -> Result<(), PluginError>;
    fn compact(&self) -> Result<(), PluginError>;
    fn new_session(&self) -> Result<(), PluginError>;
    fn fork(&self) -> Result<(), PluginError>;
    fn navigate_tree(&self, target: &str) -> Result<(), PluginError>;
    fn switch_session(&self, id: &str) -> Result<(), PluginError>;
    fn reload(&self) -> Result<(), PluginError>;
}

/// Injecting content into the run.
///
/// `RuntimeActionId::{SendMessage, SendUserMessage, AppendEntry}`. This is the
/// capability that changes what the model sees, so it is kept apart from the
/// two read/control surfaces above.
pub trait InjectApi: Send + Sync {
    fn send_message(&self, message: &Value) -> Result<(), PluginError>;
    fn send_user_message(&self, text: &str) -> Result<(), PluginError>;
    fn append_entry(&self, entry: &Value) -> Result<(), PluginError>;
}

/// The user-facing UI surface.
///
/// `RuntimeActionId::{UiDialog, SetStatus, SetEditorText, UiComponent}`.
pub trait UiApi: Send + Sync {
    /// Ask the user to choose; `options` are `{id, label, description}`
    /// objects, as the native `dialogs::permission_options` produced.
    fn dialog(&self, options: &Value) -> Result<Value, PluginError>;
    fn set_status(&self, status: Option<&str>);
    fn set_editor_text(&self, text: &str);
    fn component(&self, name: &str, payload: &Value) -> Result<Value, PluginError>;
}

/// Logging. Always available — this is the one capability no plugin is denied.
pub trait Log: Send + Sync {
    fn log(&self, level: LogLevel, message: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Everything a plugin may reach, handed in **per call**.
///
/// A capability left `None` is not merely empty — the plugin has no way to
/// express a call to it. The host builds one of these per plugin from its
/// deployment policy, which is this design's equivalent of adjudicating the
/// wasm import face before instantiation.
pub struct HostCaps<'a> {
    pub session: Option<&'a dyn SessionRead>,
    pub control: Option<&'a dyn SessionControl>,
    pub inject: Option<&'a dyn InjectApi>,
    pub ui: Option<&'a dyn UiApi>,
    pub log: &'a dyn Log,
}

impl<'a> HostCaps<'a> {
    /// Only logging: the minimum viable capability set (e.g. for a probe-only
    /// plugin that merely observes).
    pub fn observer(log: &'a dyn Log) -> Self {
        Self {
            session: None,
            control: None,
            inject: None,
            ui: None,
            log,
        }
    }
}

// ---------------------------------------------------------------------------
// The plugin
// ---------------------------------------------------------------------------

/// A plugin compiled into the binary.
///
/// Every method is optional: a plugin may contribute tools, gates, event
/// hooks, or any subset. The host calls only what the plugin declares (by
/// returning a non-empty list from [`Plugin::tools`] / [`Plugin::probe_points`]).
pub trait Plugin: Send + Sync + 'static {
    /// Stable identity: log prefix, config key, and the root of tool names.
    fn id(&self) -> &str;

    /// Tools this plugin contributes. Called once at registration.
    fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// Execute a tool. `name` is always one this plugin declared.
    ///
    /// `caps` is what this plugin may reach; `sink` is where partial output
    /// goes while the call is still running.
    fn call(
        &self,
        _name: &str,
        _arguments: &Value,
        _caps: &HostCaps<'_>,
        _sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        Err(PluginError::Failed("this plugin declares no tools".into()))
    }

    /// Slash commands this plugin contributes.
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }

    /// Run one of [`Plugin::commands`]. The returned text is shown to the user.
    fn run_command(
        &self,
        _name: &str,
        _arguments: &str,
        _caps: &HostCaps<'_>,
    ) -> Result<String, PluginError> {
        Err(PluginError::Failed(
            "this plugin declares no commands".into(),
        ))
    }

    /// Gate points this plugin wants to intervene at (e.g. `before-tool-call`).
    fn probe_points(&self) -> Vec<String> {
        Vec::new()
    }

    /// Synchronously decide in front of an action. Must be fast: the runtime
    /// waits for the verdict.
    fn probe(&self, _point: &str, _payload: &Value, _caps: &HostCaps<'_>) -> Verdict {
        Verdict::Continue
    }

    /// Observe (or claim) a runtime event.
    fn on_event(&self, _event: &Event, _caps: &HostCaps<'_>) -> EventAction {
        EventAction::Continue
    }

    /// Best-effort teardown.
    fn shutdown(&self) {}
}

/// Constructs a plugin. Each plugin crate exports exactly one of these.
pub type Factory = fn() -> Box<dyn Plugin>;
