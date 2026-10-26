//! ki's host-side extension crate: contract + wasm backend + dispatch spine.
//!
//! Formerly three crates (`kiqi-extension` / `kiqi-extension-host` / `kiqi-extensions`);
//! merged on 2026-10-26 so the host has one dependency instead of three
//! (`docs/design/kiqi-host-merge.md`). The layers inside:
//!
//! - **Contract (un-gated, no wasmtime)**: the [`Extension`] trait every extension
//!   backend implements, the event vocabulary ([`EventTag`], [`Event`],
//!   [`EventAction`], 37 tags), host identity and environment-variable
//!   constants, `grants` (capability/authorization model) and `dialogs`.
//! - **Dispatch spine (un-gated)**: `registry`, the `translate` funnels,
//!   the `compiled` sink that fans events into every wasm host, the action
//!   bridge + UI mailboxes, the stdio lifecycle, and the C-ABI bus types in
//!   [`abi`] (internal wire format; no `libloading` since the cdylib channel
//!   was retired on 2026-10-26).
//! - **Wasm backend (`wasm` feature)**: [`wasm`] binds `extensions/kiqi-extension.wit`
//!   with wasmtime and hosts a guest behind the same [`Extension`] seam. This
//!   is the only extension channel.
//!
//! # Red line
//!
//! Everything outside [`wasm`] must build **without** wasmtime: `kiqi-tools`,
//! `kiqi-mcp` and `kiqi-acp` depend on this crate unconditionally, and only
//! `kiqi-cli` (the composition root) turns the `wasm` feature on.
//!
//! # What an extension can reach
//!
//! Nothing it is not handed. The host passes [`HostCaps`] per call, built from
//! exactly the capabilities that extension's grants allow. Guests reach far less:
//! the wasm sandbox links an import only when the corresponding grant exists.
use serde_json::Value;

pub mod dialogs;

/// Largest accepted serialized provider patch (mirrors the wasm backend's
/// `wasm::MAX_ENVELOPE_BYTES`; lives at the crate root so the dispatch spine and
/// lean `ki` builds without the `wasm` feature can enforce it too).
pub const PROVIDER_PATCH_MAX_ENVELOPE_BYTES: usize = 32 * 1024;

/// Wasm (component model) extension backend — the only extension channel.
#[cfg(feature = "wasm")]
pub mod wasm;

// ---------------------------------------------------------------------------

/// Reserved key the host adds to a **extension** tool call's arguments.
///
/// The model never writes this field and never sees it: the host injects it
/// after the model's arguments are parsed and before the extension's
/// host's tool-execution hook runs, so an extension can tell *which* project and *which*
/// session it is serving without the model having to supply either. (Built-in
/// tools do not need it — they receive the host's `ExecutionToolContext`
/// directly. An extension tool has no such channel.)
///
/// The value is an object; today:
///
/// ```json
/// { "cwd": "D:\\Projects\\pi-rust",
///   "sessionId": "01adb026-7230-708b-855b-91eab6b056b8" }
/// ```
///
/// Both fields are optional and either may be absent: `cwd` when the host
/// cannot resolve one, `sessionId` for a session that has no id (ephemeral
/// sessions) and on hosts that predate this field. The key itself may be absent
/// entirely. Treat everything under it as best-effort, and prefer a
/// project-scoped fallback when `sessionId` is missing rather than refusing to
/// work.
///
/// **Consequence for extension parameter types:** an extension must tolerate unknown
/// top-level keys in its arguments. Reading them out of a `serde_json::Value`
/// does that naturally; a `#[serde(deny_unknown_fields)]` parameter struct
/// would reject this key, so do not use one for tool parameters.
///
/// The host's value wins if the model happens to send the same key: this is
/// host truth, not a hint to be merged.
pub const HOST_CONTEXT_KEY: &str = "__ki";

/// Number of event categories — `37` (33 Pi `on()` categories + 1 kiqi-specific
/// [`EventTag::BeforeTuiStart`] + 2 UI prompt events + 1 kiqi-specific
/// [`EventTag::EditorChange`]). A test asserts
/// `EVENT_TAG_COUNT == 37` so a future edit that adds/removes a tag is caught.
pub const EVENT_TAG_COUNT: usize = 37;

pub mod grants;

/// A tool the extension contributes to the model.
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
    /// This is not decoration: `kiqi-todo` keeps its whole task list here, so
    /// the state rides the session branch and survives fork/switch. Without a
    /// details channel an extension has to invent its own side file and loses the
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
/// pointer; here it is a sink the extension writes to, which is the same thing
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

/// A slash command the extension contributes (e.g. `/todos`).
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
}

/// A selector the host should show, mirroring the wasm `selector-spec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorSpec {
    pub title: String,
    pub items: Vec<String>,
}

/// An editor the host should open, mirroring the wasm `editor-spec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorSpec {
    pub initial_text: String,
}

/// A prompt the host should show, mirroring the wasm `prompt-spec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSpec {
    pub title: String,
    pub placeholder: String,
}

/// What a command wants the host to do after it returns.
///
/// Mirrors the wasm `command-result-body` variant. Only [`Message`] has host
/// handling today; the other variants are carried so a guest can return them
/// without the ABI lying about what it accepts.
///
/// [`Message`]: CommandResultBody::Message
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResultBody {
    /// Text to show the user.
    Message(String),
    /// Ask the user to pick one of `items`.
    Selector(SelectorSpec),
    /// Open an editor seeded with `initial_text`.
    Editor(EditorSpec),
    /// Ask the user for a line of text.
    Prompt(PromptSpec),
}

/// A command's outcome.
///
/// Mirrors the wasm `command-result`: `flow_id` tags selector/editor/prompt
/// continuations so a stale reply cannot be delivered to a later flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub flow_id: String,
    pub body: CommandResultBody,
}

impl CommandResult {
    /// The shape every command returns today: text for the user, no flow.
    pub fn message(text: impl Into<String>) -> Self {
        Self {
            flow_id: String::new(),
            body: CommandResultBody::Message(text.into()),
        }
    }
}

