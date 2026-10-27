//! Per-subscriber mux event delivery with bounded coalesced state.

use crate::panic_free::RecoverPoison;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use crate::event_cause::EventCause;
use crate::{MuxEvent, PaneId, ScreenId, SurfaceId, TreeDelta, TreeDeltaKind, WorkspaceId};

// A subscriber may drain accepted events after crossing this limit, then observes a disconnect.
const MAX_PENDING_EVENTS: usize = 4_096;

type SessionPathUpdate = (SurfaceId, WorkspaceId, ScreenId, PaneId);

thread_local! {
    /// Session path updates held back while a resource mutation is staged
    /// on a candidate state. `None` outside [`defer_session_paths`].
    static DEFERRED_SESSION_PATHS: RefCell<Option<Vec<SessionPathUpdate>>> =
        const { RefCell::new(None) };
}

/// Session path updates recorded by [`defer_session_paths`]. Pass them to
/// [`MuxEventBroadcaster::publish_deferred_session_paths`] after the change
/// commits; drop them when it does not.
#[must_use]
pub(crate) struct DeferredSessionPaths(Vec<SessionPathUpdate>);

/// Run `f` and hold back every surface session path update it makes on this
/// thread. A resource mutation prepares and stages its state change before
/// the durable commit; subscribers must see the new path only after that
/// commit succeeds.
pub(crate) fn defer_session_paths<R>(f: impl FnOnce() -> R) -> (R, DeferredSessionPaths) {
    struct Restore(Option<Option<Vec<SessionPathUpdate>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                DEFERRED_SESSION_PATHS.with(|cell| *cell.borrow_mut() = previous);
            }
        }
    }
    let previous = DEFERRED_SESSION_PATHS.with(|cell| cell.borrow_mut().replace(Vec::new()));
    let mut restore = Restore(Some(previous));
    let output = f();
    let previous = restore.0.take().flatten();
    let recorded = DEFERRED_SESSION_PATHS
        .with(|cell| std::mem::replace(&mut *cell.borrow_mut(), previous))
        .unwrap_or_default();
    (output, DeferredSessionPaths(recorded))
}

#[derive(Default)]
pub struct MuxEventBroadcaster {
    subscribers: Mutex<Vec<MuxEventSubscriber>>,
}

struct MuxEventSubscriber {
    mailbox: Weak<MuxEventMailbox>,
    filter: MuxEventFilter,
}

enum MuxEventFilter {
    All,
    ConfigReload,
    /// Events that can change which terminals have zero placements.
    TerminalTopology,
    /// Events that change the launch snapshot's layout or projections. Title
    /// changes alone are left out: a busy terminal retitles itself many
    /// times a second, and the live tree corrects a stale title at once.
    LaunchSnapshot,
    /// Local conversation changes (the host replica's Chief stream).
    Conversations,
    /// Only events of these kinds: a page relay's interest set, so an event
    /// it never hears is not cloned into its mailbox or rendered.
    Kinds(MuxEventKinds),
    AttachedSurface(SurfaceId),
    SurfaceSession(SurfaceSessionScope),
}

struct SurfaceSessionScope {
    surface: SurfaceId,
    workspace: WorkspaceId,
    screen: ScreenId,
    pane: PaneId,
}

#[derive(Clone)]
pub struct MuxEventReceiver {
    mailbox: Arc<MuxEventMailbox>,
}

#[derive(Default)]
struct MuxEventMailbox {
    state: Mutex<MuxEventMailboxState>,
    changed: Condvar,
    /// The subscriber's drain task when it runs on a sequence instead of
    /// blocking on `changed` (attach streams, p05-ipc-079).
    ready_hook: crate::stream_interrupt::ReadyHook,
    /// The subscriber writes events to a wire and takes each event's shared
    /// [`EventEncodings`] ([`MuxEventReceiver::share_encodings`]).
    shares_encodings: AtomicBool,
}

#[derive(Default)]
struct MuxEventMailboxState {
    next_sequence: u128,
    events: VecDeque<(u128, DeliveredEvent)>,
    coalesced_sequences: HashMap<CoalescedEventKey, u128>,
    coalesced: BTreeMap<u128, (CoalescedEventKey, DeliveredEvent)>,
    closed: bool,
    overflowed: bool,
}

