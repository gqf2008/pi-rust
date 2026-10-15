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

pub mod grants;

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

/// A CLI flag the plugin claims.
///
/// Registration is a **declaration**, not a callable: the host records that the
/// flag belongs to this plugin, and the plugin reads its value through
/// [`SessionRead::cli_flag`]. The same is true of [`ShortcutSpec`], whose key
/// arrives as an `Input` event.
#[derive(Debug, Clone)]
pub struct FlagSpec {
    pub name: String,
    pub description: String,
}

/// A keyboard shortcut the plugin claims (e.g. `f8`).
///
/// Declaring it routes the key to this plugin as an [`EventTag::Input`] event;
/// there is no separate callback.
#[derive(Debug, Clone)]
pub struct ShortcutSpec {
    pub key: String,
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

/// The event categories the host dispatches.
///
/// Mirrors the native ABI's `EventTag` **exactly**, discriminants included:
/// the plugin and the host are one binary here, so there is no version skew to
/// absorb - which is also why there is no `Other` escape hatch. An unknown tag
/// cannot exist, and pretending otherwise would just mean matching on strings
/// again.
///
/// Payloads travel as JSON (see [`Event`]): the native ABI already ships them
/// that way, so this is parity, not a regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum EventTag {
    ProjectTrust = 0,
    ResourcesDiscover = 1,
    SessionStart = 2,
    SessionInfoChanged = 3,
    SessionBeforeSwitch = 4,
    SessionBeforeFork = 5,
    SessionBeforeCompact = 6,
    SessionCompact = 7,
    SessionShutdown = 8,
    SessionBeforeTree = 9,
    SessionTree = 10,
    Context = 11,
    BeforeProviderRequest = 12,
    BeforeProviderHeaders = 13,
    AfterProviderResponse = 14,
    BeforeAgentStart = 15,
    AgentStart = 16,
    AgentEnd = 17,
    AgentSettled = 18,
    TurnStart = 19,
    TurnEnd = 20,
    MessageStart = 21,
    MessageUpdate = 22,
    MessageEnd = 23,
    ToolExecutionStart = 24,
    ToolExecutionUpdate = 25,
    ToolExecutionEnd = 26,
    ModelSelect = 27,
    ThinkingLevelSelect = 28,
    ToolCall = 29,
    ToolResult = 30,
    UserBash = 31,
    Input = 32,
    BeforeTuiStart = 33,
    UiPromptStart = 34,
    UiPromptEnd = 35,
    EditorChange = 36,
}

impl EventTag {
    /// The wire value, identical to the native ABI's `EventTag` discriminant.
    pub fn as_u32(self) -> u32 {
        self as u32
    }

    /// The tag for a wire discriminant, or `None` when the value is not one of
    /// the 37 tags.
    ///
    /// The host translates its native `EventTag` to this contract through this
    /// function: the two enumerations are pinned to the same discriminants
    /// (rpi-cli's `event_tags_match_the_native_abi` test), so no separate table
    /// has to be kept in sync.
    pub fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0 => Self::ProjectTrust,
            1 => Self::ResourcesDiscover,
            2 => Self::SessionStart,
            3 => Self::SessionInfoChanged,
            4 => Self::SessionBeforeSwitch,
            5 => Self::SessionBeforeFork,
            6 => Self::SessionBeforeCompact,
            7 => Self::SessionCompact,
            8 => Self::SessionShutdown,
            9 => Self::SessionBeforeTree,
            10 => Self::SessionTree,
            11 => Self::Context,
            12 => Self::BeforeProviderRequest,
            13 => Self::BeforeProviderHeaders,
            14 => Self::AfterProviderResponse,
            15 => Self::BeforeAgentStart,
            16 => Self::AgentStart,
            17 => Self::AgentEnd,
            18 => Self::AgentSettled,
            19 => Self::TurnStart,
            20 => Self::TurnEnd,
            21 => Self::MessageStart,
            22 => Self::MessageUpdate,
            23 => Self::MessageEnd,
            24 => Self::ToolExecutionStart,
            25 => Self::ToolExecutionUpdate,
            26 => Self::ToolExecutionEnd,
            27 => Self::ModelSelect,
            28 => Self::ThinkingLevelSelect,
            29 => Self::ToolCall,
            30 => Self::ToolResult,
            31 => Self::UserBash,
            32 => Self::Input,
            33 => Self::BeforeTuiStart,
            34 => Self::UiPromptStart,
            35 => Self::UiPromptEnd,
            36 => Self::EditorChange,
            _ => return None,
        })
    }
}

/// One runtime event delivered to plugins.
///
/// A tag plus a JSON payload. The payload travels as JSON because the native
/// ABI already ships it that way - this is parity, not a shortcut - and typed
/// accessors exist for the tags whose shape matters.
#[derive(Debug, Clone)]
pub struct Event {
    pub tag: EventTag,
    pub payload: Value,
}

impl Event {
    pub fn new(tag: EventTag, payload: Value) -> Self {
        Self { tag, payload }
    }