/// The user's answer to a selector/editor/prompt flow.
///
/// Mirrors the wasm `command-reply`: [`Selected`] answers a
/// [`CommandResultBody::Selector`], [`Submitted`] answers an editor or a
/// prompt, and [`Cancelled`] means the user dismissed the flow.
///
/// [`Selected`]: CommandReply::Selected
/// [`Submitted`]: CommandReply::Submitted
/// [`Cancelled`]: CommandReply::Cancelled
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandReply {
    /// A selector item.
    Selected(String),
    /// The editor contents or a prompt line.
    Submitted(String),
    /// The user dismissed the flow.
    Cancelled,
}

// ---------------------------------------------------------------------------
// OAuth and model-provider contributions (0.8.0)
// ---------------------------------------------------------------------------

/// Which OAuth flow a grant profile enables for a provider.
///
/// Mirrors the wasm `oauth-flow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthFlow {
    AuthorizationCode,
    DeviceCode,
}

/// A provider this guest declares it can drive.
///
/// Endpoints, client id and secrets stay host-side; the guest only sees this
/// metadata. [`Extension::oauth_action`] advances the flow and the resulting
/// credential never crosses the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProvider {
    pub id: String,
    pub display_name: String,
    pub scopes: Vec<String>,
    pub flows: Vec<OAuthFlow>,
    pub supports_pkce: bool,
}

/// What the user must do (or what the host already did) to continue a flow.
///
/// `url` is built by the host from the grant profile; the guest never supplies
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthInteraction {
    pub title: String,
    pub url: Option<String>,
    pub instructions: Vec<String>,
    pub user_code: Option<String>,
}

/// The credential the host stored for a granted provider.
///
/// No token material crosses this boundary. `expires_at` is a Unix timestamp in
/// seconds; `0` means no known expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OAuthCredentialStatus {
    pub expires_at: i64,
}

/// One step of an OAuth flow.
///
/// Mirrors the wasm `oauth-action`. [`Begin`] carries the PKCE
/// `code_challenge` (43 base64url characters) for authorization-code and an
/// empty string for device-code; the host constructs every URL.
///
/// [`Begin`]: OAuthAction::Begin
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthAction {
    Begin(String),
    Poll,
    /// Exchange the authorization code the user pasted back.
    Exchange(String),
    Cancel,
}

/// The guest-visible result of one flow step.
///
/// Mirrors the wasm `oauth-response`. Both [`Denied`] and [`Error`] are flow
/// outcomes the guest has to display or retry, not host traps.
///
/// [`Denied`]: OAuthResponse::Denied
/// [`Error`]: OAuthResponse::Error
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthResponse {
    /// Show this to the user and wait for the next step.
    Interaction(OAuthInteraction),
    /// The host stored a credential for this provider.
    Stored(OAuthCredentialStatus),
    AuthorizationPending,
    SlowDown,
    Denied(String),
    Error(String),
}

/// One model a provider exposes.
///
/// Mirrors the wasm `model-spec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub name: String,
    /// A typed `Api` variant name; `other` and `faux` are rejected by the host
    /// at grant validation.
    pub api: String,
    pub base_url: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    pub context_window: u64,
    pub max_tokens: u64,
}

/// A provider this guest offers.
///
/// The `provider` grant is the sole catalog authority; this list can only
/// narrow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    pub provider_id: String,
    pub display_name: String,
    pub models: Vec<ModelSpec>,
}

/// Which part of a model response a delta belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Thinking,
    ToolCall,
}

/// Where in a delta's lifecycle a chunk belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaPhase {
    Start,
    Delta,
    End,
}

/// One streamed delta of a provider response.
///
/// Mirrors the wasm `provider-delta`. `content_index` keeps two concurrent tool
/// calls and two thinking/text streams apart. On [`DeltaPhase::End`] of a
/// [`DeltaKind::ToolCall`], `text` is the complete `{id, name, arguments}`
/// JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDelta {
    pub kind: DeltaKind,
    pub phase: DeltaPhase,
    pub content_index: u32,
    pub text: String,
}

/// Where a running provider request writes its streamed deltas.
///
/// Mirrors the wasm `provider-stream` import, which is valid only while a
/// [`Extension::provider_request`] call is on the stack.
pub trait ProviderDeltaSink {
    fn delta(&self, delta: ProviderDelta);
}

/// A no-op delta sink, for callers that do not want the stream.
pub struct NoProviderDeltas;

impl ProviderDeltaSink for NoProviderDeltas {
    fn delta(&self, _delta: ProviderDelta) {}
}

/// A CLI flag the extension claims.
///
/// Registration is a **declaration**, not a callable: the host records that the
/// flag belongs to this extension, and the extension reads its value through
/// [`SessionRead::cli_flag`]. The same is true of [`ShortcutSpec`], whose key
/// arrives as an `Input` event.
#[derive(Debug, Clone)]
pub struct FlagSpec {
    pub name: String,
    pub description: String,
}

/// A CLI flag's value, as read through [`SessionRead::cli_flag`].
///
/// Mirrors the wasm `flag-value` variant: `Bare` is a boolean switch present
/// without a value, `Text` carries the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    /// Present without a value (e.g. `--verbose`).
    Bare,
    /// Present with a value (e.g. `--level 2`).
    Text(String),
}

/// A keyboard shortcut the extension claims (e.g. `f8`).
///
/// Declaring it routes the key to this extension as an [`EventTag::Input`] event;
/// there is no separate callback.
#[derive(Debug, Clone)]
pub struct ShortcutSpec {
    pub key: String,
    pub description: String,
}

/// An extension failure.
///
/// Distinct from [`ToolOutcome::error`]: this is "the extension could not do its
/// job", not "the tool ran and reported a failure the model should read".
#[derive(Debug)]
pub enum ExtensionError {
    /// The model invoked a tool this extension never declared.
    UnknownTool(String),
    /// The arguments did not match the tool's schema.
    InvalidArguments(String),
    /// The extension reached for a capability it was not granted.
    Denied(String),
    /// Anything else.
    Failed(String),
}

