//! Shared host state for MCP signals: the arousal budget plus the durable
//! session facts behind `/signals` and the `signals` tool.
//!
//! `signal_wake` owns the decision policy; this module owns the things that
//! outlive one decision:
//!
//! * the session-level mute (`mcp.signals.muted`), and
//! * the attention inbox (`mcp.signals.inbox`): signals that were allowed to
//!   wake the session, which the agent can `defer`, `resolve` or `dismiss`.
//!
//! Both are ordinary session values (`ki.value` custom entries), so they are
//! durable, latest-wins, and survive recovery without a new storage primitive.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use kiqi_harness::session::{Session, SessionValueWriter, SessionValues};
use kiqi_mcp::{McpNoticeBatch, SignalsConfig};
use serde::{Deserialize, Serialize};

use crate::signal_wake::{SignalWakeState, WakeDecision};

/// Session value: whether signal-triggered wakes are muted for this session.
pub(crate) const SESSION_MUTE_KEY: &str = "mcp.signals.muted";
/// Session value: the attention inbox.
pub(crate) const INBOX_KEY: &str = "mcp.signals.inbox";
/// Same bound as the notice mailbox: a burst must not grow the session file
/// without limit. Oldest entries are dropped first.
pub(crate) const MAX_INBOX_ENTRIES: usize = 64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InboxStatus {
    /// Delivered, no disposition chosen yet.
    #[default]
    Pending,
    /// The agent chose to handle it later (with an optional note).
    Deferred,
}

/// One signal the agent can still act on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InboxEntry {
    pub id: u64,
    pub server: String,
    pub uri: String,
    /// Unix milliseconds of the latest update for this `(server, uri)`.
    pub received_at: i64,
    #[serde(default)]
    pub status: InboxStatus,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Inbox {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    entries: Vec<InboxEntry>,
}

/// What to do with one inbox entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settle {
    Defer(Option<String>),
    Resolve,
    Dismiss(Option<String>),
}

/// Session-scoped host state, shared by the TUI boundary, `/signals`, and the
/// `signals` tool.
pub(crate) struct SignalHost {
    wake: Mutex<SignalWakeState>,
    session: Mutex<Option<Session>>,
}

impl Default for SignalHost {
    fn default() -> Self {
        Self::new()
    }
}

impl SignalHost {
    pub(crate) fn new() -> Self {
        Self {
            wake: Mutex::new(SignalWakeState::new()),
            session: Mutex::new(None),
        }
    }

    /// Attach the session once it exists (`build_harness` creates tools before
    /// the session). The tool is unusable until then; the TUI only runs after.
    pub(crate) fn attach_session(&self, session: Session) {
        *self.session.lock().unwrap() = Some(session);
    }

    fn session(&self) -> Option<Session> {
        self.session.lock().unwrap().clone()
    }

    /// Attach `session` and load its persisted mute in one step. The two must
    /// not be separable: a host attached to one session while holding another's
    /// mute would persist the flag into the wrong file.
    pub(crate) async fn attach_session_and_load_mute(
        &self,
        session: Session,
    ) -> Result<(), String> {
        self.attach_session(session);
        self.load_session_mute().await
    }

    /// Load the persisted session mute into the wake state. Called after
    /// [`attach_session`](Self::attach_session) (or via
    /// [`attach_session_and_load_mute`](Self::attach_session_and_load_mute)).
    ///
    /// The mute is per-session, so an absent value means "unmuted": it
    /// overwrites the previous flag rather than leaving it alone, otherwise
    /// switching from a muted session to an unmuted one would keep the old mute.
    pub(crate) async fn load_session_mute(&self) -> Result<(), String> {
        let Some(session) = self.session() else {
            return Ok(());
        };
        let values = SessionValues::load(&session)
            .await
            .map_err(|error| format!("could not read session values: {error}"))?;
        let muted = values.get_as::<bool>(SESSION_MUTE_KEY).unwrap_or(false);
        self.wake.lock().unwrap().set_session_muted(muted);
        Ok(())
    }

    pub(crate) fn record_user_input(&self) {
        self.wake.lock().unwrap().record_user_input();
    }

    pub(crate) fn record_abort(&self) {
        self.wake.lock().unwrap().record_abort();
    }

    pub(crate) fn muted_until_input(&self) -> bool {
        self.wake.lock().unwrap().muted_until_input()
    }

    pub(crate) fn allow_goal_continuation(&self, key: &str, unbounded: bool) -> bool {
        self.wake
            .lock()
            .unwrap()
            .allow_goal_continuation(key, unbounded)
    }

