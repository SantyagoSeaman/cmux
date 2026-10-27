//! `_acpmux/visibility {sessionId, visible}` (p14-energy-117): whether the
//! pane that holds an attached chat shows, sent by the app on each hidden or
//! shown edge of the pane (its workspace, its window). The chat's page says
//! the same through `_acpmux/view` only while it can still run script; the
//! pane's own word does not depend on it. The attachment shows the chat when
//! neither says hidden (`SubOpts::shows`), and that count is what a turn's
//! unread mark and the hidden idle harness freeze read (`hub/seen.rs`,
//! `hub/freeze.rs`). Showing continues a stopped harness before the answer.

use super::*;

/// `_acpmux/visibility`: errors when this connection is not attached to the
/// session, as `_acpmux/view` does.
pub(super) fn set(hub: &Hub, conn: &Conn, params: &Value) -> Result<Value, RpcError> {
    view::change(hub, conn, params, method::MUX_VISIBILITY, |sub, visible| {
        sub.pane_hidden = !visible;
    })
}