impl std::fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTool(name) => write!(f, "unknown tool `{name}`"),
            Self::InvalidArguments(detail) => write!(f, "invalid arguments: {detail}"),
            Self::Denied(detail) => write!(f, "denied: {detail}"),
            Self::Failed(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for ExtensionError {}

/// The event categories the host dispatches.
///
/// Mirrors the native ABI's `EventTag` **exactly**, discriminants included:
/// the extension and the host are one binary here, so there is no version skew to
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
    /// (kiqi-cli's `event_tags_match_the_native_abi` test), so no separate table
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

/// One runtime event delivered to extensions.
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
        // Both tags carry a tool result, exactly as the native
        // `kiqi_extension_sdk::EventView::tool_result` does: the host fans a
        // finished tool call out as `ToolExecutionEnd` (the compile-time
        // dispatcher routes that tag to the same handler), so accepting only
        // `ToolResult` silently turned those handlers into no-ops.
        if !matches!(self.tag, EventTag::ToolResult | EventTag::ToolExecutionEnd) {
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
    /// The `details` the tool returned - how an extension carries state that must
    /// survive fork/switch (kiqi-todo keeps its whole task list here).
    pub fn details(&self) -> &Value {
        self.result.get("details").unwrap_or(&Value::Null)
    }
}

/// What an extension decided to do about an event.
///
/// The native ABI returned an `i32` with `CONTINUE` / `CLAIMED`; this is the
/// same two outcomes with the magic numbers replaced by a type, plus the 0.8.0
/// [`Patch`] channel.
///
/// [`Patch`]: EventAction::Patch
#[derive(Debug, Clone, PartialEq)]
pub enum EventAction {
    /// Observed; pass the event on to the next extension and to normal handling.
    Continue,
    /// Consumed: no later extension sees it and normal handling is skipped.
    ///
    /// Mirrors the native ABI's `EVENT_HANDLER_CLAIMED` / `0`.
    Claimed,
    /// Rewrite the event payload (JSON) before handling continues.
    ///
    /// Honored ONLY for `before-provider-request` during a provider gate;
    /// every other tag ignores it. The host validates the patch against the
    /// `provider-request` grant and, when any limit is violated, rejects the
    /// WHOLE patch and proceeds unpatched with one diagnostic.
    Patch(Value),
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
// Host capabilities — the extension's whole reachable surface
// ---------------------------------------------------------------------------

/// Read-only session state.
///
/// `RuntimeActionId::{GetActiveTools, GetThinkingLevel, GetSystemPrompt,
/// GetCliFlag, GetSessionBranch}`.
pub trait SessionRead: Send + Sync {
    fn active_tools(&self) -> Vec<String>;
    fn system_prompt(&self) -> String;
    fn thinking_level(&self) -> String;
    fn cli_flag(&self, name: &str) -> Option<FlagValue>;
    fn session_branch(&self) -> Value;

    /// The session id, when the host knows it. Defaults to `None`, so an
    /// implementation that cannot answer keeps compiling and an extension reads an
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
    fn set_session_name(&self, name: &str) -> Result<(), ExtensionError>;
    fn set_active_tools(&self, tools: &[String]) -> Result<(), ExtensionError>;
    fn set_model(&self, model: &str) -> Result<(), ExtensionError>;
    fn set_thinking_level(&self, level: &str) -> Result<(), ExtensionError>;
    fn compact(&self) -> Result<(), ExtensionError>;
    fn new_session(&self) -> Result<(), ExtensionError>;
    fn fork(&self) -> Result<(), ExtensionError>;
    fn navigate_tree(&self, target: &str) -> Result<(), ExtensionError>;
    fn switch_session(&self, id: &str) -> Result<(), ExtensionError>;
    fn reload(&self) -> Result<(), ExtensionError>;
}

/// Injecting content into the run.
///
/// `RuntimeActionId::{SendMessage, SendUserMessage, AppendEntry}`. This is the
/// capability that changes what the model sees, so it is kept apart from the
/// two read/control surfaces above.
pub trait InjectApi: Send + Sync {
    fn send_message(&self, message: &Value) -> Result<(), ExtensionError>;
    fn send_user_message(&self, text: &str) -> Result<(), ExtensionError>;
    /// Append a message entry to the transcript (no run). The host takes
    /// `{"message": <AgentMessage>}`.
    fn append_message(&self, message: &Value) -> Result<(), ExtensionError>;

    /// Append a custom entry. The host takes
    /// `{"customType": "...", "data": <json>}`.
    fn append_custom_entry(&self, custom_type: &str, data: &Value) -> Result<(), ExtensionError>;
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
    fn dialog(&self, request: &Value) -> Result<Value, ExtensionError>;

    /// `{key, value?, panel?}` - the footer status the TUI renders.
    fn set_status(&self, update: &Value) -> Result<(), ExtensionError>;

    /// `{text, mode?, autoSendMs?}` - a draft dropped into the prompt editor.
    fn set_editor_text(&self, edit: &Value) -> Result<(), ExtensionError>;

    /// Custom UI components. The bridge does not serve this today, so the
    /// host refuses rather than pretending.
    fn component(&self, name: &str, payload: &Value) -> Result<Value, ExtensionError>;
}

/// Logging. Always available — this is the one capability no extension is denied.
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
/// on the floor). Long-running extensions poll it and wind down their children.
///
/// This is deliberately a probe, not a blocking wait: an extension decides where
/// its checkpoints are. It is also the one part of `HostCaps` that wasm guests
/// do not get — the ABI has no cancel channel; a guest's call is bounded by
/// the host's own limits instead.
pub trait CancelProbe: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

/// Everything an extension may reach, handed in **per call**.
///
/// A capability left `None` is not merely empty — the extension has no way to
/// express a call to it. The host builds one of these per extension from its
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
    /// extension that merely observes).
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
// The extension
// ---------------------------------------------------------------------------

/// An extension compiled into the binary.
///
/// Every method is optional: an extension may contribute tools, gates, event
/// hooks, or any subset. The host calls only what the extension declares (by
/// returning a non-empty list from [`Extension::tools`] / [`Extension::probe_points`]).
pub trait Extension: Send + Sync + 'static {
    /// Stable identity: log prefix, config key, and the root of tool names.
    fn id(&self) -> &str;

    /// Tools this extension contributes. Called once at registration.
    fn tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// Execute a tool. `name` is always one this extension declared.
    ///
    /// `caps` is what this extension may reach; `sink` is where partial output
    /// goes while the call is still running.
    fn call(
        &self,
        _name: &str,
        _arguments: &Value,
        _caps: &HostCaps<'_>,
        _sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, ExtensionError> {
        Err(ExtensionError::Failed(
            "this extension declares no tools".into(),
        ))
    }

    /// Slash commands this extension contributes.
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }

    /// Run one of [`Extension::commands`].
    ///
    /// The result says what the host should show or do next; today that is
    /// [`CommandResultBody::Message`].
    fn run_command(
        &self,
        _name: &str,
        _arguments: &str,
        _caps: &HostCaps<'_>,
    ) -> Result<CommandResult, ExtensionError> {
        Err(ExtensionError::Failed(
            "this extension declares no commands".into(),
        ))
    }

    /// Continue a selector/editor/prompt flow started by [`Extension::run_command`].
    ///
    /// `flow_id` is the id the earlier [`CommandResult`] carried; the host
    /// rejects a stale or unknown id before this is called. The default
    /// refuses, so an extension that only returns [`CommandResultBody::Message`]
    /// keeps compiling.
    fn continue_command(
        &self,
        _name: &str,
        _flow_id: &str,
        _reply: CommandReply,
        _caps: &HostCaps<'_>,
    ) -> Result<CommandResult, ExtensionError> {
        Err(ExtensionError::Failed(
            "this extension declares no continuable commands".into(),
        ))
    }

    /// CLI flags this extension claims.
    fn flags(&self) -> Vec<FlagSpec> {
        Vec::new()
    }

    /// Keyboard shortcuts this extension claims; the key then arrives as an
    /// [`EventTag::Input`] event.
    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        Vec::new()
    }

    /// OAuth providers this extension can drive (0.8.0).
    ///
    /// This is install-time discovery and a load-time cross-check only: the
    /// `oauth` grant is the sole catalog authority and may only narrow the
    /// list. Deny-by-default, like every 0.8.0 capability.
    fn oauth_providers(&self) -> Vec<OAuthProvider> {
        Vec::new()
    }

    /// Advance one OAuth flow step.
    ///
    /// The host owns endpoints, HTTP and credential storage; the guest only
    /// generates PKCE material and reports what the user must do, so no token
    /// crosses this boundary. The default refuses because an extension without the
    /// `oauth` grant is never called.
    fn oauth_action(
        &self,
        provider_id: &str,
        action: OAuthAction,
    ) -> Result<OAuthResponse, ExtensionError> {
        Err(ExtensionError::Denied(format!(
            "oauth is not granted for provider `{provider_id}`: {action:?}"
        )))
    }

    /// Model providers this extension offers (0.8.0).
    ///
    /// Metadata only; the `provider` grant is the sole catalog authority and
    /// may only narrow the list.
    fn provider_specs(&self) -> Vec<ProviderSpec> {
        Vec::new()
    }

    /// Serve one synchronous model request, streaming deltas to `sink`.
    ///
    /// Deny-by-default: without a `provider` grant the extension is never asked
    /// and the default below refuses.
    fn provider_request(
        &self,
        _request: &str,
        _sink: &dyn ProviderDeltaSink,
    ) -> Result<String, ExtensionError> {
        Err(ExtensionError::Denied(
            "provider requests are not granted".into(),
        ))
    }

    /// Rewrite model markdown output (0.8.0).
    ///
    /// Deny-by-default: the `render` grant decides, and the host owns theming,
    /// sanitization, scheduling and caching.
    fn transform_markdown(&self, _markdown: &str) -> Result<String, ExtensionError> {
        Err(ExtensionError::Denied("render is not granted".into()))
    }

    /// Rewrite the system prompt for the coming run, or `None` to leave it.
    ///
    /// The native slot is `register_before_agent_start`, which carries an `out`
    /// pointer for exactly this - unlike the fire-and-forget `BeforeAgentStart`
    /// event, which can only observe.
    fn before_agent_start(&self, _prompt: &Value, _caps: &HostCaps<'_>) -> Option<String> {
        None
    }

    /// Gate points this extension wants to intervene at (e.g. `before-tool-call`).
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

    /// Observe-only variant that must never wait for a busy extension.
    ///
    /// The observe fan-out runs on the UI's key loop, and an extension call can be
    /// waiting on the same user the UI is about to prompt (`ask_user` holds the
    /// wasm call lock for as long as the dialog is open). A blocking observe
    /// would therefore deadlock the dialog that would end the wait. Hosts that
    /// serialize calls (wasm) override this to skip instead of waiting;
    /// in-process extensions keep the blocking default.
    fn try_on_event(&self, event: &Event, caps: &HostCaps<'_>) -> EventAction {
        self.on_event(event, caps)
    }

    /// The `provider-request` policy this host enforces, if it is a sandboxed
    /// host with grants.
    ///
    /// `None` means the host has no grant model (an in-process compiled extension):
    /// it keeps the unconditional observe fan-out it has always had. A
    /// `Some` policy partitions the hosts: `patch: true` hosts are excluded
    /// from the observe fan-out and invoked only by the provider gate;
    /// `observe: true` hosts are invoked only by the fan-out; neither means the
    /// guest is never called.
    fn provider_request_policy(&self) -> Option<grants::ProviderRequestPolicy> {
        None
    }

    /// Decide in front of one provider request, returning a patch to apply.
    ///
    /// Called ONLY for `BeforeProviderRequest`, and only on hosts whose policy
    /// says `patch: true`. The implementation validates the returned patch
    /// against its grant; a violation is `Err` so the whole patch is dropped
    /// (the request then proceeds unpatched). `Ok(None)` means "no patch".
    fn provider_gate(&self, _event: &Event, _caps: &HostCaps<'_>) -> Result<Option<Value>, String> {
        Ok(None)
    }

    /// Best-effort teardown.
    fn shutdown(&self) {}
}

