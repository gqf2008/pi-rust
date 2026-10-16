//! The notice mailbox: where `notifications/resources/updated` lands.
//!
//! Product semantic (deliberately different from the retired adapter): a
//! resource update produces a **light notice** — server name + uri, never the
//! content. Content is re-read on demand through the `{server}__read_resource`
//! tool, by the model or the host. Nothing here ever issues a `resources/read`.
//!
//! The mailbox is bounded and coalescing: a newer update for an already-pending
//! uri REPLACES the pending one (the pending notice carried no content, so the
//! replacement loses nothing), and pushes past the cap drop with a counter.
//! Every suppressed update — coalesced or dropped — is folded into the next
//! drained batch as an "N update(s) suppressed" line, so a consumer can tell
//! "changed once" from "changed many times since you looked".

use std::collections::VecDeque;
use std::sync::Mutex;

/// Distinct (server, uri) notices the mailbox holds. Upstream's pending queue
/// was bounded at 64 for the same reason: a burst must not grow the host's
/// memory, and the oldest state is the least useful to re-read.
pub(crate) const MAX_NOTICE_ENTRIES: usize = 64;

/// One pending "this resource changed" notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpNotice {
    /// The server the resource lives on (the `{server}__read_resource` prefix).
    pub server: String,
    /// The resource uri, verbatim from the notification.
    pub uri: String,
}

/// What [`McpNoticeMailbox::take`] hands back: the coalesced notices plus how
/// many update events they do not individually represent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpNoticeBatch {
    pub notices: Vec<McpNotice>,
    /// Updates suppressed since the last drain: coalesced into a newer notice
    /// for the same uri, or dropped at the cap. Folded into the delivered
    /// entry as "N update(s) suppressed" when nonzero.
    pub suppressed: u64,
}

impl McpNoticeBatch {
    pub fn is_empty(&self) -> bool {
        self.notices.is_empty()
    }
}

#[derive(Default)]
struct Inner {
    entries: VecDeque<McpNotice>,
    suppressed: u64,
}

/// Session-scoped sink for resource-update notices. One per
/// [`McpManager`](crate::McpManager), shared by every server's client handler.
#[derive(Default)]
pub struct McpNoticeMailbox {
    inner: Mutex<Inner>,
}

impl McpNoticeMailbox {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record one update. Coalesces by (server, uri); counts as suppressed when
    /// it replaces a pending notice or the mailbox is at cap.
    pub(crate) fn push(&self, server: &str, uri: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .entries
            .iter()
            .any(|entry| entry.server == server && entry.uri == uri)
        {
            // A newer update for the same uri: the pending notice carries no
            // content, so replacing it loses nothing — but the replaced event
            // is one the model will never see individually.
            inner.suppressed += 1;
            return;
        }
        if inner.entries.len() >= MAX_NOTICE_ENTRIES {
            inner.suppressed += 1;
            return;
        }
        inner.entries.push_back(McpNotice {
            server: server.to_string(),
            uri: uri.to_string(),
        });
    }

    /// Forget every pending notice for one (server, uri) — a subscription
    /// lifetime was rotated or removed, so anything queued under the old one
    /// is stale content that must never deliver (upstream's queue purge).
    pub(crate) fn purge(&self, server: &str, uri: &str) {
        self.inner
            .lock()
            .unwrap()
            .entries
            .retain(|entry| !(entry.server == server && entry.uri == uri));
    }

    /// Forget every pending notice for one server — its whole connection was
    /// restored or torn down (upstream's `clear_pending_for_key`).
    pub(crate) fn purge_server(&self, server: &str) {
        self.inner
            .lock()
            .unwrap()
            .entries
            .retain(|entry| entry.server != server);
    }

    /// How many notices are currently pending, WITHOUT draining them (drain
    /// with [`McpNoticeMailbox::take`]). Lets a caller wait for a push to have
    /// landed before acting on the mailbox.
    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    /// Drain everything pending. The suppressed counter folds into this batch
    /// and resets (monotonic within one accumulation window).
    pub fn take(&self) -> McpNoticeBatch {
        let mut inner = self.inner.lock().unwrap();
        let notices = std::mem::take(&mut inner.entries).into_iter().collect();
        let suppressed = std::mem::take(&mut inner.suppressed);
        McpNoticeBatch {
            notices,
            suppressed,
        }
    }
}

/// The customType stamped on the batched transcript entry a boundary drain
/// appends (the goal-guest injection precedent).
pub const NOTICE_CUSTOM_TYPE: &str = "mcp-resource-notice";

/// Render a drained batch as the text of ONE batched transcript entry: a
/// header, one `server: uri` line per notice, and the suppressed count when
/// nonzero. Never contains resource content — reading happens on demand.
pub fn notice_entry_text(batch: &McpNoticeBatch) -> String {
    let mut text = String::from("MCP resources updated (content not auto-read; read with `<server>__read_resource` if needed):");
    for notice in &batch.notices {
        text.push_str(&format!("\n- {}: {}", notice.server, notice.uri));
    }
    if batch.suppressed > 0 {
        text.push_str(&format!("\n({} update(s) suppressed)", batch.suppressed));
    }
    text
}