/// One event as a mailbox hands it to its subscriber.
#[derive(Clone)]
pub(crate) struct DeliveredEvent {
    pub(crate) event: MuxEvent,
    pub(crate) cause: EventCause,
    /// The wire encodings of this emit, one slot shared by every mailbox
    /// that took it; `None` for a subscriber that does not share them and
    /// for a token only one receiver gets ([`MuxEventReceiver::wake`]).
    pub(crate) encodings: Option<Arc<EventEncodings>>,
}

impl DeliveredEvent {
    fn unshared(event: MuxEvent, cause: EventCause) -> Self {
        Self { event, cause, encodings: None }
    }
}

/// What a subscriber's wire line of an event depends on beyond the event
/// itself. Subscribers of one class write the same bytes, so the first of
/// them to take the event serializes it and the others reuse it
/// ([`EventEncodings`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncodeClass {
    /// A tree delta rendered in full (`tree_events: deltas`), not as
    /// `tree-changed`. Only tree events depend on it.
    pub(crate) tree_deltas: bool,
    /// The `cause` member the line carries (`event-cause-v1`).
    pub(crate) cause: Option<EventCause>,
}

impl EncodeClass {
    const COUNT: usize = 8;

    const fn index(self) -> usize {
        let cause = match self.cause {
            None => 0,
            Some(EventCause::Automation) => 1,
            Some(EventCause::Program) => 2,
            Some(EventCause::Frontend) => 3,
        };
        cause * 2 + self.tree_deltas as usize
    }
}

/// One emitted event's wire encodings, one per [`EncodeClass`], made by
/// the first subscriber of the class that writes it. Chromium serializes a
/// message for many receivers once and hands each a shallow clone that
/// shares the encoded bytes (blink::CloneableMessage::ShallowClone, used by
/// content/browser/broadcast_channel/broadcast_channel_service.cc).
#[derive(Default)]
pub(crate) struct EventEncodings {
    classes: [OnceLock<Option<Arc<EncodedEvent>>>; EncodeClass::COUNT],
}

impl EventEncodings {
    /// The event's encoding in `class`: `encode`'s result, run by the first
    /// caller of the class only (a concurrent caller waits for it). `None`
    /// when the class writes no line for the event.
    pub(crate) fn get_or_encode(
        &self,
        class: EncodeClass,
        encode: impl FnOnce() -> Option<EncodedEvent>,
    ) -> Option<&Arc<EncodedEvent>> {
        self.classes[class.index()].get_or_init(|| encode().map(Arc::new)).as_ref()
    }
}

/// One event as one JSON object, serialized once for every subscriber of
/// its [`EncodeClass`].
pub(crate) struct EncodedEvent {
    /// The object's `event` member, which page relays filter on.
    pub(crate) name: Box<str>,
    pub(crate) json: Arc<[u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum CoalescedEventKey {
    ConfigReload,
    Agent(SurfaceId),
    Title(SurfaceId),
    ProgramStatus(SurfaceId),
    SurfaceOutput(SurfaceId),
    Scroll(SurfaceId),
}

impl MuxEventBroadcaster {
    pub fn subscribe(&self) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::All)
    }

