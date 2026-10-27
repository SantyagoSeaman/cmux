//! The table of app children and what happens to one: spawn, output, the
//! ready line, reads, stops and the exit. Its one lock is a leaf: no reply
//! is sent, no signal is sent and nothing is posted while it is held.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ChildStdin, ExitStatus};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use cmux_process::{Command, Stdio};
use cmux_threads::sync::{Condvar, Mutex};
use cmux_threads::work::{Lane, Priority, Shutdown};
use serde_json::{Value, json};

use crate::panic_free::JsonFields;

use super::output::{self, OutputSink, Watch};
use crate::mux::Mux;

/// Children that run at once.
pub(crate) const MAX_RUNNING: usize = 32;
/// Children kept in the table, finished ones included; past it the oldest
/// finished ones go first.
pub(crate) const MAX_KEPT: usize = 96;
/// Output a child keeps: its last 1 MiB.
pub(crate) const MAX_BUFFER: usize = 1 << 20;
/// Output one read answers.
pub(crate) const MAX_READ: usize = 256 << 10;
/// The longest a read waits for news.
pub(crate) const MAX_READ_WAIT: Duration = Duration::from_secs(10);
/// Reads parked on one child at once.
const MAX_WAITERS: usize = 16;
/// The longest a start waits for its ready line.
pub(crate) const MAX_READY_WAIT: Duration = Duration::from_secs(120);
/// A ready line longer than this is cut; the scan keeps only this tail of a
/// line that has no end yet.
const MAX_LINE: usize = 16 * 1024;
/// Interrupt: SIGINT to the group, then SIGTERM and SIGKILL after this each.
pub(crate) const INTERRUPT_ESCALATION: Duration = Duration::from_secs(2);
/// Terminate: SIGTERM to the group (and the lifeline closed), SIGKILL after this.
pub(crate) const TERMINATE_GRACE: Duration = Duration::from_secs(5);
/// After the child exits, how long its pipe may still drain; a background
/// process that keeps the pipe open does not hold the output open past it.
pub(crate) const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(250);
/// The daemon's end: how long its children get after SIGTERM before the
/// groups still running are killed (Chrome's
/// `ChildProcessLauncherHelper::ForceNormalProcessTerminationSync` waits the
/// same way before its SIGKILL, content/browser/
/// child_process_launcher_helper_posix.cc).
pub(crate) const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);

/// How a child ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Exit {
    pub(crate) code: Option<i32>,
    pub(crate) signal: Option<i32>,
}

impl Exit {
    fn of(status: &io::Result<ExitStatus>) -> Self {
        match status {
            Ok(status) => Self { code: status.code(), signal: status.signal() },
            // Reaped elsewhere: the status is unknown.
            Err(_) => Self::default(),
        }
    }

    pub(crate) fn json(self) -> Value {
        let mut value = json!({});
        if let Some(code) = self.code {
            value.insert("code", json!(code));
        }
        if let Some(signal) = self.signal {
            value.insert("signal", json!(signal));
        }
        value
    }
}

/// One `app-child-read` answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chunk {
    pub(crate) output: String,
    pub(crate) next: u64,
    pub(crate) truncated: bool,
    pub(crate) exit: Option<Exit>,
}

impl Chunk {
    pub(crate) fn json(&self) -> Value {
        let mut value = json!({"output": self.output, "next": self.next});
        if self.truncated {
            value.insert("truncated", json!(true));
        }
        if let Some(exit) = self.exit {
            value.insert("exit", exit.json());
        }
        value
    }
}

/// Why a request about a child failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChildError {
    Limit,
    FolderMissing,
    Spawn(String),
    /// The child ended, or missed its deadline, before its ready line.
    NotReady(String),
    Unknown,
    ShuttingDown,
}

