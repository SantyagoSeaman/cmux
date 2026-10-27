//! Part of `Hub`; see `hub/mod.rs`. An idle session hibernates: its harness exits.
//!
//! Every new session used to keep its adapter alive until the daemon
//! stopped: after about 25 sessions, 36 adapters, 187 processes and 9.4 GB,
//! with switches slowing as they piled up. A session is idle when no turn
//! runs or waits (Claude Code's own turns included), nothing is queued, no
//! permission or question is pending, no start holds its spawn lock, no
//! background command or subagent runs, and nothing was logged for
//! `idle_child` (2 min, `IDLE_CHILD`; `ACPMUX_IDLE_CHILD_SECS`). A client
//! attached to it, the open chat on screen included, does not keep it awake:
//! the chat stays where it is and only the agent goes. Its harness is then
//! stopped through `detach_child`, which ends the harness's process group and
//! with it the MCP servers it started: the record and the agent session id
//! stay, so the next prompt resumes it (`session/load`, `claude --resume`).
//!
//! A wake is the next start of a hibernated session's agent: a prompt, a
//! person typing into the chat (`_acpmux/typing`, `wake_for_typing`, so the
//! agent is up by the time they send), or any request that needs the agent.
//! Every hibernation and every wake is logged with its cause and duration.
//! A wake that fails fails its prompt like any start that fails (its
//! `turn_error`, which the chat shows with Retry).
//!
//! The same task ends a running turn whose model has said nothing for
//! `stalledTurnEndMs` (`stalled_turn.rs`).
//!
//! One task per hub waits on the injected clock for the earliest deadline
//! and on a wake signal for a new child; it never polls, so idle costs no
//! CPU. Activity only moves deadlines later, so the task wakes at most once
//! per idle period per live child. It sleeps without a deadline when no
//! session has a live child, and ends with the hub (its weak reference) or
//! for good once shutdown starts (`stop_idle_reaper`).

use super::*;

impl Hub {
    /// The hub clock's time, as stored in `Session::last_active`.
    pub(super) fn clock_now(&self) -> u64 {
        self.clock.lock().unwrap().now().as_nanos() as u64
    }

    /// Something happened in `session`: its idle period starts again.
    pub(super) fn touch(&self, session: &Session) {
        session.last_active.store(self.clock_now(), Ordering::Relaxed);
    }

    /// Unused right now, read while the caller holds its `turn_lock` and `spawn_lock` (so no
    /// prompt and no start begins until the harness is gone). A turn or permission an adopted
    /// host's recovery rebuilt (`hosts.rs` `recover_work`) counts like a live one: it sets
    /// `session.turn` (and holds `turn_lock` until the answer), registers the permission as
    /// pending, and moves the status off ready.
    fn idle_eligible(&self, session: &Session) -> bool {
        !self.stopping.load(Ordering::SeqCst)
            && matches!(session.status(), SessionStatus::Ready | SessionStatus::Idle)
            && session.turn().is_none()
            // Nor a turn Claude Code started itself (a task notification's), which `turn` does
            // not show.
            && !session.harness_turn.load(Ordering::SeqCst)
            && session.queued() == 0
            && session.pending_permissions().is_empty()
            // A process still running in the background (a dev server, a
            // watcher) ends with the harness: never reap it for idleness.
            && !self.any_process_running(session)
            // Nor a background subagent, which the harness's exit would end too.
            && session.subagents.lock().unwrap_or_else(|e| e.into_inner()).background_running() == 0
    }

