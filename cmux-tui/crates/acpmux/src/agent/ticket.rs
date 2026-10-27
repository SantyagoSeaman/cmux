//! The ticket a tapped message's record is known by (`agent.rs` `Tap`).

/// When a tapped message's record, and every record of its session before it, is on disk: the
/// session's last record when the tap returned, and the store's progress for that session
/// (`store/log_writer.rs`; None for a store that writes before it returns).
pub struct WriteTicket {
    seq: u64,
    progress: Option<tokio::sync::watch::Receiver<crate::store::Progress>>,
}

impl WriteTicket {
    pub fn new(
        seq: u64,
        progress: Option<tokio::sync::watch::Receiver<crate::store::Progress>>,
    ) -> Self {
        Self { seq, progress }
    }

    /// A message that is stored as soon as it is handed over (a test's or a probe's tap).
    pub fn done() -> Self {
        Self { seq: 0, progress: None }
    }

    /// The ticket that covers both: the one that waits on the store for the later record.
    pub(super) fn later(self, other: Self) -> Self {
        match (&self.progress, &other.progress) {
            (None, _) => other,
            (Some(_), None) => self,
            (Some(_), Some(_)) if other.seq >= self.seq => other,
            _ => self,
        }
    }

    /// Waits until the ticket's records are on disk. False when its log failed, or the store
    /// ended first: the entry is then never acknowledged, and the host keeps it.
    pub async fn written(self) -> bool {
        let Some(mut rx) = self.progress else { return true };
        loop {
            let p = *rx.borrow_and_update();
            if p.failed {
                return false;
            }
            if p.covers(self.seq) {
                return true;
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }
}