    pub fn subscribe_config_reload(&self) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::ConfigReload)
    }

    pub(crate) fn subscribe_terminal_topology(&self) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::TerminalTopology)
    }

    pub(crate) fn subscribe_launch_snapshot(&self) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::LaunchSnapshot)
    }

    pub(crate) fn subscribe_conversations(&self) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::Conversations)
    }

    /// A subscription that receives only events of `kinds`.
    pub(crate) fn subscribe_kinds(&self, kinds: MuxEventKinds) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::Kinds(kinds))
    }

    pub fn subscribe_attached_surface(&self, surface: SurfaceId) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::AttachedSurface(surface))
    }

    pub fn subscribe_surface_session(
        &self,
        surface: SurfaceId,
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
    ) -> MuxEventReceiver {
        self.subscribe_with_filter(MuxEventFilter::SurfaceSession(SurfaceSessionScope {
            surface,
            workspace,
            screen,
            pane,
        }))
    }

    pub(crate) fn update_surface_session_path(
        &self,
        surface: SurfaceId,
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
    ) {
        let deferred = DEFERRED_SESSION_PATHS.with(|cell| {
            cell.borrow_mut()
                .as_mut()
                .map(|updates| updates.push((surface, workspace, screen, pane)))
        });
        if deferred.is_some() {
            return;
        }
        let mut subscribers = self.subscribers.lock().recover_poison();
        subscribers.retain_mut(|subscriber| {
            let Some(mailbox) = subscriber.mailbox.upgrade() else { return false };
            if let MuxEventFilter::SurfaceSession(scope) = &mut subscriber.filter
                && scope.surface == surface
            {
                scope.workspace = workspace;
                scope.screen = screen;
                scope.pane = pane;
                return mailbox
                    .push(DeliveredEvent::unshared(MuxEvent::TreeChanged, EventCause::current()));
            }
            true
        });
    }

    /// Publish session path updates held back while a committed change was
    /// staged.
    pub(crate) fn publish_deferred_session_paths(&self, deferred: DeferredSessionPaths) {
        for (surface, workspace, screen, pane) in deferred.0 {
            self.update_surface_session_path(surface, workspace, screen, pane);
        }
    }

    fn subscribe_with_filter(&self, filter: MuxEventFilter) -> MuxEventReceiver {
        let mailbox = Arc::new(MuxEventMailbox::default());
        self.subscribers
            .lock()
            .recover_poison()
            .push(MuxEventSubscriber { mailbox: Arc::downgrade(&mailbox), filter });
        MuxEventReceiver { mailbox }
    }

    /// Subscriptions whose receiver is still alive (`oracle-state`).
    #[cfg(any(debug_assertions, feature = "oracle"))]
    pub(crate) fn live_subscriber_count(&self) -> usize {
        let subscribers = self.subscribers.lock().recover_poison();
        subscribers.iter().filter(|subscriber| subscriber.mailbox.strong_count() > 0).count()
    }

    /// Deliver `event` with the cause of this thread's scope
    /// ([`EventCause::current`]).
    pub fn emit(&self, event: MuxEvent) {
        self.emit_caused(event, EventCause::current());
    }

    pub(crate) fn emit_caused(&self, event: MuxEvent, cause: EventCause) {
        let mut subscribers = self.subscribers.lock().recover_poison();
        // One encodings slot per emit, made only when a wire subscriber takes it.
        let mut shared: Option<Arc<EventEncodings>> = None;
        subscribers.retain_mut(|subscriber| {
            let Some(mailbox) = subscriber.mailbox.upgrade() else { return false };
            if !subscriber.filter.accepts(&event) {
                return true;
            }
            let encodings = mailbox
                .shares_encodings
                .load(Ordering::Relaxed)
                .then(|| shared.get_or_insert_with(Arc::default).clone());
            mailbox.push(DeliveredEvent { event: event.clone(), cause, encodings })
        });
    }
}

/// The variant of a [`MuxEvent`], the unit of a [`MuxEventKinds`] interest
/// set. Chromium routes an event only to listeners registered for its name
/// (extensions/browser/event_router.cc, `EventRouter::DispatchEventImpl`);
/// a subscription with an interest set is filtered the same way, on the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MuxEventKind {
    SurfaceOutput,
    SurfaceResized,
    SurfaceResizeFailed,
    SurfaceExited,
    ProgramStatusChanged,
    TitleChanged,
    AgentChanged,
    AgentPresenceChanged,
    Bell,
    Notification,
    GraphicsStatus,
    Status,
    ConfigReloadRequested,
    WindowTitleRequested,
    ScrollChanged,
    TreeChanged,
    TreeSelectionChanged,
    TreeDelta,
    FrontendProjectionChanged,
    PersonalChanged,
    BookmarksChanged,
    HistoryChanged,
    SettingsChanged,
    KeybindingsChanged,
    Conversation,
    CloudConversation,
    TerminalRegistryChanged,
    TerminalReaped,
    LayoutChanged,
    ClientAttached,
    ClientChanged,
    ClientDetached,
    SizeStateChanged,
    ClientListInvalidated,
    PairingRequested,
    PairingResolved,
    MachineUsageChanged,
    DiagnosticSite,
    Empty,
}