impl ChildError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Limit => "app-child.limit",
            Self::FolderMissing => "app-child.folder-missing",
            Self::Spawn(_) => "app-child.spawn-failed",
            Self::NotReady(_) => "app-child.not-ready",
            Self::Unknown => "app-child.unknown",
            Self::ShuttingDown => "app-child.unavailable",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::Limit => "too many app children".into(),
            Self::FolderMissing => "the child's folder does not exist".into(),
            Self::Spawn(reason) => format!("the child could not start: {reason}"),
            Self::NotReady(reason) => format!("the child did not become ready: {reason}"),
            Self::Unknown => "no such app child".into(),
            Self::ShuttingDown => "the daemon is shutting down".into(),
        }
    }
}

/// What the child's stdin is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Input {
    /// /dev/null: a command that reads its input sees its end at once.
    Null,
    /// A pipe the daemon keeps open (the lifeline): the child sees its end
    /// when the child is stopped or the daemon ends.
    Lifeline,
}

/// What `app-child-start` starts.
pub(crate) struct Spec {
    pub(crate) name: String,
    pub(crate) owner: String,
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<String>,
    /// The whole environment (none of the daemon's is inherited).
    pub(crate) env: Vec<(String, String)>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) input: Input,
    /// Write a fresh 32-byte secret (64 hex characters) as the first line of
    /// the lifeline. Only with [`Input::Lifeline`].
    pub(crate) secret: bool,
    /// The start answers once an output line contains this.
    pub(crate) ready: Option<String>,
    pub(crate) ready_wait: Duration,
}

/// How long an `app-child-read` waits when there is no news.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadWait {
    /// Up to this long (at most [`MAX_READ_WAIT`]) for output or the end.
    For(Duration),
    /// Until the child finished, with no deadline: the app's exit watch.
    UntilExit,
}

/// How `app-child-stop` ends a child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopMode {
    /// SIGINT to the group as Ctrl-C does, then SIGTERM and SIGKILL.
    Interrupt,
    /// SIGHUP to the group as a closed terminal does.
    Hangup,
    /// The lifeline closes and SIGTERM goes to the group, then SIGKILL.
    Terminate,
}

/// A start's reply, sent once.
pub(crate) type StartReply = Box<dyn FnOnce(Result<Value, ChildError>) + Send>;
/// A read's reply, sent once.
pub(crate) type ReadReply = Box<dyn FnOnce(Chunk) + Send>;

struct Waiter {
    token: u64,
    /// The connection that parked it: its close drops the read.
    client: u64,
    after: u64,
    /// Answer only once the child finished (no deadline).
    until_exit: bool,
    reply: ReadReply,
}

struct Child {
    name: String,
    owner: String,
    pid: i32,
    secret: Option<String>,
    lifeline: Option<ChildStdin>,
    buffer: VecDeque<u8>,
    /// The absolute offset of `buffer`'s first byte (what was dropped before it).
    base: u64,
    exit: Option<Exit>,
    output_closed: bool,
    stopping: bool,
    watch: Option<Watch>,
    waiters: Vec<Waiter>,
    ready: Option<String>,
    /// The ready line, once an output line contained `ready`.
    ready_line: Option<String>,
    /// The unfinished last line while the ready line is awaited.
    partial: Vec<u8>,
    starting: Option<StartReply>,
}

impl Child {
    fn end(&self) -> u64 {
        self.base + self.buffer.len() as u64
    }

    fn finished(&self) -> bool {
        self.exit.is_some() && self.output_closed
    }

    /// News for a read from `after`: whole characters past it, or the end.
    /// A trailing partial character is not news while the child runs, so a
    /// child that stops after half a character parks its readers instead of
    /// answering each of them at once with nothing ([`Self::chunk`] holds
    /// those bytes back).
    fn has_news(&self, after: u64) -> bool {
        self.finished() || self.complete_end() > after
    }

    /// The offset after the buffer's last whole UTF-8 character.
    fn complete_end(&self) -> u64 {
        let tail = self.buffer.len().min(4);
        let last: Vec<u8> = self.buffer.range(self.buffer.len() - tail..).copied().collect();
        self.end() - (tail - utf8_complete(&last)) as u64
    }