/// Constructs an extension. Each extension crate exports exactly one of these.
pub type Factory = fn() -> Box<dyn Extension>;

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

// ===========================================================================
// Dispatch spine (merged from `kiqi-extensions`, 2026-10-26)
// ===========================================================================

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use thiserror::Error;

pub use actions::{
    reload_callback_from_mailbox, trampoline_runtime_action, ActionBridge, ReloadMailbox,
    RuntimeActionHost, UiDialogMailbox, UiDialogRequest,
};
pub use compiled::{
    clear_compiled_event_sink, set_compiled_event_sink, CompiledEventSink, CompiledVerdict,
    DispatchMode,
};
pub use editor_text::{EditorTextEdit, EditorTextMailbox, EditorTextMode};
pub use event_log::{event_log_path, log_handler_invocation, EventLogger};
pub use loader::ExtensionSession;
pub use oauth::{
    list as list_oauth_providers, request as request_oauth, OAuthActionResponse,
    OAuthExtensionError, OAuthProviderInfo, OAuthResponseKind,
};
pub use prompt_transform::emit_before_agent_start;
pub use provider::PluggableProvider;
pub use provider_hooks::{before_agent_start_input, ExtensionProviderHooks};
pub use registry::{
    assert_active, current_platform, platform_allows, BeforeAgentStartHandler, ExtensionRegistry,
    ExtensionTool, RegisteredFlag, RegisteredHandler, RegisteredOAuthProvider, RegisteredProvider,
    RegisteredRenderer, RegisteredRendererKind, RegisteredShortcut, RegistryEntry,
    RegistrySnapshot, ResourcesDiscoverHandler, DEFAULT_PRIORITY,
};
pub use resources::{emit_resources_discover, DiscoveredResources};
pub use status::ExtensionStatusMailbox;
/// Serialises every test that installs the process-global compiled-event sink.
///
/// The sink is one static shared by the whole test binary; two modules testing
/// it in parallel would otherwise install/clear each other's sink mid-test.
#[cfg(test)]
pub(crate) static SINK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