// Every kind is one bit of `MuxEventKinds`.
const _: () = assert!(MuxEventKind::COUNT <= u64::BITS as usize);

impl MuxEventKind {
    /// How many kinds there are (`Empty` is the last).
    pub(crate) const COUNT: usize = Self::Empty as usize + 1;

    pub(crate) const fn of(event: &MuxEvent) -> Self {
        match event {
            MuxEvent::SurfaceOutput(_) => Self::SurfaceOutput,
            MuxEvent::SurfaceResized { .. } => Self::SurfaceResized,
            MuxEvent::SurfaceResizeFailed { .. } => Self::SurfaceResizeFailed,
            MuxEvent::SurfaceExited(..) => Self::SurfaceExited,
            MuxEvent::ProgramStatusChanged { .. } => Self::ProgramStatusChanged,
            MuxEvent::TitleChanged { .. } => Self::TitleChanged,
            MuxEvent::AgentChanged { .. } => Self::AgentChanged,
            MuxEvent::AgentPresenceChanged { .. } => Self::AgentPresenceChanged,
            MuxEvent::Bell(_) => Self::Bell,
            MuxEvent::Notification(_) => Self::Notification,
            MuxEvent::GraphicsStatus(_) => Self::GraphicsStatus,
            MuxEvent::Status(_) => Self::Status,
            MuxEvent::ConfigReloadRequested => Self::ConfigReloadRequested,
            MuxEvent::WindowTitleRequested(_) => Self::WindowTitleRequested,
            MuxEvent::ScrollChanged { .. } => Self::ScrollChanged,
            MuxEvent::TreeChanged => Self::TreeChanged,
            MuxEvent::TreeSelectionChanged => Self::TreeSelectionChanged,
            MuxEvent::TreeDelta(_) => Self::TreeDelta,
            MuxEvent::FrontendProjectionChanged { .. } => Self::FrontendProjectionChanged,
            MuxEvent::PersonalChanged { .. } => Self::PersonalChanged,
            MuxEvent::BookmarksChanged(_) => Self::BookmarksChanged,
            MuxEvent::HistoryChanged { .. } => Self::HistoryChanged,
            MuxEvent::SettingsChanged(_) => Self::SettingsChanged,
            MuxEvent::KeybindingsChanged(_) => Self::KeybindingsChanged,
            MuxEvent::Conversation(_) => Self::Conversation,
            MuxEvent::CloudConversation(_) => Self::CloudConversation,
            MuxEvent::TerminalRegistryChanged { .. } => Self::TerminalRegistryChanged,
            MuxEvent::TerminalReaped { .. } => Self::TerminalReaped,
            MuxEvent::LayoutChanged(..) => Self::LayoutChanged,
            MuxEvent::ClientAttached { .. } => Self::ClientAttached,
            MuxEvent::ClientChanged { .. } => Self::ClientChanged,
            MuxEvent::ClientDetached(_) => Self::ClientDetached,
            MuxEvent::SizeStateChanged { .. } => Self::SizeStateChanged,
            MuxEvent::ClientListInvalidated => Self::ClientListInvalidated,
            MuxEvent::PairingRequested(_) => Self::PairingRequested,
            MuxEvent::PairingResolved { .. } => Self::PairingResolved,
            MuxEvent::MachineUsageChanged(_) => Self::MachineUsageChanged,
            MuxEvent::DiagnosticSite(_) => Self::DiagnosticSite,
            MuxEvent::Empty => Self::Empty,
        }
    }
}

/// A set of [`MuxEventKind`]s: what one subscriber wants to hear.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MuxEventKinds(u64);

impl MuxEventKinds {
    pub(crate) const NONE: Self = Self(0);