    /// The id of the session this host is attached to, when it identifies one.
    ///
    /// Taken from the attached [`Session`] rather than from
    /// [`kiqi_plugin_sdk::AGENT_ID_ENV`]: that variable is a process-global cell
    /// rewritten by whichever code path activates a session, so reading it here
    /// would make a per-session decision depend on global mutation order (and
    /// made the attribution tests race each other). `set_active_session`
    /// re-attaches on every switch/fork, which is what keeps this current.
    ///
    /// Sync by construction — `Session::storage()` and
    /// `SessionStorage::metadata()` are both sync — so [`Self::decide`] needs no
    /// await. `None` before `attach_session` (tools are built before the
    /// session exists) and for the shared `ephemeral` literal.
    fn current_session_id(&self) -> Option<String> {
        let session = self.session.lock().unwrap().clone()?;
        let id = session.storage().metadata().id;
        crate::session::session_id_from(Some(&id))
    }

    pub(crate) fn decide<F>(
        &self,
        batch: &McpNoticeBatch,
        now: std::time::Instant,
        config: F,
    ) -> WakeDecision
    where
        F: Fn(&str, &str) -> Option<SignalsConfig>,
    {
        // Resolved per decision, from the session this host is serving now: the
        // MCP server can only report the id it was spawned with, so comparing
        // against that snapshot stops suppressing as soon as you switch or fork.
        let current = self.current_session_id();
        self.wake
            .lock()
            .unwrap()
            .decide(batch, current.as_deref(), now, config)
    }

    pub(crate) fn set_session_muted(&self, muted: bool) {
        self.wake.lock().unwrap().set_session_muted(muted);
    }

    pub(crate) fn session_muted(&self) -> bool {
        self.wake.lock().unwrap().session_muted()
    }

    /// Persist the session mute (latest-wins). The in-memory flag is set by the
    /// caller before spawning this, so the UI reflects the change immediately.
    pub(crate) async fn persist_session_muted(&self, muted: bool) -> Result<(), String> {
        let session = self
            .session()
            .ok_or_else(|| "no session attached".to_string())?;
        SessionValueWriter::new(session)
            .set(SESSION_MUTE_KEY, serde_json::json!(muted))
            .await
            .map_err(|error| format!("could not persist the signal mute: {error}"))?;
        Ok(())
    }

    /// Add a woken batch to the attention inbox, coalescing by `(server, uri)`.
    /// Returns the number of live entries. Only called when the host actually
    /// wakes — `off`/`notify` servers keep today's transcript-only behavior.
    pub(crate) async fn add_to_inbox(&self, batch: &McpNoticeBatch) -> Result<usize, String> {
        let session = self
            .session()
            .ok_or_else(|| "no session attached".to_string())?;
        let mut inbox = load_inbox(&session).await?;
        let now = unix_millis();
        for notice in &batch.notices {
            if let Some(entry) = inbox
                .entries
                .iter_mut()
                .find(|entry| entry.server == notice.server && entry.uri == notice.uri)
            {
                entry.received_at = now;
                continue;
            }
            inbox.next_id += 1;
            inbox.entries.push(InboxEntry {
                id: inbox.next_id,
                server: notice.server.clone(),
                uri: notice.uri.clone(),
                received_at: now,
                status: InboxStatus::Pending,
                note: None,
            });
        }
        while inbox.entries.len() > MAX_INBOX_ENTRIES {
            inbox.entries.remove(0);
        }
        save_inbox(&session, &inbox).await?;
        Ok(inbox.entries.len())
    }

    pub(crate) async fn inbox_entries(&self) -> Result<Vec<InboxEntry>, String> {
        let session = self
            .session()
            .ok_or_else(|| "no session attached".to_string())?;
        Ok(load_inbox(&session).await?.entries)
    }

    /// Settle one entry. `defer` keeps it (status + note); `resolve` and
    /// `dismiss` remove it. Unknown ids are rejected with the valid list, so
    /// the model can correct itself instead of silently doing nothing.
    pub(crate) async fn settle(&self, id: u64, action: Settle) -> Result<String, String> {
        let session = self
            .session()
            .ok_or_else(|| "no session attached".to_string())?;
        let mut inbox = load_inbox(&session).await?;
        let Some(index) = inbox.entries.iter().position(|entry| entry.id == id) else {
            let ids: Vec<String> = inbox.entries.iter().map(|e| e.id.to_string()).collect();
            return Err(if ids.is_empty() {
                format!("unknown signal id {id}; the inbox is empty")
            } else {
                format!("unknown signal id {id}; valid ids: {}", ids.join(", "))
            });
        };
        let entry = &inbox.entries[index];
        let label = format!("{}: {}", entry.server, entry.uri);
        let message = match action {
            Settle::Defer(note) => {
                inbox.entries[index].status = InboxStatus::Deferred;
                inbox.entries[index].note = note;
                format!("Deferred signal {id} ({label}).")
            }
            Settle::Resolve => {
                inbox.entries.remove(index);
                format!("Resolved signal {id} ({label}).")
            }
            Settle::Dismiss(note) => {
                inbox.entries.remove(index);
                match note {
                    Some(note) => {
                        format!("Dismissed signal {id} ({label}): {note}")
                    }
                    None => format!("Dismissed signal {id} ({label})."),
                }
            }
        };
        save_inbox(&session, &inbox).await?;
        Ok(message)
    }
}