mod panel;
pub use panel::{ExtensionPanel, PanelAnchor, PanelLayout};
pub use tool::{ExtensionToolAdapter, ExtensionToolHandle, ToolCallContext};
pub use translate::{
    dispatch_data_event, dispatch_data_event_claiming, dispatch_empty_event,
    dispatch_lifecycle_event, dispatch_tool_call_event, ExtensionEmitter, TeeEmitter,
};

pub use abi::*;
pub use stdio::StdioLifecycle;
pub mod abi;
mod actions;
mod compiled;
mod editor_text;
mod event_log;
mod loader;
mod oauth;
mod prompt_transform;
mod provider;
mod provider_hooks;
mod registry;
mod resources;
mod status;
mod stdio;
mod tool;
mod translate;

/// A diagnostics/event sink the host wires so the loader + adapter can report
/// extension-load skips, ABI mismatches, panics caught at the FFI boundary, etc.
/// Mirrors the `diagnostic` channel pi surfaces for extensions.
pub trait ExtensionDiagnostics: Send + Sync {
    /// A non-fatal warning (e.g. "skipped extension foo: ABI version mismatch").
    fn warn(&self, message: &str);
    /// An extension that registered something but a registration slot is not yet
    /// wired on the host (e.g. an event tag the host does not emit yet).
    fn unsupported(&self, message: &str);
}

/// A no-op diagnostics sink (the default when the host does not supply one).
#[derive(Default)]
pub struct NullDiagnostics;

impl ExtensionDiagnostics for NullDiagnostics {
    fn warn(&self, _message: &str) {}
    fn unsupported(&self, _message: &str) {}
}

/// Error returned by [`ExtensionToolAdapter::execute`] when the extension side failed
/// (terminal `Err` from `poll`, or the drive handle was never produced).
#[derive(Debug, Error)]
pub enum ExtensionToolError {
    #[error("extension execute returned a null handle (allocation failure)")]
    NullHandle,
    #[error("extension error: {0}")]
    Extension(String),
    #[error("runtime unavailable: {0}")]
    NoRuntime(String),
}

/// The host's `free_string` for [`StbString`]s the host *produces* and hands to
/// the extension (event payloads, action outputs, execute params when the host
/// owns them). The extension frees what it *receives* via this; the host frees
/// what it *receives* via the extension's `free_string` (stored per-tool).
///
/// Reconstructs the `Box<[u8]>` from ptr+len and drops it. Idempotent on
/// empty/null.
pub extern "C" fn host_free_string(s: StbString) {
    if s.is_empty() || s.ptr.is_null() {
        return;
    }
    // SAFETY: the host produced this StbString via `StbString::from_owned`/
    // `from_string` (a `Box<[u8]>`), so ptr+len reconstruct the same allocation.
    unsafe {
        let slice = std::slice::from_raw_parts(s.ptr as *const u8, s.len);
        let _ = Box::from_raw(slice as *const [u8] as *mut [u8]);
    }
}

// ===========================================================================
// HostApi — the ExtensionApi the host builds and hands to each extension's register
// ===========================================================================

/// The host state a [`ExtensionApi`] closes over. Held behind `Arc` so the fn
/// pointers (which are `extern "C"`,不好做闭包) can recover the host state via
/// the `user_data` slot — but since `extern "C" fn` cannot capture, the host
/// stores per-registration receiver state in the [`HostApi`] itself keyed by
/// nothing (single registry per host), and the fns are thin trampolines that
/// read a process-global-attached registry. The host builds one `HostApi` per
/// load session; the `register_*` trampolines forward into it.
///
/// This struct is `Send + Sync` (registry is `Mutex`-guarded).
pub struct HostApi {
    registry: Mutex<Option<ExtensionRegistry>>,
    /// Display name of the extension currently registering (from the cdylib file
    /// stem). Stamped onto every registration the extension makes (e.g. event
    /// handlers) so host-side diagnostics can name the owning extension. Not
    /// part of the ABI — the extension never sees it.
    name: String,
    /// B5a: the extension→host action bridge, carried in
    /// [`ExtensionApi::user_data`] so [`trampoline_runtime_action`] can recover
    /// the harness state from ANY thread an extension calls from (post-register, no
    /// thread-local). `None` keeps the no-bridge stub `runtime_action` and the
    /// register `user_data` (the `HostApi` pointer) — so call sites that
    /// don't pass a bridge behave exactly as before.
    action_bridge: Option<Arc<ActionBridge>>,
}