    pub(crate) const fn with(self, kind: MuxEventKind) -> Self {
        Self(self.0 | 1 << kind as u32)
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn contains(self, kind: MuxEventKind) -> bool {
        self.0 & (1 << kind as u32) != 0
    }

    pub(crate) const fn accepts(self, event: &MuxEvent) -> bool {
        self.contains(MuxEventKind::of(event))
    }
}

impl MuxEventFilter {
    fn accepts(&mut self, event: &MuxEvent) -> bool {
        match self {
            Self::All => true,
            Self::ConfigReload => matches!(event, MuxEvent::ConfigReloadRequested),
            Self::TerminalTopology => matches!(
                event,
                MuxEvent::TreeChanged
                    | MuxEvent::TreeDelta(_)
                    | MuxEvent::TerminalRegistryChanged { .. }
                    | MuxEvent::SurfaceExited(..)
                    | MuxEvent::Empty
            ),
            Self::LaunchSnapshot => matches!(
                event,
                MuxEvent::TreeChanged
                    | MuxEvent::TreeSelectionChanged
                    | MuxEvent::TreeDelta(_)
                    | MuxEvent::LayoutChanged(..)
                    | MuxEvent::PersonalChanged { .. }
                    | MuxEvent::FrontendProjectionChanged { .. }
                    | MuxEvent::Empty
            ),
            Self::Conversations => matches!(event, MuxEvent::Conversation(_) | MuxEvent::Empty),
            Self::Kinds(kinds) => kinds.accepts(event),
            Self::AttachedSurface(surface) => match event {
                MuxEvent::Notification(notification) => notification.surface == Some(*surface),
                MuxEvent::ScrollChanged { surface: event_surface, .. } => {
                    *event_surface == *surface
                }
                _ => false,
            },
            Self::SurfaceSession(scope) => scope.accepts(event),
        }
    }
}

impl SurfaceSessionScope {
    fn accepts(&mut self, event: &MuxEvent) -> bool {
        match event {
            MuxEvent::SurfaceOutput(surface)
            | MuxEvent::SurfaceExited(surface, _)
            | MuxEvent::ProgramStatusChanged { surface, .. }
            | MuxEvent::Bell(surface) => *surface == self.surface,
            MuxEvent::SurfaceResized { surface, .. }
            | MuxEvent::SurfaceResizeFailed { surface, .. }
            | MuxEvent::AgentChanged { surface, .. }
            | MuxEvent::AgentPresenceChanged { surface, .. }
            | MuxEvent::TitleChanged { surface, .. }
            | MuxEvent::ScrollChanged { surface, .. }
            | MuxEvent::SizeStateChanged { surface, .. } => *surface == self.surface,
            MuxEvent::Notification(notification) => {
                notification.surface.is_none_or(|surface| surface == self.surface)
            }
            MuxEvent::TreeDelta(delta) => self.accepts_tree_delta(delta),
            // A surface-only client always renders its target across the full
            // host terminal. Screen layout churn therefore carries no useful
            // state and would only force repeated whole-tree refreshes.
            MuxEvent::LayoutChanged(..) => false,
            MuxEvent::ClientAttached { .. }
            | MuxEvent::ClientChanged { .. }
            | MuxEvent::ClientDetached(_)
            | MuxEvent::ClientListInvalidated
            | MuxEvent::TreeChanged
            | MuxEvent::TreeSelectionChanged
            // The daemon's sites go to the app's control connection only.
            | MuxEvent::DiagnosticSite(_) => false,
            MuxEvent::GraphicsStatus(_)
            | MuxEvent::Status(_)
            | MuxEvent::ConfigReloadRequested
            | MuxEvent::WindowTitleRequested(_)
            | MuxEvent::FrontendProjectionChanged { .. }
            | MuxEvent::PersonalChanged { .. }
            | MuxEvent::BookmarksChanged(_)
            | MuxEvent::HistoryChanged { .. }
            | MuxEvent::SettingsChanged(_)
            | MuxEvent::KeybindingsChanged(_)
            | MuxEvent::Conversation(_)
            | MuxEvent::CloudConversation(_)
            | MuxEvent::TerminalRegistryChanged { .. }
            | MuxEvent::TerminalReaped { .. }
            | MuxEvent::PairingRequested(_)
            | MuxEvent::PairingResolved { .. }
            | MuxEvent::MachineUsageChanged(_)
            | MuxEvent::Empty => true,
        }
    }

