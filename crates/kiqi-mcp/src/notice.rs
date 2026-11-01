//! The notice mailbox: where `notifications/resources/updated` lands.
//!
//! Product semantic (deliberately different from the retired adapter): a
//! resource update produces a **light notice** — server name + uri, never the
//! content. Content is re-read on demand through the `{server}__read_resource`
//! tool, by the model or the host. The one exception is opt-in: a uri listed in
//! a server's `autoRead` config is read at the boundary drain, and its text
//! rides the notice entry (bounded). The mailbox itself never issues a read; it
//! only carries what the drain put there.
//!
//! The mailbox is bounded and coalescing: a newer update for an already-pending
//! uri REPLACES the pending one (the pending notice carried no content, so the
//! replacement loses nothing), and pushes past the cap drop with a counter.
//! Every suppressed update — coalesced or dropped — is folded into the next
//! drained batch as an "N update(s) suppressed" line, so a consumer can tell
//! "changed once" from "changed many times since you looked".
//!
//! Coalescing only covers ONE accumulation window. A resource that changes once
//! per turn would still put an identical "this changed, and I will not tell you
//! what it looks like" pointer in every turn's transcript, which is noise the
//! model cannot act on: it has to read the resource to learn anything either
//! way. So a pointer is delivered once and then held (`announced`) until the
//! resource is actually read — further updates fold into the suppressed count
//! instead. Content notices are never held: their content *is* the news.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use tokio::sync::Notify;

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
    /// Content read eagerly at the boundary because the uri is listed in the
    /// server's `autoRead` config. `None` is the default (light) notice; the
    /// content is already bounded at `MAX_RESOURCE_TEXT` with the usual
    /// truncation marker.
    pub content: Option<String>,
    /// Sessions the server says authored the changed entries (`_meta.sessions`).
    /// Empty when the server reports none (an older server, or entries written
    /// before sessions were recorded), and an empty list never counts as an
    /// echo.
    ///
    /// Deliberately the *raw* session list, not a pre-computed "this is my own
    /// write" flag: the comparison happens at decision time against the session
    /// ki is serving *then*. A server can only report the session id it was
    /// spawned with, so a flag computed from that snapshot goes stale the moment
    /// the user switches or forks a session — and would then let every own write
    /// wake the session again.
    pub authored_sessions: Vec<String>,
}

impl McpNotice {
    /// Whether this update is an echo of a write made by `current_session`.
    ///
    /// Sound only when the server attributes writes to a session at all:
    /// without provenance (`authored_sessions` empty) the answer is false, so
    /// behavior for older servers is unchanged. A batch mixing our writes with
    /// a peer's is never an echo — the peer's entry is news.
    pub fn is_own_echo(&self, current_session: Option<&str>) -> bool {
        let Some(current) = current_session else {
            return false;
        };
        !self.authored_sessions.is_empty()
            && self
                .authored_sessions
                .iter()
                .all(|session| session == current)
    }
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
    /// Resources whose pointer was already delivered and not read since. A
    /// pointer carries no content, so a repeat tells the model nothing new.
    announced: HashSet<(String, String)>,
}

/// Session-scoped sink for resource-update notices. One per
/// [`McpManager`](crate::McpManager), shared by every server's client handler.
#[derive(Default)]
pub struct McpNoticeMailbox {
    inner: Mutex<Inner>,
    /// Wakes the host's MUI input adapter when a notice lands. `notify_one`
    /// stores a permit, so a push before the waiter starts is not lost.
    notify: Notify,
}

impl McpNoticeMailbox {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record one update with no provenance. Coalesces by (server, uri); counts
    /// as suppressed when it replaces a pending notice or the mailbox is at cap.
    ///
    /// A test seam, not dead code: the real path always carries `_meta`, so every
    /// production caller goes through [`Self::push_with_provenance`]. Kept out of
    /// non-test builds so it cannot quietly become a second, provenance-dropping
    /// entry point that the echo logic would then never see.
    #[cfg(test)]
    pub(crate) fn push(&self, server: &str, uri: &str) {
        self.push_with_provenance(server, uri, Vec::new());
    }

