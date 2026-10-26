//! The dialog pump: extension UI dialogs → `session/request_permission`
//! (batch 2 wave 2, design doc §13).
//!
//! The mailbox is a plain polling structure and the extension "blocks" inside its
//! own runtime action (`UiDialogMailbox` poll loop), so `drive_turn` never
//! sees the dialog — the pump is the only piece that has to exist: it polls
//! `take_pending`, maps `purpose=permission` dialogs onto the ACP permission
//! request (kiqi-acp blueprint semantics), and routes the outcome back into
//! the mailbox. Everything else — `purpose=question`, an unusable shape — is
//! cancelled on the original request id: ACP has no generic question UI and
//! its choices are never reinterpreted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use futures::FutureExt;
use kiqi_host::{UiDialogMailbox, UiDialogRequest};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::diag;

/// The pump's wakeup source (the rpc-mode dialog pump's tick, `modes.rs`).
const DIALOG_TICK: Duration = Duration::from_millis(30);

/// The ACP request derived from one pending dialog, plus the option titles
/// the index-shaped `optionId`s resolve back to.
pub(crate) struct PermissionDialog {
    pub(crate) request: RequestPermissionRequest,
    titles: Vec<String>,
}

/// The kiqi-acp blueprint mapping (`permission_request`): only an explicit
/// `purpose=permission` with a non-empty tool call id and sdk-valid options
/// becomes an ACP permission request. Option ids are the options' indices,
/// the original titles are preserved verbatim, and the tool call carries the
/// dialog's tool call id plus the title fallback chain
/// (`question` → `message` → "Permission required").
pub(crate) fn permission_dialog(
    request: &UiDialogRequest,
    session_id: &SessionId,
) -> Option<PermissionDialog> {
    // Explicit on purpose: the sdk's `validate` also accepts a missing
    // purpose (the TUI renders those as questions), so `validate` alone would
    // relabel generic questions as permissions.
    if request
        .ui
        .get("purpose")
        .and_then(serde_json::Value::as_str)
        != Some("permission")
    {
        return None;
    }
    let tool_call_id = request
        .tool_call_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())?;
    kiqi_host::dialogs::validate(&request.ui, Some(tool_call_id)).ok()?;
    let sdk_options = kiqi_host::dialogs::permission_options(&request.ui).ok()?;
    let title = request
        .ui
        .get("question")
        .or_else(|| request.ui.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Permission required");
    let mut titles = Vec::with_capacity(sdk_options.len());
    let mut options = Vec::with_capacity(sdk_options.len());
    for (index, option) in sdk_options.iter().enumerate() {
        titles.push(option.title.clone());
        options.push(PermissionOption::new(
            index.to_string(),
            option.title.clone(),
            kind_for(&option.permission)?,
        ));
    }
    let tool_call = ToolCallUpdate::new(
        tool_call_id.to_string(),
        ToolCallUpdateFields::new().title(title),
    );
    Some(PermissionDialog {
        request: RequestPermissionRequest::new(session_id.clone(), tool_call, options),
        titles,
    })
}

/// The sdk's permission strings are 1:1 with the ACP kinds; `None` cannot
/// happen for a payload that passed `permission_options`, and callers treat it
/// as "not presentable" rather than guessing a kind.
fn kind_for(permission: &str) -> Option<PermissionOptionKind> {
    match permission {
        "allow_once" => Some(PermissionOptionKind::AllowOnce),
        "allow_always" => Some(PermissionOptionKind::AllowAlways),
        "reject_once" => Some(PermissionOptionKind::RejectOnce),
        "reject_always" => Some(PermissionOptionKind::RejectAlways),
        _ => None,
    }
}

/// The kiqi-acp `permission_answer` mapping: a Selected outcome whose
/// `optionId` parses back to one of OUR indices answers the extension with the
/// option's original title; anything else — Cancelled, an unknown/foreign
/// option id, a future variant — cancels. Never guessed.
pub(crate) fn apply_outcome(
    request_id: &str,
    mailbox: &UiDialogMailbox,
    outcome: RequestPermissionOutcome,
    titles: &[String],
) {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => {
            let chosen = selected
                .option_id
                .to_string()
                .parse::<usize>()
                .ok()
                .and_then(|index| titles.get(index));
            match chosen {
                Some(title) => {
                    let _ = mailbox.respond(request_id, serde_json::json!({ "values": [title] }));
                }
                None => {
                    let _ = mailbox.cancel(request_id);
                }
            }
        }
        RequestPermissionOutcome::Cancelled => {
            let _ = mailbox.cancel(request_id);
        }
        // Non-exhaustive upstream: an unrecognized outcome is a cancelled
        // dialog, never a guessed answer.
        _ => {
            let _ = mailbox.cancel(request_id);
        }
    }
}

