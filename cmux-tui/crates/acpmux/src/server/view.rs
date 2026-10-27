//! A connection's attachments and whether it shows each chat (cx-n8ou,
//! `hub/seen.rs`): `_acpmux/attach` with `visible: false` holds a chat
//! without showing it, `_acpmux/view` changes that, and detaching or
//! disconnecting lets go with the same visibility it held. `_acpmux/visibility`
//! (`visibility.rs`) says whether the pane holding the chat shows; the
//! attachment shows the chat only when both say so.

use super::*;

/// How one connection receives one attached session's live records.
#[derive(Debug, Clone, Default)]
pub struct SubOpts {
    /// Every record arrives as `_acpmux/event` (agent notifications nested
    /// in `msg`) instead of `session/update` plus `_acpmux/event`.
    pub event_stream: bool,
    /// With `event_stream`, only records this filter passes are sent.
    pub filter: EventFilter,
    /// Attached without showing the chat (`_acpmux/view`).
    pub hidden: bool,
    /// The pane holding the chat is hidden (`_acpmux/visibility`, `visibility.rs`).
    pub pane_hidden: bool,
}

impl SubOpts {
    /// This attachment shows the chat: neither the chat nor its pane is hidden.
    pub fn shows(&self) -> bool {
        !self.hidden && !self.pane_hidden
    }
}

/// Subscribes for `_acpmux/attach`, counting a new attachment and a changed
/// visibility of an existing one.
pub(super) fn attach(hub: &Hub, conn: &Conn, s: &crate::hub::Session, mut opts: SubOpts) {
    conn.bind_mailbox_receipt(crate::session_identity::SessionReceipt::new(
        crate::session_identity::LOCAL_HOST_ID,
        s.id.clone(),
        s.meta().agent_session_id,
        hub.started_at,
    ));
    let mut subs = conn.subs.lock().unwrap_or_else(|e| e.into_inner());
    // Attaching again keeps what the pane said (`_acpmux/visibility`).
    opts.pane_hidden = subs.get(&s.id).is_some_and(|before| before.pane_hidden);
    let shows = opts.shows();
    match subs.insert(s.id.clone(), opts) {
        None => hub.attach_count(s, 1, shows),
        Some(before) if before.shows() != shows => hub.set_visible(s, shows),
        Some(_) => {}
    }
}

/// `_acpmux/detach`.
pub(super) fn detach(hub: &Hub, conn: &Conn, params: &Value) -> Result<Value, RpcError> {
    let s = hub.resolve(session_key(params)?)?;
    if let Some(before) = conn.subs.lock().unwrap_or_else(|e| e.into_inner()).remove(&s.id) {
        conn.unbind_mailbox_receipt(&s.id);
        hub.attach_count(&s, -1, before.shows());
    }
    Ok(json!({}))
}

/// `_acpmux/view {sessionId, visible}`: whether this attached client shows the chat.
pub(super) fn view(hub: &Hub, conn: &Conn, params: &Value) -> Result<Value, RpcError> {
    change(hub, conn, params, method::MUX_VIEW, |sub, visible| sub.hidden = !visible)
}

/// Sets one of an attachment's visibility flags from `params` `visible`
/// with `set`, and counts the attachment's change, if any (`hub/seen.rs`).
pub(super) fn change(
    hub: &Hub,
    conn: &Conn,
    params: &Value,
    method: &str,
    set: impl FnOnce(&mut SubOpts, bool),
) -> Result<Value, RpcError> {
    let s = hub.resolve(session_key(params)?)?;
    let visible = params
        .get("visible")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("visible must be true or false"))?;
    let mut subs = conn.subs.lock().unwrap_or_else(|e| e.into_inner());
    let sub = subs.get_mut(&s.id).ok_or_else(|| {
        RpcError::invalid_params(format!("attach to the session before {method}"))
    })?;
    let before = sub.shows();
    set(sub, visible);
    if sub.shows() != before {
        // Under the lock, so a detach racing this one counts the same visibility.
        hub.set_visible(&s, sub.shows());
    }
    Ok(json!({}))
}

/// `_acpmux/markUnread {sessionId}` (cx-ncc.78): the person marks a chat unread; it stays so until
/// a client shows it again after hiding it (`hub/seen.rs`).
pub(super) fn mark_unread(hub: &Hub, params: &Value) -> Result<Value, RpcError> {
    hub.mark_unread(&*hub.resolve(session_key(params)?)?);
    Ok(json!({}))
}

/// The connection closed: every attachment it held ends with it.
pub(super) fn release_all(hub: &Hub, conn: &Conn) {
    let subs: Vec<(String, SubOpts)> =
        conn.subs.lock().unwrap_or_else(|e| e.into_inner()).drain().collect();
    for (id, opts) in subs {
        conn.unbind_mailbox_receipt(&id);
        if let Ok(s) = hub.resolve(&id) {
            hub.attach_count(&s, -1, opts.shows());
        }
    }
}