    /// Record one update with its provenance (the authored sessions the server
    /// reported). A self-authored update still coalesces/counts like any other;
    /// the list is carried to the wake decision so an echo of our own write can
    /// be recorded without a turn.
    pub(crate) fn push_with_provenance(
        &self,
        server: &str,
        uri: &str,
        authored_sessions: Vec<String>,
    ) {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(entry) = inner
                .entries
                .iter_mut()
                .find(|entry| entry.server == server && entry.uri == uri)
            {
                // A newer update for the same uri: the pending notice carries no
                // content, so replacing it loses nothing — but the replaced event
                // is one the model will never see individually. Carry the newer
                // provenance too: the batch must reflect the latest write.
                entry.authored_sessions = authored_sessions;
                inner.suppressed += 1;
            } else if inner.entries.len() >= MAX_NOTICE_ENTRIES {
                inner.suppressed += 1;
            } else {
                inner.entries.push_back(McpNotice {
                    server: server.to_string(),
                    uri: uri.to_string(),
                    content: None,
                    authored_sessions,
                });
            }
        }
        // Even a coalesced/dropped update means "state changed since you last
        // looked"; the waiter may have drained the previous entry already.
        self.notify.notify_one();
    }

    /// Wait for the next [`McpNoticeMailbox::push`]. A notice pushed before the
    /// call returns immediately (the permit is stored). The caller must still
    /// [`take`](Self::take) the mailbox: this is an edge, not the data.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }

    /// Forget every pending notice for one (server, uri) — a subscription
    /// lifetime was rotated or removed, so anything queued under the old one
    /// is stale content that must never deliver (upstream's queue purge).
    pub(crate) fn purge(&self, server: &str, uri: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .entries
            .retain(|entry| !(entry.server == server && entry.uri == uri));
        // A rotated or removed subscription re-arms the pointer: the new one has
        // not announced anything yet.
        inner
            .announced
            .remove(&(server.to_string(), uri.to_string()));
    }

    /// Forget every pending notice for one server — its whole connection was
    /// restored or torn down (upstream's `clear_pending_for_key`).
    pub(crate) fn purge_server(&self, server: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.retain(|entry| entry.server != server);
        inner.announced.retain(|(owner, _)| owner != server);
    }

    /// Split a drained batch into the notices that are news and the ones that
    /// repeat a pointer already delivered: the first returns the delivered
    /// notices plus how many were folded away (to add to `suppressed`).
    ///
    /// A notice carrying content is always news — the content is the payload.
    pub(crate) fn keep_news(&self, notices: Vec<McpNotice>) -> (Vec<McpNotice>, u64) {
        let mut inner = self.inner.lock().unwrap();
        let mut folded = 0;
        let kept = notices
            .into_iter()
            .filter(|notice| {
                if notice.content.is_some() {
                    return true;
                }
                let key = (notice.server.clone(), notice.uri.clone());
                if inner.announced.contains(&key) {
                    folded += 1;
                    return false;
                }
                inner.announced.insert(key);
                true
            })
            .collect();
        (kept, folded)
    }

    /// Re-arm one resource: whoever asked now holds its content, so the next
    /// update is news again rather than a repeat.
    pub(crate) fn mark_read(&self, server: &str, uri: &str) {
        self.inner
            .lock()
            .unwrap()
            .announced
            .remove(&(server.to_string(), uri.to_string()));
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
/// appends (the goal extension's injection precedent).
pub const NOTICE_CUSTOM_TYPE: &str = "mcp-resource-notice";

/// Render a drained batch as the text of ONE batched transcript entry: a
/// header, one `server: uri` line per notice, and the suppressed count when
/// nonzero. A notice with `content` (only `autoRead` uris ever have one) adds
/// it as an indented block; every other notice stays a pointer and reading
/// happens on demand.
pub fn notice_entry_text(batch: &McpNoticeBatch) -> String {
    let mut text = String::from("MCP resources updated:");
    // Only `autoRead` uris carry content; point at the on-demand read tool for
    // every notice that stayed a light pointer (a mixed batch gets both).
    if batch.notices.iter().any(|notice| notice.content.is_none()) {
        text.push_str(
            " (resources without content were not auto-read; read with `<server>__read_resource` if needed)",
        );
    }
    for notice in &batch.notices {
        text.push_str(&format!("\n- {}: {}", notice.server, notice.uri));
        if let Some(content) = &notice.content {
            for line in content.lines() {
                text.push_str("\n  ");
                text.push_str(line);
            }
        }
    }
    if batch.suppressed > 0 {
        text.push_str(&format!("\n({} update(s) suppressed)", batch.suppressed));
    }
    text
}

/// The ONE batched transcript entry a boundary drain appends (the goal extension
/// injection precedent): a generic custom message, stored as an ordinary entry
/// and forwarded to the model as a user message on the NEXT context build — no
/// harness-specific projector required, and never a mid-run splice (drains
/// happen only at mode boundaries).
pub fn notice_message(batch: &McpNoticeBatch) -> kiqi_agent::message::AgentMessage {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    kiqi_agent::message::AgentMessage::Custom(kiqi_agent::message::CustomMessage::new(
        "custom",
        vec![kiqi_ai::types::Content::text(notice_entry_text(batch))],
        serde_json::json!({ "customType": NOTICE_CUSTOM_TYPE, "display": true }),
        timestamp,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[test]
    fn a_coalesced_update_carries_the_newer_provenance() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push_with_provenance("docs", "file:///a.md", Vec::new());
        mailbox.push_with_provenance("docs", "file:///a.md", vec!["s1".to_string()]);
        let batch = mailbox.take();
        assert_eq!(batch.notices.len(), 1);
        assert!(
            batch.notices[0].is_own_echo(Some("s1")),
            "the latest update's provenance must win"
        );
    }

    #[test]
    fn an_echo_needs_a_current_session_and_a_full_match() {
        let notice = McpNotice {
            server: "collab".into(),
            uri: "walgit://refs/o/r".into(),
            content: None,
            authored_sessions: vec!["me".into()],
        };
        assert!(notice.is_own_echo(Some("me")));
        // No session of our own to compare against (ephemeral, or not yet
        // published): claiming an echo would hide a peer's update.
        assert!(!notice.is_own_echo(None));
        // A peer's write rides the same batch, so the batch is news.
        let mixed = McpNotice {
            authored_sessions: vec!["me".into(), "peer".into()],
            ..notice.clone()
        };
        assert!(!mixed.is_own_echo(Some("me")));
        // Without provenance every update is news, as for an older server.
        let unattributed = McpNotice {
            authored_sessions: Vec::new(),
            ..notice
        };
        assert!(!unattributed.is_own_echo(Some("me")));
    }

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
                    content: None,
                    authored_sessions: Vec::new(),
                },
                McpNotice {
                    server: "code".into(),
                    uri: "mem://x".into(),
                    content: None,
                    authored_sessions: Vec::new(),
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
                content: None,
                authored_sessions: Vec::new(),
            }],
            suppressed: 0,
        };
        let text = notice_entry_text(&batch);
        assert!(!text.contains("suppressed"), "got {text:?}");
    }

    #[test]
    fn auto_read_content_rides_the_entry_as_an_indented_block() {
        let batch = McpNoticeBatch {
            notices: vec![
                McpNotice {
                    server: "docs".into(),
                    uri: "file:///a.md".into(),
                    content: Some("line one\nline two".into()),
                    authored_sessions: Vec::new(),
                },
                McpNotice {
                    server: "docs".into(),
                    uri: "file:///b.md".into(),
                    content: None,
                    authored_sessions: Vec::new(),
                },
            ],
            suppressed: 0,
        };
        let text = notice_entry_text(&batch);
        assert!(
            text.contains("- docs: file:///a.md\n  line one\n  line two"),
            "got {text:?}"
        );
        assert!(text.contains("- docs: file:///b.md"), "got {text:?}");
        // The light notice stays a pointer: no fabricated content block.
        assert_eq!(text.matches("  line one").count(), 1);
    }

    #[tokio::test]
    async fn push_wakes_a_waiter_even_when_coalesced() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "file:///a.md");
        // The permit is stored before the wait starts.
        tokio::time::timeout(std::time::Duration::from_secs(1), mailbox.notified())
            .await
            .expect("a stored permit must satisfy the first wait");
        let wait = mailbox.notified();
        // A coalesced update still means "state changed" for the MUI adapter.
        mailbox.push("docs", "file:///a.md");
        tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .expect("a coalesced update must still wake the MUI adapter");
    }

    #[test]
    fn a_pointer_is_delivered_once_until_the_resource_is_read() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "mem://alpha");
        let (kept, folded) = mailbox.keep_news(mailbox.take().notices);
        assert_eq!(kept.len(), 1, "the first pointer is news");
        assert_eq!(folded, 0);

        mailbox.push("docs", "mem://alpha");
        let (kept, folded) = mailbox.keep_news(mailbox.take().notices);
        assert!(
            kept.is_empty(),
            "a repeated pointer tells the model nothing it can act on: {kept:?}"
        );
        assert_eq!(folded, 1);

        mailbox.mark_read("docs", "mem://alpha");
        mailbox.push("docs", "mem://alpha");
        let (kept, folded) = mailbox.keep_news(mailbox.take().notices);
        assert_eq!(kept.len(), 1, "reading the resource re-arms the pointer");
        assert_eq!(folded, 0);
    }

    #[test]
    fn each_resource_holds_its_own_pointer() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "mem://alpha");
        mailbox.push("docs", "mem://beta");
        assert_eq!(mailbox.keep_news(mailbox.take().notices).0.len(), 2);
        // One resource's pointer says nothing about another's.
        mailbox.push("docs", "mem://beta");
        let (kept, folded) = mailbox.keep_news(mailbox.take().notices);
        assert!(kept.is_empty(), "got {kept:?}");
        assert_eq!(folded, 1);
    }

    #[test]
    fn a_content_notice_is_always_news() {
        let mailbox = McpNoticeMailbox::new();
        let notice = McpNotice {
            server: "docs".into(),
            uri: "mem://alpha".into(),
            content: None,
            authored_sessions: Vec::new(),
        };
        assert_eq!(mailbox.keep_news(vec![notice.clone()]).0.len(), 1);
        // The content is the payload: holding it back would drop the update.
        let with_content = McpNotice {
            content: Some("alpha body".into()),
            ..notice
        };
        let (kept, folded) = mailbox.keep_news(vec![with_content]);
        assert_eq!(kept.len(), 1);
        assert_eq!(folded, 0);
        assert_eq!(kept[0].content.as_deref(), Some("alpha body"));
    }

    #[test]
    fn a_rotated_subscription_re_arms_the_pointer() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "mem://alpha");
        assert_eq!(mailbox.keep_news(mailbox.take().notices).0.len(), 1);
        mailbox.push("docs", "mem://alpha");
        assert!(mailbox.keep_news(mailbox.take().notices).0.is_empty());
        // The subscription lifetime that announced it is gone, so the next one
        // announces afresh instead of inheriting a stale "already told you".
        mailbox.purge("docs", "mem://alpha");
        mailbox.push("docs", "mem://alpha");
        assert_eq!(mailbox.keep_news(mailbox.take().notices).0.len(), 1);
    }

    #[test]
    fn a_server_purge_re_arms_every_pointer_it_owned() {
        let mailbox = McpNoticeMailbox::new();
        mailbox.push("docs", "mem://alpha");
        mailbox.push("docs", "mem://beta");
        assert_eq!(mailbox.keep_news(mailbox.take().notices).0.len(), 2);
        mailbox.purge_server("docs");
        mailbox.push("docs", "mem://alpha");
        mailbox.push("docs", "mem://beta");
        assert_eq!(mailbox.keep_news(mailbox.take().notices).0.len(), 2);
    }
}
