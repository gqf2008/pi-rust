//! The compile-time (`kiqi-plugin`) face of kiqi-langfuse.
//!
//! The native ABI registered 14 event handlers and three tools through
//! `export_plugin!`; here the same surface is a [`Plugin`] impl. The handlers
//! are plain functions that publish status through [`StatusHost`], so the two
//! channels share one body each: [`CapsHost`] hands them `HostCaps::ui`, the
//! cdylib face would hand them the raw action trampoline.
//!
//! Dispatch is a table, not a catch-all: [`handler_for`] is the single source
//! of truth for the 14 subscribed tags, and the tests assert every one of them
//! reaches its handler.

use crate::{
    on_agent_settled, on_agent_start, on_before_agent_start, on_before_provider_request,
    on_context, on_message_end, on_message_update, on_session_before_compact, on_session_compact,
    on_session_shutdown, on_session_start, on_session_tree, on_tool_execution_end,
    on_tool_execution_start, prompt, score, trace, CapsHost, StatusHost,
};
use kiqi_plugin::{
    Event, EventAction, EventTag, HostCaps, Plugin, PluginError, ProgressSink, ToolOutcome,
    ToolSpec,
};
use serde_json::Value;

const SCORE_SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["create","list"]},"name":{"type":"string"},"value":{"type":["number","string","object"]},"traceId":{"type":"string"},"observationId":{"type":"string"},"comment":{"type":"string"},"userId":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100},"page":{"type":"integer","minimum":1}}}"#;
const PROMPT_SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["create","get","list"]},"name":{"type":"string"},"prompt":{"type":"string"},"config":{"type":"object"},"version":{"type":"integer"},"limit":{"type":"integer","minimum":1,"maximum":100},"page":{"type":"integer","minimum":1}}}"#;
const TRACE_SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["get","list"]},"id":{"type":"string","description":"trace id (action=get)"},"userId":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":1000}}}"#;

/// One subscribed event handler.
pub(crate) type Handler = fn(&Event, &dyn StatusHost) -> i32;

/// The 14 tags kiqi-langfuse subscribes to.
///
/// This table **is** the registration: a tag absent here is one the plugin
/// does not observe, and there is deliberately no silent catch-all. The
/// compile-time host calls [`Plugin::on_event`] for every event, so this is
/// what keeps an unhandled tag from being swallowed by `_ => {}`.
pub(crate) fn handler_for(tag: EventTag) -> Option<Handler> {
    let handler: Handler = match tag {
        EventTag::SessionStart => on_session_start,
        EventTag::BeforeAgentStart => on_before_agent_start,
        EventTag::Context => on_context,
        EventTag::AgentStart => on_agent_start,
        EventTag::BeforeProviderRequest => on_before_provider_request,
        EventTag::MessageUpdate => on_message_update,
        EventTag::MessageEnd => on_message_end,
        EventTag::ToolExecutionStart => on_tool_execution_start,
        EventTag::ToolExecutionEnd => on_tool_execution_end,
        EventTag::SessionBeforeCompact => on_session_before_compact,
        EventTag::SessionCompact => on_session_compact,
        EventTag::SessionTree => on_session_tree,
        EventTag::AgentSettled => on_agent_settled,
        EventTag::SessionShutdown => on_session_shutdown,
        _ => return None,
    };
    Some(handler)
}

pub struct LangfusePlugin;

impl LangfusePlugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LangfusePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for LangfusePlugin {
    fn id(&self) -> &str {
        "kiqi-langfuse"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let schema =
            |text: &str| serde_json::from_str(text).expect("langfuse schema is valid JSON");
        vec![
            ToolSpec {
                name: "langfuse_score".into(),
                description: "Create or list Langfuse scores".into(),
                parameters: schema(SCORE_SCHEMA),
            },
            ToolSpec {
                name: "langfuse_prompt".into(),
                description: "Create, get, or list Langfuse prompts".into(),
                parameters: schema(PROMPT_SCHEMA),
            },
            ToolSpec {
                name: "langfuse_trace".into(),
                description: "Query Langfuse traces (Langfuse v4 Observations API v2). 'get' returns every observation of one trace; 'list' returns one root observation per trace. 'update' is gone in v4.".into(),
                parameters: schema(TRACE_SCHEMA),
            },
        ]
    }