    /// The payload of a `ToolResult` event, matching the native
    /// `EventToolResult` fields (`tool_call_id` / `tool_name` / `result` /
    /// `is_error`), where `result` is a JSON *string* that is parsed here.
    pub fn tool_result(&self) -> Option<ToolResult> {
        if self.tag != EventTag::ToolResult {
            return None;
        }
        let get = |key: &str| self.payload.get(key);
        let text = |key: &str| get(key).and_then(Value::as_str).map(str::to_string);
        let raw = get("result").or_else(|| get("resultJson"))?;
        let result = match raw {
            Value::String(encoded) => serde_json::from_str(encoded).unwrap_or(Value::Null),
            other => other.clone(),
        };
        Some(ToolResult {
            tool_call_id: text("toolCallId")
                .or_else(|| text("tool_call_id"))
                .unwrap_or_default(),
            tool: text("toolName")
                .or_else(|| text("tool_name"))
                .unwrap_or_default(),
            is_error: get("isError")
                .or_else(|| get("is_error"))
                .map(|v| v == &Value::Bool(true) || v.as_u64() == Some(1))
                .unwrap_or(false),
            result,
        })
    }
}

/// A parsed `ToolResult` event payload.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub tool: String,
    /// The tool result object (`{"content": [...], "details": {...}}`).
    pub result: Value,
    pub is_error: bool,
}

impl ToolResult {
    /// The `details` the tool returned - how a plugin carries state that must
    /// survive fork/switch (rpi-todo keeps its whole task list here).
    pub fn details(&self) -> &Value {
        self.result.get("details").unwrap_or(&Value::Null)
    }
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
    ///
    /// The enum stops here on purpose: it mirrors the native ABI's
    /// `EVENT_HANDLER_CLAIMED` / `0`, and the host has no channel for a rewritten
    /// event payload, so a `Replace` variant would be a promise nothing keeps.
    Claimed,
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

    /// The session id, when the host knows it. Defaults to `None`, so an
    /// implementation that cannot answer keeps compiling and a plugin reads an
    /// empty id rather than a fabricated one.
    fn session_id(&self) -> Option<String> {
        None
    }
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
    /// Append a message entry to the transcript (no run). The host takes
    /// `{"message": <AgentMessage>}`.
    fn append_message(&self, message: &Value) -> Result<(), PluginError>;

    /// Append a custom entry. The host takes
    /// `{"customType": "...", "data": <json>}`.
    fn append_custom_entry(&self, custom_type: &str, data: &Value) -> Result<(), PluginError>;
}

/// The user-facing UI surface.
///
/// `RuntimeActionId::{UiDialog, SetStatus, SetEditorText, UiComponent}`.
/// The user-facing UI surface.
///
/// Payloads are the host's own shapes, named field for field after what the
/// mailbox handlers actually read - the native packages build these by hand
/// too, so this is parity rather than a shortcut. Guessing simplified
/// signatures here would silently drop fields the host needs (a status
/// without its `key`, a draft without `mode`/`autoSendMs`).
pub trait UiApi: Send + Sync {
    /// `{op, requestId, ui?, toolCallId?, answer?}`.
    ///
    /// `op` drives the dialog lifecycle, `ui` carries the descriptor (the
    /// option list a permission prompt renders).
    fn dialog(&self, request: &Value) -> Result<Value, PluginError>;

    /// `{key, value?, panel?}` - the footer status the TUI renders.
    fn set_status(&self, update: &Value) -> Result<(), PluginError>;

    /// `{text, mode?, autoSendMs?}` - a draft dropped into the prompt editor.
    fn set_editor_text(&self, edit: &Value) -> Result<(), PluginError>;

    /// Custom UI components. The bridge does not serve this today, so the
    /// host refuses rather than pretending.
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

/// A cancellation probe, wired per call when the host can abort a running
/// tool (the harness's `CancellationToken`, which the adapter otherwise dropped
/// on the floor). Long-running plugins poll it and wind down their children.
///
/// This is deliberately a probe, not a blocking wait: a plugin decides where
/// its checkpoints are. It is also the one part of `HostCaps` that wasm guests
/// do not get — the ABI has no cancel channel; a guest's call is bounded by
/// the host's own limits instead.
pub trait CancelProbe: Send + Sync {
    fn is_cancelled(&self) -> bool;
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
    /// Set only for the duration of a tool call the host can abort.
    pub cancel: Option<&'a dyn CancelProbe>,
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
            cancel: None,
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

    /// CLI flags this plugin claims.
    fn flags(&self) -> Vec<FlagSpec> {
        Vec::new()
    }

    /// Keyboard shortcuts this plugin claims; the key then arrives as an
    /// [`EventTag::Input`] event.
    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        Vec::new()
    }

    /// Rewrite the system prompt for the coming run, or `None` to leave it.
    ///
    /// The native slot is `register_before_agent_start`, which carries an `out`
    /// pointer for exactly this - unlike the fire-and-forget `BeforeAgentStart`
    /// event, which can only observe.
    fn before_agent_start(&self, _prompt: &Value, _caps: &HostCaps<'_>) -> Option<String> {
        None
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

#[cfg(test)]
mod tests {
    use super::EventTag;

    #[test]
    fn tag_discriminants_round_trip_and_stop_at_the_surface() {
        for value in 0..=36 {
            let tag = EventTag::from_u32(value).expect("a tag for every discriminant");
            assert_eq!(tag.as_u32(), value);
        }
        assert_eq!(EventTag::from_u32(37), None);
        assert_eq!(EventTag::from_u32(u32::MAX), None);
    }
}