    fn accepts_tree_delta(&mut self, delta: &TreeDelta) -> bool {
        let relevant = match delta.kind {
            TreeDeltaKind::TabAdded
            | TreeDeltaKind::TabClosed
            | TreeDeltaKind::TabRenamed
            | TreeDeltaKind::TabChanged => delta.surface == Some(self.surface),
            TreeDeltaKind::PaneClosed => delta.pane == Some(self.pane),
            TreeDeltaKind::ScreenClosed => delta.screen == Some(self.screen),
            TreeDeltaKind::WorkspaceClosed => delta.workspace == self.workspace,
            TreeDeltaKind::WorkspaceAdded
            | TreeDeltaKind::WorkspaceRenamed
            | TreeDeltaKind::WorkspaceMoved
            | TreeDeltaKind::WorkspaceChanged
            | TreeDeltaKind::ScreenAdded
            | TreeDeltaKind::ScreenRenamed
            | TreeDeltaKind::ScreenChanged
            | TreeDeltaKind::PaneAdded => false,
        };
        if delta.surface == Some(self.surface) && delta.kind == TreeDeltaKind::TabAdded {
            self.workspace = delta.workspace;
            if let Some(screen) = delta.screen {
                self.screen = screen;
            }
            if let Some(pane) = delta.pane {
                self.pane = pane;
            }
        }
        relevant
    }
}

impl Drop for MuxEventBroadcaster {
    fn drop(&mut self) {
        for subscriber in self.subscribers.get_mut().recover_poison().drain(..) {
            if let Some(mailbox) = subscriber.mailbox.upgrade() {
                mailbox.close();
            }
        }
    }
}

impl MuxEventMailbox {
    fn push(&self, delivered: DeliveredEvent) -> bool {
        let mut state = self.state.lock().recover_poison();
        if state.closed {
            return false;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let coalesced_key = match &delivered.event {
            MuxEvent::AgentChanged { surface, .. } => Some(CoalescedEventKey::Agent(*surface)),
            MuxEvent::TitleChanged { surface, .. } => Some(CoalescedEventKey::Title(*surface)),
            MuxEvent::ProgramStatusChanged { surface, .. } => {
                Some(CoalescedEventKey::ProgramStatus(*surface))
            }
            MuxEvent::SurfaceOutput(surface) => Some(CoalescedEventKey::SurfaceOutput(*surface)),
            MuxEvent::ScrollChanged { surface, .. } => Some(CoalescedEventKey::Scroll(*surface)),
            MuxEvent::ConfigReloadRequested => Some(CoalescedEventKey::ConfigReload),
            _ => None,
        };
        let accepted = match (coalesced_key, &delivered.event) {
            (Some(key), _) => state.push_coalesced(sequence, key, delivered),
            (None, MuxEvent::SurfaceExited(surface, _)) => {
                state.discard_surface_state(*surface);
                if !state.reserve_pending_slot() {
                    false
                } else {
                    state.events.push_back((sequence, delivered));
                    true
                }
            }
            (None, MuxEvent::Empty) => {
                let mut terminal_events = state
                    .events
                    .iter()
                    .filter(|(_, queued)| {
                        matches!(queued.event, MuxEvent::SurfaceExited(..))
                            || matches!(
                                &queued.event,
                                MuxEvent::TreeDelta(delta)
                                    if delta.kind == TreeDeltaKind::WorkspaceClosed
                            )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let keep = MAX_PENDING_EVENTS.saturating_sub(1);
                if terminal_events.len() > keep {
                    terminal_events.drain(..terminal_events.len() - keep);
                }
                state.events.clear();
                state.coalesced_sequences.clear();
                state.coalesced.clear();
                state.events.extend(terminal_events);
                state.events.push_back((sequence, delivered));
                true
            }
            (None, _) => {
                if !state.reserve_pending_slot() {
                    false
                } else {
                    state.events.push_back((sequence, delivered));
                    true
                }
            }
        };
        drop(state);
        if !accepted {
            self.changed.notify_all();
            self.ready_hook.wake();
            return false;
        }
        self.changed.notify_one();
        self.ready_hook.wake();
        true
    }

    fn close(&self) {
        self.state.lock().recover_poison().closed = true;
        self.changed.notify_all();
        self.ready_hook.wake();
    }
}

impl MuxEventMailboxState {
    /// Replaces the pending event of `key`, if any. The kept event takes
    /// the higher of the two causes: a person's change folded into a later
    /// program one is still the person's.
    fn push_coalesced(
        &mut self,
        sequence: u128,
        key: CoalescedEventKey,
        mut delivered: DeliveredEvent,
    ) -> bool {
        if let Some(previous) = self.coalesced_sequences.get(&key).copied() {
            if let Some((_, previous)) = self.coalesced.remove(&previous) {
                delivered.cause = delivered.cause.merge(previous.cause);
            }
        } else if !self.reserve_pending_slot() {
            return false;
        }
        self.coalesced_sequences.insert(key, sequence);
        self.coalesced.insert(sequence, (key, delivered));
        true
    }

    fn discard_coalesced(&mut self, key: CoalescedEventKey) {
        if let Some(previous) = self.coalesced_sequences.remove(&key) {
            self.coalesced.remove(&previous);
        }
    }

    fn discard_surface_state(&mut self, surface: SurfaceId) {
        self.discard_coalesced(CoalescedEventKey::Agent(surface));
        self.discard_coalesced(CoalescedEventKey::Title(surface));
        self.discard_coalesced(CoalescedEventKey::ProgramStatus(surface));
        self.discard_coalesced(CoalescedEventKey::SurfaceOutput(surface));
        self.discard_coalesced(CoalescedEventKey::Scroll(surface));
    }

    fn reserve_pending_slot(&mut self) -> bool {
        if self.events.len() + self.coalesced.len() < MAX_PENDING_EVENTS {
            true
        } else {
            self.closed = true;
            self.overflowed = true;
            false
        }
    }

    fn pop(&mut self) -> Option<MuxEvent> {
        self.pop_delivered().map(|delivered| delivered.event)
    }

    fn pop_caused(&mut self) -> Option<(MuxEvent, EventCause)> {
        self.pop_delivered().map(|delivered| (delivered.event, delivered.cause))
    }

    fn pop_delivered(&mut self) -> Option<DeliveredEvent> {
        let event_sequence = self.events.front().map(|(sequence, ..)| *sequence);
        let coalesced_sequence = self.coalesced.first_key_value().map(|(sequence, _)| *sequence);
        let next_sequence = [event_sequence, coalesced_sequence].into_iter().flatten().min()?;
        if event_sequence == Some(next_sequence) {
            self.events.pop_front().map(|(_, delivered)| delivered)
        } else {
            let (_, (key, delivered)) = self.coalesced.pop_first()?;
            self.coalesced_sequences.remove(&key);
            Some(delivered)
        }
    }
}

impl MuxEventReceiver {
    /// Runs `hook` after each event and when the mailbox closes, for a
    /// subscriber whose drain polls `try_recv` on a sequence instead of
    /// blocking. False when a hook is set already.
    pub(crate) fn on_ready(&self, hook: impl Fn() + Send + Sync + 'static) -> bool {
        self.mailbox.ready_hook.set(hook)
    }

    pub fn close(&self) {
        self.mailbox.close();
    }

    /// Hands this subscriber each later event with the encodings slot it
    /// shares with every other wire subscriber of the same emit
    /// ([`DeliveredEvent::encodings`]).
    pub(crate) fn share_encodings(&self) {
        self.mailbox.shares_encodings.store(true, Ordering::Relaxed);
    }

    /// Wake this receiver alone with a `TreeChanged` token. Owner-internal
    /// consumers use it for state changes that no broadcast event carries.
    pub(crate) fn wake(&self) {
        self.mailbox.push(DeliveredEvent::unshared(MuxEvent::TreeChanged, EventCause::Program));
    }

    pub fn overflowed(&self) -> bool {
        self.mailbox.state.lock().recover_poison().overflowed
    }

    pub fn recv(&self) -> Result<MuxEvent, RecvError> {
        let mut state = self.mailbox.state.lock().recover_poison();
        loop {
            if let Some(event) = state.pop() {
                return Ok(event);
            }
            if state.closed {
                return Err(RecvError);
            }
            state = self.mailbox.changed.wait(state).recover_poison();
        }
    }

    /// Wakes a blocked `recv_until_interrupted` when `interrupt` fires.
    pub(crate) fn wake_on(&self, interrupt: &crate::stream_interrupt::StreamInterrupt) {
        let mailbox = Arc::downgrade(&self.mailbox);
        interrupt.on_fire(move || {
            if let Some(mailbox) = mailbox.upgrade() {
                let _state = mailbox.state.lock().unwrap_or_else(|error| error.into_inner());
                mailbox.changed.notify_all();
            }
        });
    }

    /// Blocks for an event. Returns `Timeout` once `interrupt` has fired
    /// and nothing is queued.
    pub(crate) fn recv_until_interrupted(
        &self,
        interrupt: &crate::stream_interrupt::StreamInterrupt,
    ) -> Result<MuxEvent, RecvTimeoutError> {
        self.recv_caused_until_interrupted(interrupt).map(|(event, _)| event)
    }

    /// [`Self::recv_until_interrupted`] with the event's cause.
    pub(crate) fn recv_caused_until_interrupted(
        &self,
        interrupt: &crate::stream_interrupt::StreamInterrupt,
    ) -> Result<(MuxEvent, EventCause), RecvTimeoutError> {
        self.recv_delivered_until_interrupted(interrupt)
            .map(|delivered| (delivered.event, delivered.cause))
    }

    /// [`Self::recv_until_interrupted`] with the event's cause and shared
    /// encodings.
    pub(crate) fn recv_delivered_until_interrupted(
        &self,
        interrupt: &crate::stream_interrupt::StreamInterrupt,
    ) -> Result<DeliveredEvent, RecvTimeoutError> {
        let mut state = self.mailbox.state.lock().recover_poison();
        loop {
            if let Some(delivered) = state.pop_delivered() {
                return Ok(delivered);
            }
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            if interrupt.is_fired() {
                return Err(RecvTimeoutError::Timeout);
            }
            state = self.mailbox.changed.wait(state).recover_poison();
        }
    }

    pub fn try_recv(&self) -> Result<MuxEvent, TryRecvError> {
        self.try_recv_caused().map(|(event, _)| event)
    }

    /// [`Self::try_recv`] with the event's cause.
    pub(crate) fn try_recv_caused(&self) -> Result<(MuxEvent, EventCause), TryRecvError> {
        let mut state = self.mailbox.state.lock().recover_poison();
        if let Some(event) = state.pop_caused() {
            Ok(event)
        } else if state.closed {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn try_iter(&self) -> impl Iterator<Item = MuxEvent> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<MuxEvent, RecvTimeoutError> {
        self.recv_caused_timeout(timeout).map(|(event, _)| event)
    }

    /// [`Self::recv_timeout`] with the event's cause.
    pub(crate) fn recv_caused_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(MuxEvent, EventCause), RecvTimeoutError> {
        let started = Instant::now();
        let mut remaining = timeout;
        let mut state = self.mailbox.state.lock().recover_poison();
        loop {
            if let Some(event) = state.pop_caused() {
                return Ok(event);
            }
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let (next, waited) =
                self.mailbox.changed.wait_timeout(state, remaining).recover_poison();
            state = next;
            if waited.timed_out() {
                if let Some(event) = state.pop_caused() {
                    return Ok(event);
                }
                if state.closed {
                    return Err(RecvTimeoutError::Disconnected);
                }
                return Err(RecvTimeoutError::Timeout);
            }
            remaining = timeout.saturating_sub(started.elapsed());
        }
    }

    /// The checked wait of a mailbox-owning thread: the next event, waiting
    /// at most `timeout` (`None`: until one arrives or the bus closes). It is
    /// checked as a [`cmux_threads::BlockingKind::Recv`] at the caller's line,
    /// so it is never called on a request or echo thread.
    #[track_caller]
    pub(crate) fn wait_event(
        &self,
        timeout: Option<Duration>,
    ) -> Result<MuxEvent, RecvTimeoutError> {
        cmux_threads::assert_blocking_allowed(cmux_threads::BlockingKind::Recv);
        match timeout {
            // wakeup-allow: deadline the caller's one timeout; an event or the bus closing ends it first
            Some(timeout) => self.recv_timeout(timeout),
            None => self.recv().map_err(|_| RecvTimeoutError::Disconnected),
        }
    }
}

#[cfg(test)]
#[path = "event_bus_encode_tests.rs"]
mod encode_tests;