/// One dialog, panic-isolated: a poisoned dialog must neither take the pump
/// (or the connection) down nor leave the extension's poll blocked, so the guard
/// logs the panic and releases the mailbox entry.
async fn guarded_dialog<F, T>(request_id: &str, mailbox: &UiDialogMailbox, work: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    match std::panic::AssertUnwindSafe(work).catch_unwind().await {
        Ok(value) => Some(value),
        Err(panic) => {
            diag::diagln!(
                "dialog task {request_id} panicked: {}; cancelled its mailbox entry",
                panic_message(&panic)
            );
            let _ = mailbox.cancel(request_id);
            None
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = panic.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = panic.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// The work of one dialog: present it to the client (unless it was already
/// closed between the pump's take and this task's start), then route the
/// outcome back into the mailbox.
async fn dialog_work(
    request: &UiDialogRequest,
    mailbox: &UiDialogMailbox,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
) {
    if !mailbox.is_pending(&request.request_id) {
        return;
    }
    let Some(dialog) = permission_dialog(request, session_id) else {
        // ACP has no generic question UI and an unusable permission shape
        // cannot be presented either — cancel on the original request id and
        // never reinterpret (kiqi-acp precedent).
        let _ = mailbox.cancel(&request.request_id);
        return;
    };
    // Legal outside the dispatch loop (the crate's blessed context for
    // `block_task`): this runs on a dedicated per-dialog task, never inline in
    // a handler. Dropping an unanswered request on abort auto-sends
    // `$/cancel_request`, so cancelling this task is protocol-clean.
    match cx.send_request(dialog.request).block_task().await {
        Ok(response) => apply_outcome(
            &request.request_id,
            mailbox,
            response.outcome,
            &dialog.titles,
        ),
        Err(error) => {
            // The connection is closing (or answered uncleanly): the extension
            // must never hang on a dead client.
            diag::diagln!(
                "request_permission for {} failed ({error}); cancelling",
                request.request_id
            );
            let _ = mailbox.cancel(&request.request_id);
        }
    }
}

/// The per-live-session dialog pump: attaches the mailbox, polls
/// `take_pending`, and drives one spawned task per dialog so a slow or
/// never-answering client cannot wedge the pump (and a poisoned dialog cannot
/// take the connection down).
pub(crate) struct DialogPump {
    mailbox: UiDialogMailbox,
    stop: CancellationToken,
    tasks: Arc<Mutex<HashMap<String, Option<JoinHandle<()>>>>>,
}

impl DialogPump {
    /// Attaches the mailbox (idempotent) and spawns the pump. `cx` is the
    /// connection the outbound permission requests go out on; the pump holds
    /// clones, so it works for any later turn, not one captured prompt.
    pub(crate) fn start(
        mailbox: UiDialogMailbox,
        cx: ConnectionTo<Client>,
        session_id: SessionId,
    ) -> Arc<Self> {
        mailbox.attach();
        let stop = CancellationToken::new();
        let tasks: Arc<Mutex<HashMap<String, Option<JoinHandle<()>>>>> = Arc::default();
        let pump = Arc::new(Self {
            mailbox: mailbox.clone(),
            stop: stop.clone(),
            tasks: Arc::clone(&tasks),
        });
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(DIALOG_TICK) => {}
                }
                while let Some(request) = mailbox.take_pending() {
                    if stop.is_cancelled() {
                        break;
                    }
                    let request_id = request.request_id.clone();
                    let registry_key = request_id.clone();
                    let registry = Arc::clone(&tasks);
                    // Clones land BEFORE the spawn: the loop's own mailbox /
                    // cx / session_id must survive into the next iteration.
                    let dialog_mailbox = mailbox.clone();
                    let dialog_cx = cx.clone();
                    let dialog_session = session_id.clone();
                    // The slot is planted and filled under ONE hold of the
                    // registry lock: a dialog that finishes before its handle
                    // exists removes the placeholder itself (so the fill below
                    // must not resurrect it), and `abort_tasks` never observes
                    // a slot whose task it cannot abort.
                    let mut registry_guard = tasks.lock().unwrap();
                    registry_guard.insert(registry_key.clone(), None);
                    let handle = tokio::spawn(async move {
                        guarded_dialog(
                            &request_id,
                            &dialog_mailbox,
                            dialog_work(&request, &dialog_mailbox, &dialog_cx, &dialog_session),
                        )
                        .await;
                        registry.lock().unwrap().remove(&request_id);
                    });
                    if let Some(slot) = registry_guard.get_mut(&registry_key) {
                        *slot = Some(handle);
                    }
                    drop(registry_guard);
                }
                if stop.is_cancelled() {
                    break;
                }
            }
        });
        pump
    }

    /// `session/cancel` for the live session: release every open dialog on the
    /// extension side and stop waiting on the client for them. The pump itself
    /// keeps running for the rest of the session.
    pub(crate) fn cancel_all(&self) {
        self.mailbox.cancel_all();
        self.abort_tasks();
    }

    /// Session close. Detach comes FIRST (the rpc-mode EOF order): cancelled
    /// mailbox entries release the extension polls while the extension runtime is
    /// still alive to observe them; the caller then dispatches the
    /// `SessionShutdown` advisory before the reload context drops.
    pub(crate) fn shutdown(&self) {
        self.stop.cancel();
        self.mailbox.detach();
        self.abort_tasks();
    }

    /// Aborting a task that still awaits the client drops its unanswered
    /// `SentRequest`, which auto-sends `$/cancel_request` — protocol-clean.
    fn abort_tasks(&self) {
        for (_, handle) in self.tasks.lock().unwrap().drain() {
            if let Some(handle) = handle {
                handle.abort();
            }
        }
    }
}