    /// The daemon is stopping (`_acpmux/shutdown`, SIGTERM, then
    /// `shutdown_all`): the idle reaper never stops a harness again.
    pub fn stop_idle_reaper(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// A child was published: make sure the reaper runs and sees it.
    pub(super) fn wake_idle_reaper(self: &Arc<Self>) {
        if !self.idle_reaper.swap(true, Ordering::SeqCst) {
            let weak = Arc::downgrade(self);
            let wake = self.idle_wake.clone();
            tokio::spawn(async move { Self::idle_reaper_loop(weak, wake).await });
        }
        self.idle_wake.notify_one();
    }

    async fn idle_reaper_loop(weak: std::sync::Weak<Self>, wake: Arc<Notify>) {
        loop {
            let (next, clock) = {
                let Some(hub) = weak.upgrade() else { return };
                let clock = hub.clock.lock().unwrap().clone();
                // The same task ends stalled turns (`stalled_turn.rs`).
                let stalled = hub.config.read().await.stalled_turn_end();
                let stalled = stalled.and_then(|s| hub.next_stalled_deadline(s));
                let next = match (hub.next_idle_deadline(), stalled) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                (next, clock)
            };
            match next {
                Some(at) => {
                    tokio::select! {
                        _ = clock.sleep_until(at) => {}
                        _ = wake.notified() => {}
                    }
                }
                None => wake.notified().await,
            }
            let Some(hub) = weak.upgrade() else { return };
            if let Some(stalled) = hub.config.read().await.stalled_turn_end() {
                hub.settle_stalled_turns(stalled);
            }
            hub.stop_idle_children().await;
        }
    }

    /// When the first live child's idle period ends, on the hub clock.
    fn next_idle_deadline(&self) -> Option<std::time::Duration> {
        let idle = (*self.idle_child.lock().unwrap())?;
        self.sessions()
            .iter()
            .filter(|s| s.child.try_lock().map(|c| c.is_some()).unwrap_or(true))
            .map(|s| std::time::Duration::from_nanos(s.last_active.load(Ordering::Relaxed)) + idle)
            .min()
    }

    async fn stop_idle_children(&self) {
        // Shutdown owns every child from its first step: hosted ones are
        // handed off or ended there, never terminated here meanwhile.
        let _pass = self.idle_pass.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let Some(idle) = *self.idle_child.lock().unwrap() else { return };
        let now = self.clock_now();
        let idle = idle.as_nanos() as u64;
        for session in self.sessions() {
            if session.last_active.load(Ordering::Relaxed).saturating_add(idle) > now {
                continue;
            }
            let has_child = session.child.try_lock().map(|c| c.is_some()).unwrap_or(true);
            if !has_child {
                continue;
            }
            // Held until the harness is gone: a prompt (the queue takes `turn_lock` first) or a
            // start that comes meanwhile waits, then starts the agent again, never one that is
            // being ended. Either held: in use.
            let held = (session.turn_lock.try_lock(), session.spawn_lock.try_lock());
            let (Ok(_turn), Ok(_spawn)) = held else {
                self.touch(&session);
                continue;
            };
            if self.idle_eligible(&session) {
                let idle_for = now.saturating_sub(session.last_active.load(Ordering::Relaxed));
                let began = std::time::Instant::now();
                // A hosted child whose link is closed cannot take a Terminate
                // frame: end its host with the nonce proof, as closing does.
                let child = session.child.lock().await.clone();
                let link_lost = match &child {
                    Some(c) => c.host_record().is_some() && !c.is_alive().await,
                    None => false,
                };
                // Marked before the harness ends: typing or a click while it ends wakes it
                // again, its start waiting for the spawn lock this pass holds.
                session.hibernated_at.store(self.clock_now().max(1), Ordering::SeqCst);
                self.detach_child(&session).await;
                if link_lost {
                    self.end_unadopted_host(&session).await;
                }
                tracing::info!(
                    session = %session.id,
                    cause = "idle",
                    idle_secs = idle_for / 1_000_000_000,
                    exit_ms = began.elapsed().as_millis() as u64,
                    "hibernated: no turn, nothing queued, no question pending, no command running; \
                     the harness and its MCP servers exited and the session wakes on its next prompt"
                );
            } else {
                // In use at its deadline: a full idle period starts now.
                self.touch(&session);
            }
        }
    }

    /// What asks for a hibernated session's agent: told to the wake's log (`wake_began`).
    /// Only the first cause counts until the wake is done.
    pub(super) fn wake_cause(&self, session: &Session, cause: &'static str) {
        if session.hibernated_at.load(Ordering::SeqCst) != 0 {
            session.wake_cause.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(cause);
        }
    }

    /// A start of `session`'s agent begins (`ensure_child`): a wake when the idle exit hibernated
    /// it, timed from here.
    pub(super) fn wake_began(&self, session: &Session) -> Option<std::time::Instant> {
        (session.hibernated_at.load(Ordering::SeqCst) != 0).then(std::time::Instant::now)
    }

    /// The start `wake_began` timed ended: logs the wake with its cause and duration. A failed
    /// one stays hibernated, so the next try is logged as a wake again.
    pub(super) fn wake_ended(
        &self,
        session: &Session,
        began: Option<std::time::Instant>,
        failed: Option<&RpcError>,
    ) {
        let Some(began) = began else { return };
        let took_ms = began.elapsed().as_millis() as u64;
        let at = session.hibernated_at.load(Ordering::SeqCst);
        let slept_secs = self.clock_now().saturating_sub(at) / 1_000_000_000;
        let cause = session.wake_cause.lock().unwrap_or_else(|e| e.into_inner()).take();
        let cause = cause.unwrap_or("request");
        match failed {
            None => {
                session.hibernated_at.store(0, Ordering::SeqCst);
                tracing::info!(session = %session.id, cause, took_ms, slept_secs, "woke");
            }
            Some(e) => {
                tracing::warn!(session = %session.id, cause, took_ms, slept_secs, "wake failed: {}", e.message)
            }
        }
    }

    /// A person typed into `session` (`_acpmux/typing` on, sent again every 5 s while typing goes
    /// on, cause `typing`) or opened it on screen (`_acpmux/attach` or `_acpmux/view` visible,
    /// cause `click`): its idle period starts again, and a hibernated session wakes now, so its
    /// agent is up by the time they send. Nothing for a session that was not hibernated, is
    /// closed, is watched (no agent until Take Over, `watch.rs`), or whose folder has no Trust
    /// answer (`warm.rs`). The wake runs on its own task under the stop count read here, so a
    /// Stop that comes first is never undone by it (`ensure_child`). A failure is logged and the
    /// prompt that follows starts the agent itself (and shows its error, with Retry, if that
    /// fails too).
    pub fn wake_for(self: &Arc<Self>, session: &Arc<Session>, web: bool, cause: &'static str) {
        // Use: a chat being typed into or opened does not hibernate meanwhile.
        self.touch(session);
        let stops = session.stops.load(Ordering::SeqCst);
        if session.hibernated_at.load(Ordering::SeqCst) == 0
            || session.status() == SessionStatus::Closed
            || session.meta().watching.is_some()
            || self.stopping.load(Ordering::SeqCst)
        {
            return;
        }
        let hub = self.clone();
        let session = session.clone();
        tokio::spawn(async move {
            if !hub.folder_trusted(&session, web).await
                || session.stops.load(Ordering::SeqCst) != stops
                || session.status() == SessionStatus::Closed
            {
                return;
            }
            hub.wake_cause(&session, cause);
            let _ = hub.child_for_since(&session, stops).await;
        });
    }
}
