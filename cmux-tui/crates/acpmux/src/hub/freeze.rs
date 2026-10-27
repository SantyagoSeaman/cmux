//! Part of `Hub`; see `hub/mod.rs`. The harness of a chat nobody sees and
//! nobody uses stops (SIGSTOP) until something is sent to it (p14-energy-117).
//!
//! Chrome freezes a page that has been hidden and quiet for a while
//! (components/performance_manager/freezing/freezing_policy.cc), never one
//! that plays audio, captures or holds form input (`CannotFreezeReason`), and
//! freezes sooner in battery saver (`ToggleFreezingOnBatterySaverMode`).
//! acpmux does the same for agent harnesses: a session that is attached but
//! that no attached client shows (`_acpmux/view`, `_acpmux/visibility`;
//! `seen.rs` counts them), with no turn, queued prompt, pending permission or
//! question, start, background process or waiting continuation, gets its
//! harness's process group stopped after `freezeAfterSeconds` of no use
//! (300; 30 under Energy Saver). Its harness then costs no CPU and no wakeups.
//!
//! The continue (SIGCONT) is the write path's: `ChildAgent::write` passes the
//! harness's [`HarnessGate`] before any byte, so a prompt, a permission
//! answer, a cancel or any other request continues the harness first,
//! whichever path sends it. Showing the chat continues it on the same turn.
//! Ending or detaching a harness continues it first, so no harness outlives
//! its controller stopped, and adopting a host continues its harness in case
//! the daemon that stopped it crashed.
//!
//! The freeze runs on the idle reaper's task (`idle.rs`) at its deadline: no
//! timer runs for a session that shows or is in use. Behind the
//! `hiddenHarnessFreeze` feature (default off).

use super::*;
use std::time::Duration;

/// Stops and continues a harness's process group; tests record instead.
pub trait ProcessControl: Send + Sync {
    /// Stops every process of `group`; false when nothing was stopped.
    fn stop(&self, group: u32) -> bool;
    /// Continues every process of `group`.
    fn resume(&self, group: u32);
}

/// [`ProcessControl`] with `killpg`; Windows stops nothing.
pub struct SignalControl;

#[cfg(unix)]
fn signal_group(group: u32, signal: i32) -> bool {
    // Never 0 (our own group) or 1 (init): only a harness that leads its own group.
    let Ok(group) = i32::try_from(group) else { return false };
    if group <= 1 {
        return false;
    }
    // SAFETY: killpg takes no pointers; `group` is a harness that leads its own
    // process group (process_group(0) where it was spawned).
    unsafe { libc::killpg(group, signal) == 0 }
}

impl ProcessControl for SignalControl {
    fn stop(&self, group: u32) -> bool {
        #[cfg(unix)]
        return signal_group(group, libc::SIGSTOP);
        #[cfg(not(unix))]
        {
            let _ = group;
            false
        }
    }

    fn resume(&self, group: u32) {
        #[cfg(unix)]
        signal_group(group, libc::SIGCONT);
        #[cfg(not(unix))]
        let _ = group;
    }
}

/// One harness's freeze state, passed by every write to it. The lock is held
/// only for a signal (no I/O, no await): a stop can never land between a
/// write's continue and its bytes.
#[derive(Default)]
pub struct HarnessGate {
    state: StdMutex<GateState>,
}

#[derive(Default)]
struct GateState {
    /// Set while stopped: who continues it, and its group.
    frozen: Option<(Arc<dyn ProcessControl>, u32)>,
    /// Writes in progress.
    writing: usize,
    /// Writes begun, and that count when the harness was last seen quiet.
    writes: u64,
    quiet_at: u64,
}

/// A write in progress (`HarnessGate::writing`): no stop until it drops.
pub struct Writing<'a>(&'a HarnessGate);

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.0.lock().writing -= 1;
    }
}

impl HarnessGate {
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Before bytes go to the harness: continues it when stopped. Until the
    /// guard drops the harness is not stopped again.
    pub fn writing(&self) -> Writing<'_> {
        let mut state = self.lock();
        if let Some((control, group)) = state.frozen.take() {
            control.resume(group);
        }
        state.writing += 1;
        state.writes += 1;
        Writing(self)
    }

    /// Continues a stopped harness; true when it was stopped.
    pub fn thaw(&self) -> bool {
        let frozen = self.lock().frozen.take();
        frozen.map(|(control, group)| control.resume(group)).is_some()
    }

    pub fn is_frozen(&self) -> bool {
        self.lock().frozen.is_some()
    }

    /// The harness counts as quiet from now: only a write after this keeps
    /// the next [`HarnessGate::freeze`] from stopping it.
    pub fn mark_quiet(&self) {
        let mut state = self.lock();
        state.quiet_at = state.writes;
    }

    /// Stops `group` when nothing was written since the last
    /// [`HarnessGate::mark_quiet`] and no write runs; otherwise marks it quiet
    /// from now and stops nothing. True when it is stopped now.
    pub fn freeze(&self, control: Arc<dyn ProcessControl>, group: u32) -> bool {
        let mut state = self.lock();
        if state.frozen.is_some() {
            return true;
        }
        if state.writing > 0 || state.writes != state.quiet_at {
            state.quiet_at = state.writes;
            return false;
        }
        if !control.stop(group) {
            return false;
        }
        state.frozen = Some((control, group));
        true
    }
}