async fn load_inbox(session: &Session) -> Result<Inbox, String> {
    let values = SessionValues::load(session)
        .await
        .map_err(|error| format!("could not read session values: {error}"))?;
    let mut inbox = values.get_as::<Inbox>(INBOX_KEY).unwrap_or_default();
    // A hand-edited or older value may not carry a usable counter.
    let max_id = inbox
        .entries
        .iter()
        .map(|entry| entry.id)
        .max()
        .unwrap_or(0);
    inbox.next_id = inbox.next_id.max(max_id);
    Ok(inbox)
}

async fn save_inbox(session: &Session, inbox: &Inbox) -> Result<(), String> {
    let value = serde_json::to_value(inbox)
        .map_err(|error| format!("could not encode the signal inbox: {error}"))?;
    SessionValueWriter::new(session.clone())
        .set(INBOX_KEY, value)
        .await
        .map_err(|error| format!("could not persist the signal inbox: {error}"))?;
    Ok(())
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiqi_harness::session::memory::{InMemorySessionStorage, SystemClock};
    use kiqi_harness::session::session::DefaultIdGenerator;
    use kiqi_harness::session::types::SessionMetadata;
    use kiqi_mcp::{McpNotice, McpNoticeBatch};
    use std::sync::Arc;

    fn session() -> Session {
        session_with_id("s-signals")
    }

    fn session_with_id(id: &str) -> Session {
        let metadata = SessionMetadata {
            id: id.to_string(),
            created_at: 0,
            parent_session_id: None,
        };
        let storage = InMemorySessionStorage::new(
            metadata,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        );
        Session::new(Arc::new(storage), None)
    }

    fn host() -> SignalHost {
        let host = SignalHost::new();
        host.attach_session(session());
        host
    }

    fn host_serving(id: &str) -> SignalHost {
        let host = SignalHost::new();
        host.attach_session(session_with_id(id));
        host
    }

    fn batch(notices: &[(&str, &str)]) -> McpNoticeBatch {
        McpNoticeBatch {
            notices: notices
                .iter()
                .map(|(server, uri)| McpNotice {
                    server: server.to_string(),
                    uri: uri.to_string(),
                    content: None,
                    authored_sessions: Vec::new(),
                })
                .collect(),
            suppressed: 0,
        }
    }

    /// The host must resolve the session it is serving and hand it to the wake
    /// policy — through the real channel, not a stub.
    ///
    /// `signal_wake`'s own tests pass `current_session` explicitly, so they all
    /// stay green if this call site drops it. Verified: replacing
    /// `current_session_id()` with `None` here left every `signal_wake` test
    /// passing. The whole fix would then be dead — a server reports the sessions
    /// that authored the entries, nothing compares them, and every own write wakes
    /// the session again, which is the bug this branch exists to close. This test
    /// is the only one that would notice.
    ///
    /// The id comes from the attached session, matching production: this is
    /// deliberately *not* driven through `KI_AGENT_ID`, because that would make
    /// the assertion depend on a process-global cell that unrelated tests also
    /// write (one such sibling poisoned the env lock and took 92 tests with it).
    #[tokio::test]
    async fn decide_reads_the_session_this_process_is_serving() {
        // `decide` wants the per-request config lookup, not a resolved value.
        let cfg = |_server: &str, _uri: &str| {
            Some(SignalsConfig {
                wake: kiqi_mcp::WakeMode::Auto,
                ..SignalsConfig::default()
            })
        };
        let mut own = batch(&[("collab", "walgit://refs/o/r")]);
        own.notices[0].authored_sessions = vec!["s-serving".to_string()];

        // A fresh host per case: a wake spends the arousal budget, and a later
        // `Skip("no auto server is within its budget")` would be indistinguishable
        // from the echo skip this test is about.
        assert_eq!(
            host_serving("s-serving").decide(&own, std::time::Instant::now(), cfg),
            WakeDecision::Skip("the update was authored by this session"),
            "the host must compare against the session it is serving"
        );

        // A different attached session means a different author: news, not an echo.
        assert!(matches!(
            host_serving("s-other").decide(&own, std::time::Instant::now(), cfg),
            WakeDecision::Wake(_)
        ));

        // `--no-session` carries the shared literal, which is not an identity.
        assert!(matches!(
            host_serving("ephemeral").decide(&own, std::time::Instant::now(), cfg),
            WakeDecision::Wake(_)
        ));

        // Nothing attached yet (tools are built before the session): no id, and
        // nothing may be suppressed in the meantime.
        assert!(matches!(
            SignalHost::new().decide(&own, std::time::Instant::now(), cfg),
            WakeDecision::Wake(_)
        ));
    }

    #[tokio::test]
    async fn inbox_assigns_ids_coalesces_and_survives_a_reload() {
        let host = host();
        assert_eq!(
            host.add_to_inbox(&batch(&[("docs", "file:///a.md")]))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            host.add_to_inbox(&batch(&[("docs", "file:///a.md"), ("code", "mem://x")]))
                .await
                .unwrap(),
            2
        );
        let entries = host.inbox_entries().await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 1);
        assert!(entries.iter().any(|e| e.server == "code" && e.id == 2));
    }

    #[tokio::test]
    async fn defer_keeps_resolve_removes_and_dismiss_reports() {
        let host = host();
        host.add_to_inbox(&batch(&[
            ("docs", "file:///a.md"),
            ("code", "mem://x"),
            ("other", "mem://y"),
        ]))
        .await
        .unwrap();
        host.settle(1, Settle::Defer(Some("after the current task".into())))
            .await
            .unwrap();
        let entries = host.inbox_entries().await.unwrap();
        let deferred = entries.iter().find(|e| e.id == 1).unwrap();
        assert_eq!(deferred.status, InboxStatus::Deferred);
        assert_eq!(deferred.note.as_deref(), Some("after the current task"));

        host.settle(2, Settle::Resolve).await.unwrap();
        let message = host
            .settle(3, Settle::Dismiss(Some("not relevant".into())))
            .await
            .unwrap();
        assert!(message.contains("not relevant"), "got {message:?}");
        let entries = host.inbox_entries().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 1);
    }

    #[tokio::test]
    async fn unknown_id_lists_the_valid_ids() {
        let host = host();
        host.add_to_inbox(&batch(&[("docs", "file:///a.md")]))
            .await
            .unwrap();
        let error = host.settle(99, Settle::Resolve).await.unwrap_err();
        assert!(error.contains("valid ids: 1"), "got {error:?}");
    }

    #[tokio::test]
    async fn the_inbox_is_capped_oldest_first() {
        let host = host();
        let notices: Vec<(String, String)> = (0..(MAX_INBOX_ENTRIES + 3))
            .map(|i| ("docs".to_string(), format!("file:///{i}.md")))
            .collect();
        let refs: Vec<(&str, &str)> = notices
            .iter()
            .map(|(server, uri)| (server.as_str(), uri.as_str()))
            .collect();
        host.add_to_inbox(&batch(&refs)).await.unwrap();
        let entries = host.inbox_entries().await.unwrap();
        assert_eq!(entries.len(), MAX_INBOX_ENTRIES);
        assert_eq!(entries.first().unwrap().id, 4);
    }

    #[tokio::test]
    async fn session_mute_round_trips_through_session_values() {
        let host = host();
        host.set_session_muted(true);
        host.persist_session_muted(true).await.unwrap();
        let values = SessionValues::load(&host.session().unwrap()).await.unwrap();
        assert_eq!(values.get_as::<bool>(SESSION_MUTE_KEY), Some(true));
        // Reload into a fresh wake state.
        let fresh = SignalHost::new();
        fresh.attach_session(host.session().unwrap());
        fresh.load_session_mute().await.unwrap();
        assert!(fresh.session_muted());
    }

    /// The mute follows the attached session, not the host: a session without a
    /// mute clears whatever the previous session carried.
    #[tokio::test]
    async fn attaching_an_unmuted_session_clears_a_previous_mute() {
        let muted_session = session();
        let host = SignalHost::new();
        host.attach_session(muted_session.clone());
        host.set_session_muted(true);
        host.persist_session_muted(true).await.unwrap();
        assert!(host.session_muted());

        // A fresh in-memory session has no `mcp.signals.muted` value.
        host.attach_session_and_load_mute(session()).await.unwrap();
        assert!(
            !host.session_muted(),
            "an unmuted session must clear the previous session's mute"
        );

        // Re-attaching the muted session brings the flag back.
        host.attach_session_and_load_mute(muted_session)
            .await
            .unwrap();
        assert!(host.session_muted());
    }
}