    fn chunk(&self, after: u64) -> Chunk {
        let start = after.clamp(self.base, self.end());
        let skip = usize::try_from(start - self.base).unwrap_or(usize::MAX);
        let available = self.buffer.len().saturating_sub(skip);
        let take = available.min(MAX_READ);
        let bytes: Vec<u8> = self.buffer.iter().skip(skip).take(take).copied().collect();
        let complete = self.finished() && take == available;
        let keep = if complete { bytes.len() } else { utf8_complete(&bytes) };
        Chunk {
            output: String::from_utf8_lossy(&bytes[..keep]).into_owned(),
            next: start + keep as u64,
            truncated: after < self.base,
            exit: if complete { self.exit } else { None },
        }
    }

    /// The waiters with news, taken out to answer outside the lock.
    fn ready_waiters(&mut self) -> Vec<(ReadReply, Chunk)> {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.waiters.len() {
            let waiter = &self.waiters[index];
            let due = if waiter.until_exit { self.finished() } else { self.has_news(waiter.after) };
            if due {
                let waiter = self.waiters.swap_remove(index);
                ready.push((waiter.reply, self.chunk(waiter.after)));
            } else {
                index += 1;
            }
        }
        ready
    }

    /// Scans `bytes` for the ready line; true once it is found.
    fn scan_ready(&mut self, bytes: &[u8]) -> bool {
        let Some(ready) = self.ready.as_deref() else { return false };
        if self.ready_line.is_some() {
            return false;
        }
        for &byte in bytes {
            if byte != b'\n' {
                self.partial.push(byte);
                continue;
            }
            let line = String::from_utf8_lossy(&self.partial).trim_end_matches('\r').to_owned();
            self.partial.clear();
            if line.contains(ready) {
                self.ready_line = Some(line);
                self.partial = Vec::new();
                return true;
            }
        }
        if self.partial.len() > MAX_LINE {
            let cut = self.partial.len() - MAX_LINE;
            self.partial.drain(..cut);
        }
        false
    }

    fn started_json(&self, id: &str) -> Value {
        let mut value = json!({"child": id, "pid": self.pid});
        if let Some(secret) = &self.secret {
            value.insert("secret", json!(secret));
        }
        if let Some(line) = &self.ready_line {
            value.insert("ready_line", json!(line));
        }
        value
    }

    fn row(&self, id: &str) -> Value {
        let mut value = self.started_json(id);
        value.insert("name", json!(self.name));
        value.insert("owner", json!(self.owner));
        value.insert("running", json!(self.exit.is_none()));
        value
    }
}

#[derive(Default)]
struct State {
    children: HashMap<String, Child>,
    order: VecDeque<String>,
    spawning: usize,
    next_token: u64,
    /// Bumped by every grace start and by an app's return (app_children.rs).
    grace_generation: u64,
    /// A grace is armed and has not ended (app_children.rs).
    grace_armed: bool,
    /// The grace a test set; else `app_children::APP_GRACE`.
    #[cfg(test)]
    grace: Option<Duration>,
}

impl State {
    fn running(&self) -> usize {
        self.children.values().filter(|child| child.exit.is_none()).count() + self.spawning
    }

    /// Drops the oldest finished children past [`MAX_KEPT`]. A finished
    /// child has news for every read, so none of them holds a waiter.
    fn evict(&mut self) {
        while self.order.len() > MAX_KEPT {
            let Some(index) = self.order.iter().position(|id| {
                self.children
                    .get(id)
                    .is_some_and(|child| child.finished() && child.starting.is_none())
            }) else {
                return;
            };
            if let Some(id) = self.order.remove(index) {
                self.children.remove(&id);
            }
        }
    }
}

/// The daemon's app children.
#[derive(Default)]
pub(crate) struct AppChildren {
    state: Mutex<State>,
    /// Signalled at every child's exit: the daemon's end waits on it
    /// ([`AppChildren::end_with_daemon`]).
    exits: Condvar,
    /// The mux whose work, reaper and read loop the children use; bound
    /// once it is built.
    owner: OnceLock<Weak<Mux>>,
}