impl ChildAgent {
    /// The process group the harness leads (its pid: a direct child and a
    /// host's harness both start in their own group). None on Windows, where
    /// nothing is stopped, or before a host reported it.
    pub fn harness_group(&self) -> Option<u32> {
        if cfg!(windows) {
            return None;
        }
        self.pid.or_else(|| self.host_record().and_then(|r| r.harness_pid))
    }
}

/// A host this daemon connects to: its harness may be one a crashed daemon
/// stopped, so it continues (a running one is unaffected).
pub(crate) fn resume_adopted(child: &ChildAgent) {
    if let Some(group) = child.harness_group() {
        SignalControl.resume(group);
    }
}

/// `freezeAfterSeconds` (`seconds`), or its 300 s default, at most 30 s
/// under Energy Saver (`saver`); None when it is 0 (off).
pub(crate) fn freeze_after(seconds: Option<u64>, saver: bool) -> Option<Duration> {
    match seconds.unwrap_or(300) {
        0 => None,
        secs if saver => Some(Duration::from_secs(secs.min(30))),
        secs => Some(Duration::from_secs(secs)),
    }
}

/// The hub's side of the freeze.
pub(crate) struct FreezeState {
    /// The `hiddenHarnessFreeze` feature (`daemon_lifecycle.rs`), or a test.
    on: AtomicBool,
    control: StdMutex<Arc<dyn ProcessControl>>,
    /// The gates this hub stopped, by session: what showing a chat continues
    /// without its child lock, and what `frozen` reports. A gate that a write
    /// continued stays until the next show or count; it reads not frozen.
    frozen: StdMutex<HashMap<String, Arc<HarnessGate>>>,
    /// Energy Saver edges, for the hub's life (`watch_energy`).
    energy: std::sync::OnceLock<cmux_wake::EnergyWatch>,
}

impl Default for FreezeState {
    fn default() -> Self {
        Self {
            on: AtomicBool::new(false),
            control: StdMutex::new(Arc::new(SignalControl)),
            frozen: StdMutex::new(HashMap::new()),
            energy: std::sync::OnceLock::new(),
        }
    }
}

