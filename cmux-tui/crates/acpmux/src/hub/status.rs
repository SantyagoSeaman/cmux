//! A session's status: the one setter, the parent's slot and record that follow
//! it, and the variant for a caller that holds the session's `permissions`.

use super::permission_groups::PermissionState;
use super::*;

impl Hub {
    pub(super) fn set_status(&self, session: &Session, status: SessionStatus) {
        let Some((was, link)) = self.write_status(session, status) else { return };
        if let Some(link) = &link {
            self.subsession_status(&session.id, link, was, status);
        }
        self.status_changed(session, status);
    }

    /// [`Self::set_status`] under the session's `permissions`, which an ask, its answer and a
    /// turn's end hold so the status follows the pending asks. The guard is released before a
    /// sub-session's closed or reopened record for its parent: that looks the parent up in
    /// `Hub::sessions`, which ranks before every session lock (`crate::lock_rank`).
    pub(super) fn set_status_releasing(
        &self,
        session: &Session,
        status: SessionStatus,
        permissions: RankedGuard<'_, PermissionState>,
    ) {
        let changed = self.write_status(session, status);
        if changed.is_some() {
            self.status_changed(session, status);
        }
        drop(permissions);
        if let Some((was, Some(link))) = changed {
            self.subsession_status(&session.id, &link, was, status);
        }
    }

    /// Writes the status; answers the one it replaced and the sub-session link when it changed.
    fn write_status(
        &self,
        session: &Session,
        status: SessionStatus,
    ) -> Option<(SessionStatus, Option<crate::store::SubsessionLink>)> {
        let mut m = session.meta.lock().unwrap();
        let was = m.status;
        m.status = status;
        m.updated_at = now_ms();
        // The parent's slot follows the status under the same lock, so
        // concurrent status writes and the index never disagree (the
        // index is sub-state, `subsessions.rs`).
        if let Some(link) = &m.subsession {
            self.subsessions.set_open(
                &link.started_by,
                &session.id,
                status != SessionStatus::Closed,
            );
        }
        (was != status).then(|| (was, m.subsession.clone()))
    }

    /// What a status change does past the meta write and the parent's record.
    fn status_changed(&self, session: &Session, status: SessionStatus) {
        session.mark_waiting(status == SessionStatus::Waiting);
        if status == SessionStatus::Closed {
            session.watch_end.notify_one();
        }
        self.append(session, "mux", "status", json!({"status": status.to_string()}));
        self.save_meta(session);
        self.care_status(status);
    }
}