impl HostApi {
    /// Build a fresh host API bound to the given registry + diagnostics. With
    /// no action bridge, `runtime_action` stays the stub returning `-1`. `name`
    /// is the registering extension's display name (from the cdylib file stem).
    pub fn new(
        name: impl Into<String>,
        registry: ExtensionRegistry,
        _diagnostics: Arc<dyn ExtensionDiagnostics>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(Some(registry)),
            name: name.into(),
            action_bridge: None,
        })
    }

    /// The registering extension's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// B5a: build a host API that wires the real [`trampoline_runtime_action`]
    /// via the bridge. The bridge is cloned into every extension's vtable
    /// `user_data` so post-register `runtime_action` calls recover it on any
    /// thread. `kiqi-cli` keeps one master `Arc<ActionBridge>` per session; the
    /// extension gets a scoped context sharing the session's facilities. The
    /// loader retains that context with the library, so its pointer stays valid
    /// after registration. `name` is the registering extension's display name.
    pub fn with_action_bridge(
        name: impl Into<String>,
        registry: ExtensionRegistry,
        _diagnostics: Arc<dyn ExtensionDiagnostics>,
        action_bridge: Arc<ActionBridge>,
    ) -> Arc<Self> {
        let name = name.into();
        let action_bridge = action_bridge.for_extension(&name);
        Arc::new(Self {
            registry: Mutex::new(Some(registry)),
            name,
            action_bridge: Some(action_bridge),
        })
    }

    fn with_registry<R>(&self, f: impl FnOnce(&mut ExtensionRegistry) -> R) -> Option<R> {
        let mut guard = self.registry.lock().expect("host api registry lock");
        guard.as_mut().map(f)
    }

    /// Detach the registry (call after registration completes so the host can
    /// move it out for use). The `Mutex<Option<...>>` is left `None`.
    pub fn take_registry(&self) -> Option<ExtensionRegistry> {
        self.registry.lock().expect("host api registry lock").take()
    }

    /// Build the [`ExtensionApi`] struct passed to `kiqi_extension_register`.
    ///
    /// Every optional registrar slot resolves to a real `extern "C"` trampoline
    /// that forwards into `self`'s registry (so an extension that calls
    /// `register_tool` / `register_command` / renderer registration sees its
    /// registration land). `runtime_action` resolves to the real
    /// [`trampoline_runtime_action`] when a bridge is present (B5a), else the
    /// stub returning `-1`.
    ///
    /// **`user_data`**: the register trampolines recover host state via the
    /// thread-local `CURRENT_HOST_API` (set for the duration of register in
    /// `load_one`) — they ignore `user_data`. Post-register `runtime_action`
    /// cannot use the thread-local (extension calls from foreign threads), so when
    /// a bridge is present `user_data` is repurposed to point at the
    /// `ActionBridge` (the SDK-designated "host's opaque context"). The bridge's
    /// scoped `Arc` is retained by the loaded extension and session keepalive, so
    /// the pointer an extension stores during register stays valid.
    pub fn build_vtable(self: &Arc<Self>) -> crate::ExtensionApi {
        // When a bridge is present, user_data carries it (post-register action
        // recovery). Otherwise keep the register-path HostApi pointer (harmless
        // — register trampolines use the thread-local and ignore user_data).
        // Cast both arms to the `RuntimeActionFn` pointer type — distinct fn
        // items have unique types even with identical signatures, so the match
        // needs a common fn-pointer type to unify on (the struct field is
        // `RuntimeActionFn`, a bare `extern "C" fn` alias, not an `Option`).
        let (runtime_action_fn, ud) = match &self.action_bridge {
            Some(bridge) => (
                trampoline_runtime_action as RuntimeActionFn,
                Arc::as_ptr(bridge) as *mut c_void,
            ),
            None => (
                stub_runtime_action as RuntimeActionFn,
                Arc::as_ptr(self) as *mut c_void,
            ),
        };
        crate::ExtensionApi {
            abi_version: crate::KI_EXTENSION_ABI_VERSION_UNIFIED,
            struct_size: std::mem::size_of::<crate::ExtensionApi>() as u32,
            free_string: host_free_string,
            register_tool: Some(trampoline_register_tool),
            register_command: Some(trampoline_register_command),
            register_shortcut: Some(trampoline_register_shortcut),
            register_flag: Some(trampoline_register_flag),
            register_provider: Some(trampoline_register_provider), // B5c
            register_message_renderer: Some(trampoline_register_message_renderer), // B5c
            register_markdown_transformer: Some(trampoline_register_markdown_transformer), // B5c
            register_entry_renderer: Some(trampoline_register_entry_renderer), // B5c
            register_event_handler: Some(trampoline_register_event_handler),
            register_resources_discover: Some(trampoline_register_resources_discover),
            // Appended last so `struct_size` grows without shifting any earlier
            // offset (an extension built against the previous layout keeps working).
            register_before_agent_start: Some(trampoline_register_before_agent_start),
            register_oauth_provider: Some(trampoline_register_oauth_provider),
            runtime_action: runtime_action_fn,
            dispatch_event: Some(trampoline_dispatch_event),
            user_data: ud,
            declare: Some(trampoline_declare),
        }
    }
}

// Thread-local "current host api" for the duration of an extension register call.
// Set by the retired `load_one` loader before calling register, cleared after.
// The loader is gone, so nothing sets it today; kept with the rest of the
// unreachable C-ABI spine. This is the
// sound way to let `extern "C" fn` trampolines (which cannot capture) reach the
// host's registry: the register call is synchronous and single-threaded per
// extension, so a thread-local is unambiguous.
thread_local! {
    static CURRENT_HOST_API: std::cell::Cell<*const HostApi> = std::cell::Cell::new(std::ptr::null());
}