fn locked<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Hub {
    /// Turns the hidden harness freeze on or off (the `hiddenHarnessFreeze`
    /// feature, `daemon_lifecycle.rs`); off continues nothing.
    pub fn set_harness_freeze(&self, on: bool) {
        self.freeze.on.store(on, Ordering::SeqCst);
        self.idle_wake.notify_one();
    }

    /// Stops and continues harnesses through `control` (tests).
    pub fn set_process_control(&self, control: Arc<dyn ProcessControl>) {
        *locked(&self.freeze.control) = control;
    }

    fn freeze_control(&self) -> Arc<dyn ProcessControl> {
        locked(&self.freeze.control).clone()
    }

    /// How long a hidden session goes unused before its harness stops, under
    /// the current Energy Saver mode; None while the freeze is off.
    pub(super) fn freeze_after(&self, config: &Config) -> Option<Duration> {
        if !self.freeze.on.load(Ordering::SeqCst) || self.stopping.load(Ordering::SeqCst) {
            return None;
        }
        freeze_after(config.freeze_after_seconds, self.maintenance.energy().is_saver())
    }

    /// Attached, and no attached client shows it.
    fn hidden_attached(&self, session: &Session) -> bool {
        session.attached.load(Ordering::SeqCst) > 0 && !self.shown(session)
    }

    /// After every attach, detach and visibility change (`seen.rs`): a shown
    /// chat's harness continues on this turn; a hidden one's quiet period
    /// starts now.
    pub(super) fn visibility_changed(&self, session: &Session) {
        if self.shown(session) {
            self.thaw_session(session);
        } else if self.hidden_attached(session) {
            self.touch(session);
            if let Ok(child) = session.child.try_lock()
                && let Some(child) = child.as_ref()
            {
                child.gate.mark_quiet();
            }
            self.idle_wake.notify_one();
        }
    }

    /// Continues the session's stopped harness; true when it was stopped.
    pub(super) fn thaw_session(&self, session: &Session) -> bool {
        let gate = locked(&self.freeze.frozen).remove(&session.id);
        gate.is_some_and(|g| g.thaw())
    }

    /// Whether the session's harness is stopped now (the summary's `frozen`).
    pub(super) fn is_frozen(&self, session: &Session) -> bool {
        locked(&self.freeze.frozen).get(&session.id).is_some_and(|g| g.is_frozen())
    }

    /// Stopped harnesses now (`_acpmux/status` `frozen`).
    pub(super) fn frozen_count(&self) -> usize {
        let mut frozen = locked(&self.freeze.frozen);
        frozen.retain(|_, g| g.is_frozen());
        frozen.len()
    }

    /// The child a freeze pass may stop: hidden and attached, with a harness
    /// group, not stopped. A child lock held now counts as a child.
    fn freeze_candidate(&self, session: &Session) -> Option<Option<Arc<ChildAgent>>> {
        if !self.hidden_attached(session) {
            return None;
        }
        match session.child.try_lock() {
            Ok(child) => {
                let child = child.as_ref()?;
                (!child.gate.is_frozen() && child.harness_group().is_some())
                    .then(|| Some(child.clone()))
            }
            Err(_) => Some(None),
        }
    }

    /// When the first candidate's quiet period ends, on the hub clock.
    pub(super) fn next_freeze_deadline(&self, after: Duration) -> Option<Duration> {
        self.sessions()
            .iter()
            .filter(|s| self.freeze_candidate(s).is_some())
            .map(|s| Duration::from_nanos(s.used_at.load(Ordering::Relaxed)) + after)
            .min()
    }

    /// Nothing runs or waits in the session's harness (`idle.rs` shares it).
    pub(super) fn harness_quiet(&self, session: &Session) -> bool {
        !self.stopping.load(Ordering::SeqCst)
            && matches!(session.status(), SessionStatus::Ready | SessionStatus::Idle)
            && session.turn().is_none()
            && session.turn_lock.try_lock().is_ok()
            && session.queued() == 0
            && session.pending_permissions().is_empty()
            && session.spawn_lock.try_lock().is_ok()
            // A process still running in the background (a dev server, a
            // watcher) is in the harness's group: never stop or end it.
            && !self.any_process_running(session)
    }

    /// Stops the harness of every candidate whose quiet period ended and that
    /// is still unused; one in use starts a full period again.
    pub(super) async fn freeze_hidden_children(&self, after: Duration) {
        let _pass = self.idle_pass.lock().await;
        if self.stopping.load(Ordering::SeqCst) {
            return;
        }
        let now = self.clock_now();
        let after = after.as_nanos() as u64;
        for session in self.sessions() {
            let Some(child) = self.freeze_candidate(&session) else { continue };
            if session.used_at.load(Ordering::Relaxed).saturating_add(after) > now {
                continue;
            }
            let frozen = child.as_ref().is_some_and(|child| {
                let quiet = self.harness_quiet(&session)
                    && !session.closing.load(Ordering::SeqCst)
                    && !self.continuation_pending(&session)
                    && !child.has_exited();
                match (quiet, child.harness_group()) {
                    (true, Some(group)) => child.gate.freeze(self.freeze_control(), group),
                    _ => {
                        child.gate.mark_quiet();
                        false
                    }
                }
            });
            match child {
                Some(child) if frozen => {
                    tracing::info!(session = %session.id, "hidden idle harness stopped; it continues before anything is sent to it");
                    locked(&self.freeze.frozen).insert(session.id.clone(), child.gate.clone());
                }
                // In use, or written to since it was last quiet: a full period starts now.
                _ => self.touch(&session),
            }
        }
    }

    /// Energy Saver edges: the freeze deadline follows the mode, and the pool
    /// parks its ready entries when the saver turns on (`pool/`).
    pub(super) fn watch_energy(self: &Arc<Self>) {
        let edge = Arc::new(Notify::new());
        let notify = edge.clone();
        let watch = self.maintenance.energy().watch(move || notify.notify_one());
        if self.freeze.energy.set(watch).is_err() {
            return;
        }
        let hub = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                edge.notified().await;
                let Some(hub) = hub.upgrade() else { return };
                hub.idle_wake.notify_one();
                hub.pool_saver_edge().await;
            }
        });
    }
}

// The stand-in harness is `cat`, and Windows stops nothing.
#[cfg(all(test, unix))]
#[path = "freeze_tests.rs"]
mod freeze_tests;