    /// The native tools went through a `StepHandle` start/poll/cancel/destroy
    /// shell only because the C ABI cannot block; the implementations are
    /// synchronous, so here they are called directly.
    fn call(
        &self,
        name: &str,
        arguments: &Value,
        caps: &HostCaps<'_>,
        sink: &dyn ProgressSink,
    ) -> Result<ToolOutcome, PluginError> {
        let _ = (caps, sink);
        let result = match name {
            "langfuse_score" => score(arguments),
            "langfuse_prompt" => prompt(arguments),
            "langfuse_trace" => trace(arguments),
            other => return Err(PluginError::UnknownTool(other.to_string())),
        };
        Ok(match result {
            Ok(text) => ToolOutcome::text(text),
            Err(error) => ToolOutcome::error(error),
        })
    }

    fn on_event(&self, event: &Event, caps: &HostCaps<'_>) -> EventAction {
        let host = CapsHost(caps);
        if let Some(handler) = handler_for(event.tag) {
            let _ = handler(event, &host);
        }
        EventAction::Continue
    }
}

/// The factory the host registry calls.
pub fn factory() -> Box<dyn Plugin> {
    Box::new(LangfusePlugin::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_subscribed_tag_has_a_handler() {
        // The mapping the native `export_plugin!` table had, asserted entry by
        // entry so a swap cannot pass as "some handler".
        let expected: &[(EventTag, Handler)] = &[
            (EventTag::SessionStart, on_session_start),
            (EventTag::BeforeAgentStart, on_before_agent_start),
            (EventTag::Context, on_context),
            (EventTag::AgentStart, on_agent_start),
            (EventTag::BeforeProviderRequest, on_before_provider_request),
            (EventTag::MessageUpdate, on_message_update),
            (EventTag::MessageEnd, on_message_end),
            (EventTag::ToolExecutionStart, on_tool_execution_start),
            (EventTag::ToolExecutionEnd, on_tool_execution_end),
            (EventTag::SessionBeforeCompact, on_session_before_compact),
            (EventTag::SessionCompact, on_session_compact),
            (EventTag::SessionTree, on_session_tree),
            (EventTag::AgentSettled, on_agent_settled),
            (EventTag::SessionShutdown, on_session_shutdown),
        ];
        assert_eq!(expected.len(), 14, "the native ABI registered 14 tags");
        for (tag, handler) in expected {
            assert_eq!(
                handler_for(*tag),
                Some(*handler),
                "tag {tag:?} is subscribed but dispatched to the wrong handler"
            );
        }
    }

    #[test]
    fn unsubscribed_tags_are_not_dispatched() {
        // A representative sample of the tags kiqi-langfuse never registered.
        for tag in [
            EventTag::AgentEnd,
            EventTag::TurnStart,
            EventTag::TurnEnd,
            EventTag::MessageStart,
            EventTag::ToolCall,
            EventTag::ToolResult,
            EventTag::Input,
        ] {
            assert!(
                handler_for(tag).is_none(),
                "tag {tag:?} must not be handled"
            );
        }
    }

    #[test]
    fn tool_specs_match_the_native_registration() {
        let plugin = LangfusePlugin::new();
        let names: Vec<String> = plugin.tools().into_iter().map(|tool| tool.name).collect();
        assert_eq!(
            names,
            vec!["langfuse_score", "langfuse_prompt", "langfuse_trace"]
        );
        assert_eq!(plugin.id(), "kiqi-langfuse");
    }
}