/// The ONE batched transcript entry a boundary drain appends (the goal-guest
/// injection precedent): a generic custom message, stored as an ordinary entry
/// and forwarded to the model as a user message on the NEXT context build — no
/// harness-specific projector required, and never a mid-run splice (drains
/// happen only at mode boundaries).
pub fn notice_message(batch: &McpNoticeBatch) -> rpi_agent::message::AgentMessage {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    rpi_agent::message::AgentMessage::Custom(rpi_agent::message::CustomMessage::new(
        "custom",
        vec![rpi_ai::types::Content::text(notice_entry_text(batch))],
        serde_json::json!({ "customType": NOTICE_CUSTOM_TYPE, "display": true }),
        timestamp,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_for_the_same_uri_coalesce_into_one_notice() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "file:///a.md");
        mailbox.push("docs", "file:///a.md");
        mailbox.push("docs", "file:///a.md");
        let batch = mailbox.take();
        assert_eq!(batch.notices.len(), 1);
        assert_eq!(batch.notices[0].server, "docs");
        assert_eq!(batch.notices[0].uri, "file:///a.md");
        // Two events folded into the one notice.
        assert_eq!(batch.suppressed, 2);
    }

    #[test]
    fn distinct_uris_stay_distinct_and_order_is_arrival_order() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "file:///b.md");
        mailbox.push("code", "mem://x");
        mailbox.push("docs", "file:///a.md");
        let batch = mailbox.take();
        let pairs: Vec<_> = batch
            .notices
            .iter()
            .map(|n| (n.server.as_str(), n.uri.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("docs", "file:///b.md"),
                ("code", "mem://x"),
                ("docs", "file:///a.md")
            ]
        );
        assert_eq!(batch.suppressed, 0);
    }

    #[test]
    fn cap_drops_are_counted_not_lost_silently() {
        let mailbox = McpNoticeMailbox::new();
        for i in 0..MAX_NOTICE_ENTRIES {
            mailbox.push("srv", &format!("uri://{i}"));
        }
        assert_eq!(mailbox.take().notices.len(), MAX_NOTICE_ENTRIES);
        // Refill, then push past the cap: the newest extras drop, each counted.
        for i in 0..(MAX_NOTICE_ENTRIES + 3) {
            mailbox.push("srv", &format!("uri://{i}"));
        }
        let batch = mailbox.take();
        assert_eq!(batch.notices.len(), MAX_NOTICE_ENTRIES);
        assert_eq!(batch.suppressed, 3);
    }

    #[test]
    fn suppressed_counter_folds_into_the_next_drain_and_resets() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "file:///a.md");
        mailbox.push("docs", "file:///a.md");
        let first = mailbox.take();
        assert_eq!(first.suppressed, 1);
        // Nothing new arrived: the next drain is empty and reports nothing.
        let second = mailbox.take();
        assert!(second.is_empty());
        assert_eq!(second.suppressed, 0);
    }

    #[test]
    fn purge_drops_only_the_targeted_lifetime_queue() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "file:///a.md");
        mailbox.push("docs", "file:///b.md");
        mailbox.push("code", "mem://x");
        mailbox.purge("docs", "file:///a.md");
        let batch = mailbox.take();
        let pairs: Vec<_> = batch
            .notices
            .iter()
            .map(|n| (n.server.as_str(), n.uri.as_str()))
            .collect();
        assert_eq!(pairs, vec![("docs", "file:///b.md"), ("code", "mem://x")]);
    }

    #[test]
    fn purge_server_drops_every_notice_of_that_server() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "file:///a.md");
        mailbox.push("docs", "file:///b.md");
        mailbox.push("code", "mem://x");
        mailbox.purge_server("docs");
        let batch = mailbox.take();
        assert_eq!(batch.notices.len(), 1);
        assert_eq!(batch.notices[0].server, "code");
    }

    #[test]
    fn entry_text_carries_server_uri_and_the_suppressed_count() {
        let batch = McpNoticeBatch {
            notices: vec![
                McpNotice {
                    server: "docs".into(),
                    uri: "file:///a.md".into(),
                },
                McpNotice {
                    server: "code".into(),
                    uri: "mem://x".into(),
                },
            ],
            suppressed: 2,
        };
        let text = notice_entry_text(&batch);
        assert!(text.contains("- docs: file:///a.md"), "got {text:?}");
        assert!(text.contains("- code: mem://x"), "got {text:?}");
        assert!(text.contains("(2 update(s) suppressed)"), "got {text:?}");
        // The notice is a pointer, not content: the entry must not grow with
        // resource bodies, so there is nothing here to assert — the shape
        // simply has no field that could carry one.
    }

    #[test]
    fn entry_text_omits_the_suppressed_line_when_zero() {
        let batch = McpNoticeBatch {
            notices: vec![McpNotice {
                server: "docs".into(),
                uri: "file:///a.md".into(),
            }],
            suppressed: 0,
        };
        let text = notice_entry_text(&batch);
        assert!(!text.contains("suppressed"), "got {text:?}");
    }
}