/// SAFETY: must be called only while the `Arc<HostApi>` pointed to by `api` is
/// kept alive (i.e. inside `with_current_api`). Sets the thread-local current
/// api pointer.
unsafe fn set_current_api(api: &Arc<HostApi>) {
    CURRENT_HOST_API.with(|c| c.set(Arc::as_ptr(api) as *const HostApi));
}

fn clear_current_api() {
    CURRENT_HOST_API.with(|c| c.set(std::ptr::null()));
}

/// Run `f` with the current thread-local host api borrowed. No-op (returns
/// `false`) if no api is current (e.g. a trampoline called outside register).
fn with_current_api<R>(f: impl FnOnce(&HostApi) -> R) -> Option<R> {
    let ptr = CURRENT_HOST_API.with(|c| c.get());
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the `Arc<HostApi>` is alive for the duration of register (kept by
    // `load_one`'s stack), and this thread set it; borrowing here is sound.
    let api = unsafe { &*ptr };
    Some(f(api))
}

/// True if a current host api is set (cheap check used by trampolines that only
/// need presence, not the api itself).
fn current_api_present() -> bool {
    let ptr = CURRENT_HOST_API.with(|c| c.get());
    !ptr.is_null()
}

// --- the registrar trampolines (extern "C", forward into the current api) ---

extern "C" fn trampoline_register_tool(
    schema: *const StableToolSchema,
    execute_fn: ToolExecuteFn,
    poll_fn: ToolPollFn,
    cancel_fn: ToolCancelFn,
    destroy_fn: ToolDestroyFn,
    extension_free_string: FreeStringFn,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    if schema.is_null() {
        return 1;
    }
    // SAFETY: the extension guarantees `schema` is valid for the call; we copy the
    // strings out immediately (under the borrow) and free them via the extension's
    // `extension_free_string` right after, so no retention past the call.
    let (name, description, parameters_value, schema_owned) = unsafe {
        let s = &*schema;
        (
            s.name.to_string_lossy(),
            s.description.to_string_lossy(),
            serde_json::from_str::<serde_json::Value>(&s.parameters.to_string_lossy()).ok(),
            *s,
        )
    };
    // Free the schema strings via the extension's free fn (extension produced them).
    // `FreeStringFn` is `extern "C" fn(StbString)` — calling a bare fn pointer
    // is safe (no `unsafe` needed); the idempotent free contract is on the extension.
    extension_free_string(schema_owned.name);
    extension_free_string(schema_owned.description);
    extension_free_string(schema_owned.parameters);
    let Some(parameters_value) = parameters_value else {
        return 2;
    };
    if name.trim().is_empty() || !parameters_value.is_object() {
        return 2;
    }
    let tool = kiqi_ai::types::Tool {
        name,
        description,
        parameters: kiqi_ai::types::Schema::new(parameters_value),
        constrained_sampling: None,
    };
    let handle = ExtensionToolHandle {
        execute_fn,
        poll_fn,
        cancel_fn,
        destroy_fn,
        extension_free_string,
    };
    // `register_tool` returns `bool` (true on overwrite of a prior same-named
    // tool). For the extension a name collision is not a hard failure — always
    // return 0 (success) when the registry accepted it; -1 only if the registry
    // was already detached.
    let ok =
        with_current_api(
            |api| match api.with_registry(|reg| reg.register_tool(tool, handle)) {
                Some(_) => true,
                None => false,
            },
        );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_command(
    name: StbStringRef,
    description: StbStringRef,
    handler: crate::CommandHandlerFn,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the extension guarantees the refs are valid for this call.
    let (name, description) =
        unsafe { (name.as_str().to_string(), description.as_str().to_string()) };
    let ok = with_current_api(|api| {
        // The v1 command ABI predates an explicit context parameter. Keep a
        // null context for compatibility; extensions that need host state can use
        // `runtime_action` from their command callback.
        match api.with_registry(|reg| {
            reg.register_command(name, description, handler, std::ptr::null_mut())
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_shortcut(key: StbStringRef, description: StbStringRef) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the extension guarantees these borrowed refs are valid for the
    // duration of the registration call; copy before returning.
    let (key, description) =
        unsafe { (key.as_str().to_string(), description.as_str().to_string()) };
    // Record the claim. The key is NOT bound to a callback here: the owning
    // extension receives presses/releases through its `Input` event handler
    // (dispatched by the TUI) and decides what to do. The host only needs to
    // know the key is claimed so it can route it away from the editor.
    let ok = with_current_api(|api| {
        match api
            .with_registry(|reg| reg.register_shortcut(api.name().to_string(), key, description))
        {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_flag(name: StbStringRef, description: StbStringRef) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the extension guarantees these borrowed refs are valid for the
    // duration of the registration call; copy them before returning.
    let (name, description) =
        unsafe { (name.as_str().to_string(), description.as_str().to_string()) };
    if name.trim().is_empty() || name.starts_with('-') || name.contains('=') || name.contains(' ') {
        return 2;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| reg.register_flag(name, description)) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_event_handler(
    tag: EventTag,
    handler: EventHandlerFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| {
            reg.register_event_handler(api.name().to_string(), tag, handler, user_data)
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// Extension `declare` trampoline. Runs synchronously during an extension's
/// `register` (thread-local `CURRENT_HOST_API` is set). The extension hands a
/// borrowed JSON payload (`{"priority":60,"platforms":["linux"]}`); we parse
/// and apply it to this extension's registry (retro-updating any handlers already
/// registered, so declare order doesn't matter). Returns `0` on a parsed
/// payload, `1` on malformed JSON, `-1` outside register.
extern "C" fn trampoline_declare(json: StbStringRef) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the extension guarantees `json` is valid UTF-8 for the call; we read
    // it immediately (copy into the registry) and never retain the borrow.
    let text = unsafe { json.as_str() };
    match with_current_api(|api| {
        api.with_registry(|reg| {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                return false;
            };
            if value.get("ownsStdio").and_then(serde_json::Value::as_bool) == Some(true) {
                if let Some(bridge) = &api.action_bridge {
                    if let Err(error) = bridge.claim_stdio() {
                        tracing::error!("{error}");
                        return false;
                    }
                } else {
                    return false; // A stdio owner requires a live lifecycle bridge.
                }
            }
            reg.apply_declaration_json(text)
        })
    }) {
        Some(Some(true)) => 0,
        Some(Some(false)) => 1,
        _ => -1,
    }
}

/// B5c: `register_provider` trampoline. Runs synchronously inside an extension's
/// `register` call (thread-local `CURRENT_HOST_API` is set). The extension hands its
/// `provider_id`/`base_url`/`api_style` (borrowed `StbStringRef`s), its
/// `request_fn`, its own `extension_free_string` (the `out` StbString `request_fn`
/// later produces is extension-owned — the host reclaims it via this fn), and its
/// opaque `user_data`. We copy the id/base_url/api_style to owned `String`s
/// (they're the provider's identity, read when building the
/// [`PluggableProvider`](crate::PluggableProvider) — we can't keep the borrowed
/// refs past register), then store the full record in the registry.
extern "C" fn trampoline_register_provider(
    provider_id: StbStringRef,
    base_url: StbStringRef,
    api_style: StbStringRef,
    request_fn: ProviderRequestFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the extension guarantees the refs are valid for this call.
    let record = crate::registry::RegisteredProvider {
        provider_id: unsafe { provider_id.as_str().to_string() },
        base_url: unsafe { base_url.as_str().to_string() },
        api_style: unsafe { api_style.as_str().to_string() },
        request_fn,
        extension_free_string,
        user_data,
    };
    let ok = with_current_api(
        |api| match api.with_registry(|reg| reg.register_provider(record)) {
            Some(_) => true,
            None => false,
        },
    );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// Register an OAuth provider manifest and action callback. The manifest is
/// copied immediately; token-bearing responses remain extension-owned and are
/// reclaimed through the extension free function when invoked by a future host
/// OAuth bridge.
extern "C" fn trampoline_register_oauth_provider(
    manifest_json: StbStringRef,
    request_fn: OAuthRequestFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let manifest = unsafe { manifest_json.as_str().to_string() };
    let record = crate::registry::RegisteredOAuthProvider {
        extension: with_current_api(|api| api.name.clone()).unwrap_or_default(),
        manifest_json: manifest,
        request_fn,
        extension_free_string,
        user_data,
    };
    match with_current_api(|api| api.with_registry(|reg| reg.register_oauth_provider(record))) {
        Some(Some(true)) => 1,
        Some(Some(false)) => 0,
        _ => -1,
    }
}

/// B5c: the three renderer registrars share a single recording helper; each
/// trampoline below fixes its [`RegisteredRendererKind`] and forwards here.
fn register_renderer_common(
    name: StbStringRef,
    kind: crate::registry::RegisteredRendererKind,
    render_fn: RenderFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the extension guarantees the ref is valid for this call.
    let record = crate::registry::RegisteredRenderer {
        name: unsafe { name.as_str().to_string() },
        kind,
        render_fn,
        extension_free_string,
        user_data,
    };
    let ok = with_current_api(
        |api| match api.with_registry(|reg| reg.register_renderer(record)) {
            Some(_) => true,
            None => false,
        },
    );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_message_renderer(
    name: StbStringRef,
    render_fn: RenderFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Message,
        render_fn,
        extension_free_string,
        user_data,
    )
}

extern "C" fn trampoline_register_markdown_transformer(
    name: StbStringRef,
    render_fn: RenderFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Markdown,
        render_fn,
        extension_free_string,
        user_data,
    )
}

extern "C" fn trampoline_register_entry_renderer(
    name: StbStringRef,
    render_fn: RenderFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Entry,
        render_fn,
        extension_free_string,
        user_data,
    )
}

extern "C" fn trampoline_dispatch_event(
    _event: StableExtensionEvent,
    _user_data: *mut c_void,
) -> i32 {
    // v1: an extension emitting an event upstream — we accept it (return 0) but do
    // not yet forward to host subscribers (no upstream channel wired). B5 wires
    // the reverse-direction event bus.
    0
}

/// B5b: `register_resources_discover` trampoline. Runs synchronously inside a
/// extension's `register` call (so the thread-local `CURRENT_HOST_API` is set → the
/// register-path recovery works, same as the other `trampoline_register_*` fns).
/// The extension hands its `handler`, its own `extension_free_string` (the `out`
/// StbString the handler later produces is extension-owned — the host must reclaim
/// it via this fn), and its opaque `user_data`. The host stores all three in the
/// registry and later fans the discovery event out via `emit_resources_discover`.
extern "C" fn trampoline_register_resources_discover(
    handler: ResourcesDiscoverFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| {
            reg.register_resources_discover(handler, extension_free_string, user_data)
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// `register_before_agent_start` trampoline. Same shape as
/// [`trampoline_register_resources_discover`]: runs synchronously inside a
/// extension's `register` call (thread-local `CURRENT_HOST_API` is set), stores the
/// handler plus the extension's own `extension_free_string` (the `out` StbString the
/// handler later produces is extension-owned) and its opaque `user_data`.
extern "C" fn trampoline_register_before_agent_start(
    handler: BeforeAgentStartFn,
    extension_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| {
            reg.register_before_agent_start(handler, extension_free_string, user_data)
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// Fallback for [`HostApi`]s built without an [`ActionBridge`] (the
/// `HostApi::new` path). Return -1 (`EPERM`-ish) so an extension can detect
/// "unsupported" without crashing. When a bridge is present, `build_vtable`
/// installs [`trampoline_runtime_action`] instead.
extern "C" fn stub_runtime_action(
    _action_id: u32,
    _args: StbStringRef,
    _out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    -1
}

// Keep the `RuntimeActionFn` type name referenced for doc clarity / future wiring.
#[allow(dead_code)]
type _RuntimeActionFnDoc = RuntimeActionFn;