/// What a finished event leaves to do outside the lock.
#[derive(Default)]
struct Effects {
    reads: Vec<(ReadReply, Chunk)>,
    start: Option<(StartReply, Result<Value, ChildError>)>,
    /// Stop this child's group (it missed its ready line).
    terminate: Option<i32>,
}

impl Effects {
    fn run(self) {
        for (reply, chunk) in self.reads {
            reply(chunk);
        }
        if let Some((reply, result)) = self.start {
            reply(result);
        }
        if let Some(pid) = self.terminate {
            signal_group(pid, libc::SIGTERM);
        }
    }
}

impl AppChildren {
    pub(crate) fn bind(&self, mux: Weak<Mux>) {
        let _ = self.owner.set(mux);
    }

    pub(super) fn mux(&self) -> Option<Arc<Mux>> {
        self.owner.get().and_then(Weak::upgrade)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes a running slot for a spawn, or refuses past [`MAX_RUNNING`].
    pub(crate) fn reserve(&self) -> Result<(), ChildError> {
        let mut state = self.lock();
        if state.running() >= MAX_RUNNING {
            return Err(ChildError::Limit);
        }
        state.spawning += 1;
        Ok(())
    }

    /// Gives back a slot [`Self::reserve`] took for a start that never ran.
    pub(crate) fn unreserve(&self) {
        let mut state = self.lock();
        state.spawning = state.spawning.saturating_sub(1);
    }

    /// Starts `spec` in the slot [`Self::reserve`] took; `reply` gets
    /// `{child, pid, secret?, ready_line?}` once it runs, or once its ready
    /// line came. It spawns a process: call it on a work item that may
    /// block, never a reader thread.
    pub(crate) fn start(self: &Arc<Self>, mux: &Arc<Mux>, spec: Spec, reply: StartReply) {
        let spawned = spawn(&spec);
        let mut state = self.lock();
        state.spawning = state.spawning.saturating_sub(1);
        let (pid, lifeline, secret, read) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                drop(state);
                reply(Err(error));
                return;
            }
        };
        let id = crate::workspace_registry::new_uuid_v4();
        let ready_wait = spec.ready_wait.min(MAX_READY_WAIT);
        let mut child = Child {
            name: spec.name,
            owner: spec.owner,
            pid,
            secret,
            lifeline,
            buffer: VecDeque::new(),
            base: 0,
            exit: None,
            output_closed: false,
            stopping: false,
            watch: None,
            waiters: Vec::new(),
            ready: spec.ready,
            ready_line: None,
            partial: Vec::new(),
            starting: None,
        };
        let immediate = if child.ready.is_some() {
            child.starting = Some(reply);
            None
        } else {
            Some((reply, child.started_json(&id)))
        };
        state.children.insert(id.clone(), child);
        state.order.push_back(id.clone());
        state.evict();
        let grace_armed = state.grace_armed;
        drop(state);
        if mux.daemon_shutdown_requested() {
            // The daemon's end took its list before this child was in it.
            signal_group(pid, libc::SIGKILL);
        } else if !grace_armed && !super::app_connected(mux) {
            // The app left while this start was in flight and found nothing
            // running then: the lifeline rule still applies to this child.
            self.last_app_left();
        }
        if let Some((reply, started)) = immediate {
            reply(Ok(started));
        } else {
            self.ready_deadline(mux, id.clone(), ready_wait);
        }
        // Watched only once the child is in the table, so its first bytes
        // and its exit always find it.
        let sink = Box::new(ChildOutput { children: Arc::downgrade(self), id: id.clone() });
        match output::watch(mux, read, sink) {
            Ok(watch) => {
                if let Some(child) = self.lock().children.get_mut(&id) {
                    child.watch = Some(watch);
                }
            }
            // The sink heard the end already; the child still runs and exits.
            Err(error) => eprintln!("cmux-tui: app child output not watched: {error}"),
        }
        let children = Arc::downgrade(self);
        let on_exit = Box::new(move |status: io::Result<ExitStatus>| {
            if let Some(children) = children.upgrade() {
                children.exited(&id, Exit::of(&status));
            }
        });
        if let Err(unwatched) = mux.reaper.watch(pid.unsigned_abs(), on_exit) {
            // Nothing can report its end: end it now rather than keep a
            // child that never finishes.
            eprintln!("cmux-tui: app child exit not watched: {}", unwatched.error());
            signal_group(pid, libc::SIGKILL);
        }
    }

    fn ready_deadline(self: &Arc<Self>, mux: &Mux, id: String, wait: Duration) {
        let children = Arc::downgrade(self);
        let seconds = wait.as_secs();
        let expiring = id.clone();
        let posted = mux.work.post_delayed(
            wait,
            Priority::UserVisible,
            Lane::Cpu,
            Shutdown::ContinueOnShutdown,
            move || {
                if let Some(children) = children.upgrade() {
                    children.not_ready(&expiring, format!("no ready line within {seconds} s"));
                }
            },
        );
        if posted.is_err() {
            // Shutdown began: nothing would end the wait.
            self.not_ready(&id, "the daemon is shutting down".into());
        }
    }

    /// The child missed its ready line: the start fails and the child ends.
    fn not_ready(&self, id: &str, reason: String) {
        let mut effects = Effects::default();
        {
            let mut state = self.lock();
            let Some(child) = state.children.get_mut(id) else { return };
            let Some(reply) = child.starting.take() else { return };
            effects.start = Some((reply, Err(ChildError::NotReady(reason))));
            if child.exit.is_none() {
                child.stopping = true;
                child.lifeline = None;
                effects.terminate = Some(child.pid);
            }
        }
        effects.run();
    }

    /// Answers `reply` with output after `after`: at once when there is
    /// news, else at the next output or end, or after `wait`.
    pub(crate) fn read(
        self: &Arc<Self>,
        mux: &Mux,
        client: u64,
        id: &str,
        after: u64,
        wait: ReadWait,
        reply: ReadReply,
    ) -> Result<(), ChildError> {
        let mut state = self.lock();
        let token = state.next_token;
        state.next_token += 1;
        let child = state.children.get_mut(id).ok_or(ChildError::Unknown)?;
        let due = match wait {
            ReadWait::UntilExit => child.finished(),
            ReadWait::For(wait) => child.has_news(after) || wait.is_zero(),
        };
        if due {
            let chunk = child.chunk(after);
            drop(state);
            reply(chunk);
            return Ok(());
        }
        if child.waiters.len() >= MAX_WAITERS {
            return Err(ChildError::Limit);
        }
        let until_exit = wait == ReadWait::UntilExit;
        child.waiters.push(Waiter { token, client, after, until_exit, reply });
        drop(state);
        let ReadWait::For(wait) = wait else { return Ok(()) };
        let children = Arc::downgrade(self);
        let expiring = id.to_owned();
        let posted = mux.work.post_delayed(
            wait.min(MAX_READ_WAIT),
            Priority::UserVisible,
            Lane::Cpu,
            Shutdown::ContinueOnShutdown,
            move || {
                if let Some(children) = children.upgrade() {
                    children.expire(&expiring, token);
                }
            },
        );
        if posted.is_err() {
            // Shutdown began: answer with what there is now.
            self.expire(id, token);
        }
        Ok(())
    }

    /// Connection `client` closed: its parked reads go with it, unanswered.
    /// An exit watch has no deadline, so without this every app life that
    /// watched a long-lived child would keep a slot of its
    /// [`MAX_WAITERS`] until the child ends.
    pub(crate) fn disconnect(&self, client: u64) {
        let dropped: Vec<Waiter> = {
            let mut state = self.lock();
            let mut dropped = Vec::new();
            for child in state.children.values_mut() {
                let (gone, kept): (Vec<Waiter>, Vec<Waiter>) = std::mem::take(&mut child.waiters)
                    .into_iter()
                    .partition(|w| w.client == client);
                child.waiters = kept;
                dropped.extend(gone);
            }
            dropped
        };
        // The replies drop outside the lock.
        drop(dropped);
    }

    fn expire(&self, id: &str, token: u64) {
        let mut state = self.lock();
        let Some(child) = state.children.get_mut(id) else { return };
        let Some(index) = child.waiters.iter().position(|waiter| waiter.token == token) else {
            return;
        };
        let waiter = child.waiters.swap_remove(index);
        let chunk = child.chunk(waiter.after);
        drop(state);
        (waiter.reply)(chunk);
    }

    /// Stops child `id` with `mode`. False for an unknown child; true for
    /// one that already ended or is already being stopped.
    pub(crate) fn stop(self: &Arc<Self>, id: &str, mode: StopMode) -> bool {
        let (pid, lifeline) = {
            let mut state = self.lock();
            let Some(child) = state.children.get_mut(id) else { return false };
            if child.exit.is_some() || (child.stopping && mode != StopMode::Hangup) {
                return true;
            }
            child.stopping = true;
            let lifeline = if mode == StopMode::Terminate { child.lifeline.take() } else { None };
            (child.pid, lifeline)
        };
        drop(lifeline);
        match mode {
            StopMode::Hangup => {
                signal_group(pid, libc::SIGHUP);
                signal_group(pid, libc::SIGCONT);
            }
            StopMode::Interrupt => {
                signal_group(pid, libc::SIGINT);
                self.escalate(
                    id.to_owned(),
                    INTERRUPT_ESCALATION,
                    vec![libc::SIGTERM, libc::SIGKILL],
                );
            }
            StopMode::Terminate => {
                signal_group(pid, libc::SIGTERM);
                self.escalate(id.to_owned(), TERMINATE_GRACE, vec![libc::SIGKILL]);
            }
        }
        true
    }

    /// Sends the next of `signals` after `every` while the child still runs.
    fn escalate(self: &Arc<Self>, id: String, every: Duration, mut signals: Vec<i32>) {
        if signals.is_empty() {
            return;
        }
        let Some(mux) = self.mux() else { return };
        let signal = signals.remove(0);
        let children = Arc::downgrade(self);
        let _ = mux.work.post_delayed(
            every,
            Priority::BestEffort,
            Lane::Cpu,
            Shutdown::ContinueOnShutdown,
            move || {
                let Some(children) = children.upgrade() else { return };
                let pid = match children.lock().children.get(&id) {
                    Some(child) if child.exit.is_none() => child.pid,
                    _ => return,
                };
                signal_group(pid, signal);
                children.escalate(id, every, signals);
            },
        );
    }

    /// Terminates every running child: the app quit cleanly
    /// (`app-child-release-all`), no app came back within the grace, or the
    /// daemon is ending. Returns how many were running.
    pub(crate) fn terminate_all(self: &Arc<Self>) -> usize {
        let running: Vec<String> = {
            let state = self.lock();
            state
                .order
                .iter()
                .filter(|id| state.children.get(*id).is_some_and(|child| child.exit.is_none()))
                .cloned()
                .collect()
        };
        for id in &running {
            self.stop(id, StopMode::Terminate);
        }
        running.len()
    }

    /// The daemon's end ([`crate::mux::Mux::shutdown`]): every running
    /// child is terminated, the children get up to [`SHUTDOWN_WAIT`] to exit, and the
    /// groups still running then are killed. The usual escalation is a
    /// posted item that shutdown does not wait for, so a child that ignores
    /// SIGTERM would outlive the daemon without this. Returns how many were
    /// running.
    pub(crate) fn end_with_daemon(self: &Arc<Self>) -> usize {
        self.end_within(SHUTDOWN_WAIT)
    }

    /// [`Self::end_with_daemon`] with `wait` for the exits.
    pub(crate) fn end_within(self: &Arc<Self>, wait: Duration) -> usize {
        let running = self.terminate_all();
        if running == 0 {
            return 0;
        }
        let state = self.lock();
        let (state, _) = self
            .exits
            .wait_timeout_while(state, wait, |state| {
                state.children.values().any(|child| child.exit.is_none())
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let left: Vec<i32> =
            state.children.values().filter(|child| child.exit.is_none()).map(|c| c.pid).collect();
        drop(state);
        for pid in left {
            signal_group(pid, libc::SIGKILL);
        }
        running
    }

    /// Every child, oldest first, or only `owner`'s.
    pub(crate) fn list(&self, owner: Option<&str>) -> Vec<Value> {
        let state = self.lock();
        state
            .order
            .iter()
            .filter_map(|id| state.children.get(id).map(|child| (id, child)))
            .filter(|(_, child)| owner.is_none_or(|owner| child.owner == owner))
            .map(|(id, child)| child.row(id))
            .collect()
    }

    /// A child runs or a start is between its reserve and its insert.
    pub(crate) fn any_running(&self) -> bool {
        self.lock().running() > 0
    }

    /// Starts a new grace period: its generation and how long it lasts.
    pub(crate) fn begin_grace(&self, grace: Duration) -> (u64, Duration) {
        let mut state = self.lock();
        state.grace_generation += 1;
        state.grace_armed = true;
        #[cfg(test)]
        let grace = state.grace.unwrap_or(grace);
        (state.grace_generation, grace)
    }

    #[cfg(test)]
    pub(crate) fn set_grace_for_test(&self, grace: Duration) {
        self.lock().grace = Some(grace);
    }

    /// Ends any grace period (an app came back, or the grace ran out).
    pub(crate) fn end_grace(&self) {
        let mut state = self.lock();
        state.grace_generation += 1;
        state.grace_armed = false;
    }

    /// Whether `generation` is still the current grace period.
    pub(crate) fn grace_current(&self, generation: u64) -> bool {
        self.lock().grace_generation == generation
    }

    fn output(&self, id: &str, bytes: &[u8]) {
        let mut effects = Effects::default();
        {
            let mut state = self.lock();
            let Some(child) = state.children.get_mut(id) else { return };
            child.buffer.extend(bytes);
            if child.buffer.len() > MAX_BUFFER {
                let cut = child.buffer.len() - MAX_BUFFER;
                child.buffer.drain(..cut);
                child.base += cut as u64;
            }
            if child.scan_ready(bytes)
                && let Some(reply) = child.starting.take()
            {
                effects.start = Some((reply, Ok(child.started_json(id))));
            }
            effects.reads = child.ready_waiters();
        }
        effects.run();
    }

    fn output_ended(&self, id: &str) {
        let mut effects = Effects::default();
        {
            let mut state = self.lock();
            let Some(child) = state.children.get_mut(id) else { return };
            child.output_closed = true;
            child.watch = None;
            if let Some(reply) = child.starting.take() {
                let reason = "its output ended first".to_owned();
                effects.start = Some((reply, Err(ChildError::NotReady(reason))));
                if child.exit.is_none() {
                    child.stopping = true;
                    child.lifeline = None;
                    effects.terminate = Some(child.pid);
                }
            }
            effects.reads = child.ready_waiters();
        }
        effects.run();
    }

    fn exited(self: &Arc<Self>, id: &str, exit: Exit) {
        let mut effects = Effects::default();
        let drain = {
            let mut state = self.lock();
            let Some(child) = state.children.get_mut(id) else { return };
            child.exit = Some(exit);
            child.lifeline = None;
            if let Some(reply) = child.starting.take() {
                let reason = format!("it exited first ({})", exit.json());
                effects.start = Some((reply, Err(ChildError::NotReady(reason))));
            }
            effects.reads = child.ready_waiters();
            !child.output_closed
        };
        self.exits.notify_all();
        effects.run();
        if !drain {
            return;
        }
        let Some(mux) = self.mux() else { return };
        let children = Arc::downgrade(self);
        let id = id.to_owned();
        let _ = mux.work.post_delayed(
            DRAIN_AFTER_EXIT,
            Priority::UserVisible,
            Lane::Cpu,
            Shutdown::ContinueOnShutdown,
            move || {
                let Some(children) = children.upgrade() else { return };
                if let Some(watch) =
                    children.lock().children.get(&id).and_then(|c| c.watch.as_ref())
                {
                    watch.give_up();
                }
            },
        );
    }
}

/// Spawns `spec` in its own process group with stdout and stderr in one
/// pipe: its pid, the lifeline, the secret and the pipe's read end.
fn spawn(spec: &Spec) -> Result<(i32, Option<ChildStdin>, Option<String>, OwnedFd), ChildError> {
    if let Some(cwd) = &spec.cwd
        && !cmux_threads::checked::fs::metadata(cwd).is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(ChildError::FolderMissing);
    }
    let spawn_error = |error: io::Error| ChildError::Spawn(error.to_string());
    let (read, write) = output::pipe().map_err(spawn_error)?;
    let error_write = write.try_clone().map_err(spawn_error)?;
    let secret = if spec.secret { Some(new_secret()?) } else { None };
    // mq14-allow: a work item that may block spawns the child, never a reader thread.
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(key, value)| (key.as_str(), value.as_str())))
        .stdout(Stdio::from(write))
        .stderr(Stdio::from(error_write))
        // Its own process group: a signal to the daemon's group does not
        // reach it, and a stop reaches what it started.
        .process_group(0);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    match spec.input {
        Input::Null => command.stdin(Stdio::null()),
        Input::Lifeline => command.stdin(Stdio::piped()),
    };
    crate::request_path::note(crate::request_path::BlockingKind::ProcessSpawn);
    let mut child = command.spawn().map_err(spawn_error)?;
    // The child holds its own ends of the pipe; ours closed with `command`.
    drop(command);
    let pid = i32::try_from(child.id()).map_err(|_| ChildError::Spawn("no pid".into()))?;
    let mut lifeline = child.stdin.take();
    if let (Some(secret), Some(stdin)) = (&secret, lifeline.as_mut()) {
        // 65 bytes fit any pipe buffer: the write never waits.
        let written =
            stdin.write_all(format!("{secret}\n").as_bytes()).and_then(|()| stdin.flush());
        if let Err(error) = written {
            signal_group(pid, libc::SIGKILL);
            // No reaper watches it yet: reap it here.
            let _ = child.wait();
            return Err(ChildError::Spawn(error.to_string()));
        }
    }
    Ok((pid, lifeline, secret, read))
}