impl Drop for DialogPump {
    fn drop(&mut self) {
        // Backstop for a pump dropped without shutdown(): the pump task must
        // not outlive the pump. The mailbox detach stays explicit at the close
        // site — its ordering against the SessionShutdown advisory is the
        // whole point of shutdown().
        self.stop.cancel();
        self.abort_tasks();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permission_ui() -> serde_json::Value {
        serde_json::json!({
            "purpose": "permission",
            "kind": "confirm",
            "question": "Allow this tool?",
            "options": [
                {"title": "允许一次", "permission": "allow_once"},
                {"title": "拒绝", "permission": "reject_once"},
            ],
        })
    }

    fn dialog(ui: serde_json::Value, tool_call_id: Option<&str>) -> (UiDialogRequest, SessionId) {
        (
            UiDialogRequest {
                request_id: "q1".to_string(),
                tool_call_id: tool_call_id.map(str::to_owned),
                ui,
                raw: serde_json::Value::Null,
            },
            SessionId::from("ki-test"),
        )
    }

    // ---- kiqi-acp race property ①: generic questions are never relabelled ----

    #[test]
    fn generic_questions_are_never_relabelled_as_permissions() {
        let mut question = permission_ui();
        question["purpose"] = serde_json::json!("question");
        let (request, sid) = dialog(question, Some("call-1"));
        assert!(permission_dialog(&request, &sid).is_none());

        // A missing purpose is a question too (the sdk's validate accepts it —
        // the explicit purpose gate is what keeps it out of ACP).
        let mut unpurposed = permission_ui();
        unpurposed
            .as_object_mut()
            .unwrap()
            .remove("purpose")
            .expect("purpose key");
        let (request, sid) = dialog(unpurposed, Some("call-1"));
        assert!(permission_dialog(&request, &sid).is_none());
    }

    // ---- race property ②: labels preserved, ids are indices, title fallback ----

    #[test]
    fn permission_mapping_preserves_labels_and_indexes_options() {
        let (request, sid) = dialog(permission_ui(), Some("call-1"));
        let mapped = permission_dialog(&request, &sid).expect("permission shape maps");
        assert_eq!(mapped.request.session_id, sid);
        assert_eq!(mapped.request.tool_call.tool_call_id.to_string(), "call-1");
        assert_eq!(
            mapped.request.tool_call.fields.title.as_deref(),
            Some("Allow this tool?")
        );
        assert_eq!(mapped.request.options.len(), 2);
        assert_eq!(mapped.request.options[0].option_id.to_string(), "0");
        assert_eq!(mapped.request.options[0].name, "允许一次");
        assert_eq!(
            mapped.request.options[0].kind,
            PermissionOptionKind::AllowOnce
        );
        assert_eq!(mapped.request.options[1].option_id.to_string(), "1");
        assert_eq!(mapped.request.options[1].name, "拒绝");
        assert_eq!(
            mapped.request.options[1].kind,
            PermissionOptionKind::RejectOnce
        );
        // The ids resolve back to the original titles.
        assert_eq!(
            mapped.titles,
            vec!["允许一次".to_string(), "拒绝".to_string()]
        );
    }

    #[test]
    fn title_falls_back_through_question_message_default() {
        let mut ui = permission_ui();
        ui.as_object_mut().unwrap().remove("question");
        let (request, sid) = dialog(ui, Some("call-1"));
        let mapped = permission_dialog(&request, &sid).expect("maps");
        assert_eq!(
            mapped.request.tool_call.fields.title.as_deref(),
            Some("Permission required")
        );

        // The fallback is at the KEY level (blueprint wording): a present
        // question key — even null — wins over `message`.
        let mut ui = permission_ui();
        ui.as_object_mut().unwrap().remove("question");
        ui["message"] = serde_json::json!("the message");
        let (request, sid) = dialog(ui, Some("call-1"));
        let mapped = permission_dialog(&request, &sid).expect("maps");
        assert_eq!(
            mapped.request.tool_call.fields.title.as_deref(),
            Some("the message")
        );
    }

    #[test]
    fn unusable_dialogs_do_not_map() {
        // Permission without a tool call id can never be presented (the ACP
        // tool_call is required).
        let (request, sid) = dialog(permission_ui(), None);
        assert!(permission_dialog(&request, &sid).is_none());
        let (request, sid) = dialog(permission_ui(), Some("  "));
        assert!(permission_dialog(&request, &sid).is_none());
        // An unknown purpose is not a permission dialog.
        let mut ui = permission_ui();
        ui["purpose"] = serde_json::json!("approval");
        let (request, sid) = dialog(ui, Some("call-1"));
        assert!(permission_dialog(&request, &sid).is_none());
    }

    // ---- outcome mapping: unknown choice is cancelled, never guessed ----

    #[test]
    fn selected_outcome_answers_with_the_original_title_and_unknown_ids_cancel() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "q1", "toolCallId": "call-1", "ui": permission_ui(),
            }))
            .expect("open");
        let titles = vec!["允许一次".to_string(), "拒绝".to_string()];

        apply_outcome(
            "q1",
            &mailbox,
            RequestPermissionOutcome::Selected(
                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new("1"),
            ),
            &titles,
        );
        let poll = mailbox
            .handle(serde_json::json!({"op": "poll", "requestId": "q1"}))
            .expect("poll");
        assert_eq!(poll["status"], "answered");
        assert_eq!(poll["answer"]["values"][0], "拒绝");

        // Unknown option ids are treated as cancelled (kiqi-acp
        // permission_answer: never guess).
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "q2", "toolCallId": "call-1", "ui": permission_ui(),
            }))
            .expect("open");
        apply_outcome(
            "q2",
            &mailbox,
            RequestPermissionOutcome::Selected(
                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new("allow-made-up"),
            ),
            &titles,
        );
        let poll = mailbox
            .handle(serde_json::json!({"op": "poll", "requestId": "q2"}))
            .expect("poll");
        assert_eq!(poll["status"], "cancelled");

        // An out-of-range index likewise.
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "q3", "toolCallId": "call-1", "ui": permission_ui(),
            }))
            .expect("open");
        apply_outcome(
            "q3",
            &mailbox,
            RequestPermissionOutcome::Selected(
                agent_client_protocol::schema::v1::SelectedPermissionOutcome::new("99"),
            ),
            &titles,
        );
        let poll = mailbox
            .handle(serde_json::json!({"op": "poll", "requestId": "q3"}))
            .expect("poll");
        assert_eq!(poll["status"], "cancelled");

        // Cancelled outcomes cancel.
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "q4", "toolCallId": "call-1", "ui": permission_ui(),
            }))
            .expect("open");
        apply_outcome("q4", &mailbox, RequestPermissionOutcome::Cancelled, &titles);
        let poll = mailbox
            .handle(serde_json::json!({"op": "poll", "requestId": "q4"}))
            .expect("poll");
        assert_eq!(poll["status"], "cancelled");
    }

    // ---- panic isolation ----

    #[tokio::test]
    async fn panicked_dialog_releases_the_mailbox_and_does_not_propagate() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "q1", "toolCallId": "call-1", "ui": permission_ui(),
            }))
            .expect("open");
        // A default panic hook prints the panic; catching it must still yield
        // `None` and release the entry so the extension poll cannot hang.
        let result = guarded_dialog("q1", &mailbox, async { panic!("poisoned dialog") }).await;
        assert!(result.is_none());
        let poll = mailbox
            .handle(serde_json::json!({"op": "poll", "requestId": "q1"}))
            .expect("poll");
        assert_eq!(poll["status"], "cancelled");
        // And the guard passes success through untouched.
        let result = guarded_dialog("q2", &mailbox, async { 42u8 }).await;
        assert_eq!(result, Some(42));
    }
}