fn new_secret() -> Result<String, ChildError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| ChildError::Spawn(format!("no random source: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) fn signal_group(pid: i32, signal: i32) {
    if pid > 0 {
        // SAFETY: plain syscall on the child's process group.
        unsafe { libc::killpg(pid, signal) };
    }
}

/// The sink of one child's pipe.
struct ChildOutput {
    children: Weak<AppChildren>,
    id: String,
}

impl OutputSink for ChildOutput {
    fn bytes(&mut self, bytes: &[u8]) {
        if let Some(children) = self.children.upgrade() {
            children.output(&self.id, bytes);
        }
    }

    fn ended(self: Box<Self>) {
        if let Some(children) = self.children.upgrade() {
            children.output_ended(&self.id);
        }
    }
}

/// How many leading bytes of `bytes` end on a whole UTF-8 character.
pub(crate) fn utf8_complete(bytes: &[u8]) -> usize {
    let count = bytes.len();
    for back in 0..count.min(4) {
        let byte = bytes[count - 1 - back];
        if byte & 0xC0 != 0x80 {
            let length = match byte {
                0xF0..=0xFF => 4,
                0xE0..=0xEF => 3,
                0xC0..=0xDF => 2,
                _ => 1,
            };
            return if back + 1 >= length { count } else { count - back - 1 };
        }
    }
    count
}
