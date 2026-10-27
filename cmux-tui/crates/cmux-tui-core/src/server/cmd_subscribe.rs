//! Event subscription command handlers: subscribe (tree events and surface
//! output streams), a streamed page relay's `subscribe {stream}` and
//! subscribe-activity, plus the JSON for every subscribed mux event and the
//! overflow notice. Each handler is one `Command` arm of
//! `handle_command_with_cancellation`, except `subscribe {stream}`, a
//! command of a streamed relay's stream 0 (client_hello.rs).
//!
//! Encode once: a subscription takes each event with the encodings slot the
//! bus made for that emit (event_bus.rs `EventEncodings`). Subscribers whose
//! line of the event depends on the same inputs (`EncodeClass`) share one
//! serialization; only what one connection may hear (its admission) and a
//! per-reader event (`size-state`) are decided per subscriber.

use super::MessageWriter;
use super::OPEN_DEVICE_KINDS_CAPABILITY;
use super::OutboundStream;
use super::SHARED_SIZING_CAPABILITY;
use super::SizeStateReader;
use super::attach_streams::ClientStreamId;
use super::connection_scheduler::REQUEST_THREAD_QOS;
use super::machine_usage_json;
use super::size_state_event_json;
use super::tree_delta_json;
use crate::GraphicsStatus;
use crate::Mux;
use crate::MuxEvent;
use crate::MuxEventReceiver;
use crate::SurfaceId;
use crate::event_bus::DeliveredEvent;
use crate::event_bus::EncodeClass;
use crate::event_bus::EncodedEvent;
use crate::event_bus::MuxEventKind;
use crate::event_bus::MuxEventKinds;
use crate::event_cause::EventCause;
use crate::panic_free::JsonFields;
use crate::stream_interrupt::StreamInterrupt;
use serde_json::Value;
use serde_json::json;
use std::num::NonZeroU16;
use std::sync::Arc;

pub(super) fn subscribe_activity(
    mux: &Arc<Mux>,
    client: u64,
    writer: &MessageWriter,
) -> anyhow::Result<Value> {
    if !mux.control_clients.is_unix(client) {
        anyhow::bail!("subscribe-activity requires a trusted local connection");
    }
    mux.activity.subscribe(mux, client, writer)
}

pub(super) fn subscribe_command(
    mux: &Arc<Mux>,
    client: u64,
    writer: &MessageWriter,
    tree_events: Option<String>,
    surface: Option<SurfaceId>,
) -> anyhow::Result<Value> {
    let tree_deltas = match tree_events.as_deref().unwrap_or("coarse") {
        "coarse" => false,
        "deltas" => true,
        other => anyhow::bail!("bad request: unsupported tree_events {other:?}"),
    };
    // A page relay hears only its page's events (page-router.json `events`;
    // client_hello admits its `subscribe` only when there are some). Their
    // kinds filter its stream on the bus, so the others (a busy terminal's
    // output) are never queued for it nor rendered to JSON.
    let page_events = super::origin_gate::page_relay_events(mux, client);
    let subscriber = Subscriber::new(mux, client, page_events, tree_deltas);
    let events = match (surface, subscriber.interest) {
        (Some(surface), _) => mux
            .subscribe_surface_session(surface)
            .ok_or_else(|| anyhow::anyhow!("unknown surface {surface}"))?,
        (None, Some(kinds)) => mux.subscribe_kinds(kinds),
        (None, None) => mux.subscribe(),
    };
    // Its events share the reply FIFO (spec/transports.md "Reply and event order").
    let outbound = writer.start_control_stream(&subscription_overflow_json())?;
    subscriber.spawn(writer, events, outbound)?;
    Ok(json!({}))
}

/// Why a streamed page relay's `subscribe {stream}` was refused.
#[derive(Debug)]
pub(super) enum PageStreamSubscribeError {
    /// The stream is not open on this connection.
    NotOpen,
    /// The stream's page hears no events (page-router.json `events`).
    NoEvents,
    /// The subscription's stream or thread could not start.
    Start(std::io::Error),
}

/// `subscribe {stream}` on a streamed page relay (`page-streams-v1`): page
/// stream `stream` hears its page's events, each line tagged
/// `"stream": stream`, until `close-stream`, a later `subscribe` of the same
/// stream, or the connection ends. Its subscriber is the page relay's, with
/// the stream's page (request-origin.md "Page streams"). Each page is an
/// endpoint of one connection, as Mojo's associated interfaces are of one
/// message pipe (mojo/public/cpp/bindings/README.md "Associated Interfaces").
pub(super) fn subscribe_page_stream(
    mux: &Arc<Mux>,
    client: u64,
    writer: &MessageWriter,
    stream: NonZeroU16,
) -> Result<(), PageStreamSubscribeError> {
    let tag = ClientStreamId(stream.get());
    let outbound = writer
        .start_tagged_control_stream(tag, &subscription_overflow_json())
        .map_err(PageStreamSubscribeError::Start)?;
    // Ending the subscription closes its stream before the reply that
    // ends it is queued, so no line of it follows that reply.
    let ended = StreamInterrupt::new();
    let closing = outbound.clone();
    ended.on_fire(move || closing.close_by_client());
    let page_events = super::origin_gate::subscribe_page_stream(mux, client, stream, ended)?;
    let subscriber = Subscriber::new(mux, client, Some(page_events), false);
    let events = match subscriber.interest {
        Some(kinds) => mux.subscribe_kinds(kinds),
        None => mux.subscribe(),
    };
    subscriber.spawn(writer, events, outbound).map_err(PageStreamSubscribeError::Start)
}

/// One subscription's settings, read once when it starts.
struct Subscriber {
    mux: Arc<Mux>,
    client: u64,
    /// The names a page relay hears, or `None` for any other connection.
    page_events: Option<&'static [String]>,
    /// The page's names as bus kinds, when every name has one.
    interest: Option<MuxEventKinds>,
    tree_deltas: bool,
    trusted_pairing_client: bool,
    /// Conversation events go to the person's own clients: the Unix socket
    /// and authenticated WebSocket connections (`ws-conversations-v1`).
    conversation_client: bool,
    /// `event-cause-v1`, echoed in set-client-info before this subscribe:
    /// each event says what set it off (spec/events.md "Event cause").
    caused_events: bool,
    /// `diagnostic-sites-v1`: the daemon's diagnostic sites, to the person's
    /// own local app only.
    diagnostic_client: bool,
    #[cfg(test)]
    probe: Arc<SubscribeProbe>,
}

/// How one subscriber writes one event.
enum Admission {
    /// The connection does not hear it.
    Skip,
    /// The line depends only on the event and the [`EncodeClass`].
    Shared,
    /// The line depends on the reader (`size-state`).
    Own,
}

impl Subscriber {
    fn new(
        mux: &Arc<Mux>,
        client: u64,
        page_events: Option<&'static [String]>,
        tree_deltas: bool,
    ) -> Self {
        let clients = &mux.control_clients;
        let trusted_pairing_client = clients.is_unix(client);
        Self {
            mux: mux.clone(),
            client,
            page_events,
            interest: page_events.and_then(page_event_kinds),
            // Its events are never tree deltas: render none only to drop them.
            tree_deltas: tree_deltas && page_events.is_none(),
            trusted_pairing_client,
            conversation_client: clients.is_owner_client(client),
            caused_events: clients.supports_capability(client, crate::event_cause::CAPABILITY),
            diagnostic_client: trusted_pairing_client
                && page_events.is_none()
                && clients.supports_capability(client, super::DIAGNOSTIC_SITES_CAPABILITY),
            #[cfg(test)]
            probe: clients.subscribe_probe(client),
        }
    }

    fn supports(&self, capability: &str) -> bool {
        self.mux.control_clients.supports_capability(self.client, capability)
    }

    /// Starts the thread that writes `events` to `outbound`.
    fn spawn(
        self,
        writer: &MessageWriter,
        events: MuxEventReceiver,
        outbound: OutboundStream,
    ) -> std::io::Result<()> {
        events.share_encodings();
        let pending_pairings = if self.trusted_pairing_client && self.page_events.is_none() {
            self.mux.pending_pairings()
        } else {
            Vec::new()
        };
        // Read after `events` subscribed, so a site noted in between comes
        // twice rather than never (the app keeps one).
        let seen_sites =
            if self.diagnostic_client { self.mux.diagnostic_sites.snapshot() } else { Vec::new() };
        let writer = writer.clone();
        cmux_threads::spawn("mux-events-out", REQUEST_THREAD_QOS, move || {
            let pairings = pending_pairings
                .into_iter()
                .map(|challenge| subscribed_event_json(&MuxEvent::PairingRequested(challenge)));
            let sites = seen_sites
                .into_iter()
                .map(|site| subscribed_event_json(&MuxEvent::DiagnosticSite(site)));
            let mut sent = Ok(());
            for value in pairings.chain(sites) {
                sent = writer.send_stream_backpressured(&value, &outbound);
                if sent.is_err() {
                    break;
                }
            }
            if sent.is_ok() {
                sent = self.forward(&writer, &events, &outbound);
            }
            let transport_overflow =
                sent.is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock);
            if events.overflowed() || transport_overflow {
                let _ = writer.send_terminal(&subscription_overflow_json(), &outbound);
            }
        })?;
        Ok(())
    }

    /// Writes each event of `events` this connection hears until the
    /// connection, the stream or the bus ends.
    fn forward(
        &self,
        writer: &MessageWriter,
        events: &MuxEventReceiver,
        outbound: &OutboundStream,
    ) -> std::io::Result<()> {
        let interrupt = StreamInterrupt::new();
        writer.register_interrupt(&interrupt);
        outbound.register_interrupt(&interrupt);
        events.wake_on(&interrupt);
        while writer.is_open() && outbound.is_open() {
            let delivered = match events.recv_delivered_until_interrupted(&interrupt) {
                Ok(delivered) => delivered,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            self.write(writer, &delivered, outbound)?;
        }
        Ok(())
    }

    fn write(
        &self,
        writer: &MessageWriter,
        delivered: &DeliveredEvent,
        outbound: &OutboundStream,
    ) -> std::io::Result<()> {
        let event = &delivered.event;
        #[cfg(test)]
        self.probe.received.note(event);
        // A surface stream is not filtered on the bus: drop an unwanted
        // kind before any JSON is built.
        if self.interest.is_some_and(|kinds| !kinds.accepts(event)) {
            return Ok(());
        }
        #[cfg(test)]
        self.probe.rendered.note(event);
        // `cause` is left out for a program cause: a missing field means it.
        let cause = (self.caused_events && delivered.cause != EventCause::Program)
            .then_some(delivered.cause);
        match self.admission(event) {
            Admission::Skip => Ok(()),
            Admission::Own => {
                let mut value = self.own_event_json(event);
                if !self.hears(value.field("event").as_str().unwrap_or_default()) {
                    return Ok(());
                }
                if let Some(cause) = cause {
                    stamp_cause(&mut value, cause);
                }
                writer.send_stream_backpressured(&value, outbound)
            }
            Admission::Shared => {
                let tree_deltas = self.tree_deltas
                    && matches!(event, MuxEvent::TreeDelta(_) | MuxEvent::TreeSelectionChanged);
                let class = EncodeClass { tree_deltas, cause };
                let encode = || {
                    #[cfg(test)]
                    self.probe.encoded.note(event);
                    let mut value = shared_event_json(event, tree_deltas, &self.mux);
                    if let Some(cause) = cause {
                        stamp_cause(&mut value, cause);
                    }
                    encoded_event(&value)
                };
                let fresh;
                let encoded = match &delivered.encodings {
                    Some(encodings) => encodings.get_or_encode(class, encode),
                    None => {
                        fresh = encode().map(Arc::new);
                        fresh.as_ref()
                    }
                };
                match encoded {
                    Some(encoded) if self.hears(&encoded.name) => {
                        writer.send_json_backpressured(&encoded.json, outbound)
                    }
                    _ => Ok(()),
                }
            }
        }
    }

    /// Whether a page relay's page hears the event `name` (any other
    /// connection hears every name it is admitted to).
    fn hears(&self, name: &str) -> bool {
        self.page_events.is_none_or(|events| events.iter().any(|event| event == name))
    }

    /// Whether this connection hears `event`, and whether its line is the
    /// same for every subscriber of its [`EncodeClass`].
    fn admission(&self, event: &MuxEvent) -> Admission {
        let heard = match event {
            MuxEvent::PairingRequested(_) | MuxEvent::PairingResolved { .. } => {
                self.trusted_pairing_client
            }
            MuxEvent::Conversation(event) => self.hears_conversation(event),
            MuxEvent::CloudConversation(event) => self.hears_cloud_conversation(event),
            MuxEvent::DiagnosticSite(_) => self.diagnostic_client,
            MuxEvent::TreeSelectionChanged => self.tree_deltas,
            MuxEvent::SizeStateChanged { .. } if self.supports(SHARED_SIZING_CAPABILITY) => {
                return Admission::Own;
            }
            MuxEvent::SizeStateChanged { .. } => false,
            _ => true,
        };
        if heard { Admission::Shared } else { Admission::Skip }
    }

    fn hears_conversation(&self, event: &crate::conversation_store::ConversationEvent) -> bool {
        use super::chief_registry_resource::CHIEF_EVENTS_CAPABILITY;
        use super::conversation_live::CHIEF_LIVE_CAPABILITY;
        use super::conversation_runs::CONVERSATION_RUNS_CAPABILITY;
        use super::conversations::CONVERSATION_DRAFT_EVENTS_CAPABILITY;
        use super::participant_avatars::PARTICIPANT_AVATARS_CAPABILITY;
        use crate::conversation_store::{ConversationEvent, LOCAL_USER};
        if !self.conversation_client {
            return false;
        }
        // A draft goes only to a connection that asked for drafts in
        // set-client-info (the admission of `conversation.events`; old
        // clients never see the event).
        if event.is_draft() {
            return self.supports(CONVERSATION_DRAFT_EVENTS_CAPABILITY)
                && matches!(event, ConversationEvent::Draft { .. });
        }
        // A participant's own list state (a pin, a "marked unread") goes
        // only to connections that act as that participant.
        if let ConversationEvent::Inbox { participant, .. } = event
            && *participant != self.mux.conversation_principal(self.client)
        {
            return false;
        }
        // A run's steps only with `conversation-runs-v1` (they can be large),
        // a Chief's live frames only with `chief-live-v1`, an avatar change
        // only with `participant-avatars-v1`.
        (!event.is_run() || self.supports(CONVERSATION_RUNS_CAPABILITY))
            && (!event.is_live() || self.supports(CHIEF_LIVE_CAPABILITY))
            && (!event.is_avatar() || self.supports(PARTICIPANT_AVATARS_CAPABILITY))
            // A Chief change only to the person's own connection that asked
            // for it (`chief-events-v1`): never to a bound agent, which
            // chief.profile.list may refuse.
            && (!event.is_chief()
                || (self.mux.conversation_principal(self.client) == LOCAL_USER
                    && self.supports(CHIEF_EVENTS_CAPABILITY)))
    }

    fn hears_cloud_conversation(&self, event: &crate::cloud_conversations::CloudEvent) -> bool {
        use super::cloud_conversations::CLOUD_TYPING_CAPABILITY;
        use super::conversation_live::CHIEF_LIVE_CAPABILITY;
        use super::conversation_runs::CONVERSATION_RUNS_CAPABILITY;
        use crate::cloud_conversations::CloudEvent;
        // The host replica's renewal only to the verified app: it holds the
        // install token.
        if host_replica_needed(event) {
            return self.conversation_client
                && matches!(
                    super::origin_gate::connection_actor(&self.mux, self.client),
                    crate::workspace_registry::Actor::Frontend { .. }
                );
        }
        // The person's own clients: Unix, or an authenticated page
        // (`ws-cloud-conversations-v1`); `delivers` narrows it to the lease's.
        self.conversation_client
            && super::cloud_conversations::delivers(&self.mux, self.client)
            && match event {
                // A Cloud Chief's run change, like a local one, only with
                // `conversation-runs-v1`; a live frame only with
                // `chief-live-v1`; typing only with
                // `cloud-conversation-typing-v1`.
                CloudEvent::RunChanged { .. } => self.supports(CONVERSATION_RUNS_CAPABILITY),
                CloudEvent::Live { .. } => self.supports(CHIEF_LIVE_CAPABILITY),
                CloudEvent::Typing { .. } => self.supports(CLOUD_TYPING_CAPABILITY),
                _ => true,
            }
    }

    /// The line of an [`Admission::Own`] event, for this reader.
    fn own_event_json(&self, event: &MuxEvent) -> Value {
        match event {
            MuxEvent::SizeStateChanged { surface, runtime, state } => {
                let open = self.supports(OPEN_DEVICE_KINDS_CAPABILITY);
                let reader = SizeStateReader {
                    client: self.client,
                    open,
                    scheme: self.mux.participant_scheme(),
                };
                size_state_event_json(*surface, *runtime, state, Some(reader))
            }
            event => subscribed_event_json(event),
        }
    }
}

/// The line of an [`Admission::Shared`] event, the same for every
/// subscriber with these `tree_deltas` (before its `cause`).
fn shared_event_json(event: &MuxEvent, tree_deltas: bool, mux: &Mux) -> Value {
    match event {
        MuxEvent::Conversation(event) => match event.as_ref() {
            crate::conversation_store::ConversationEvent::Draft {
                conversation,
                participant,
                item,
            } => json!({
                "event": "conversation-draft",
                "conversation": conversation,
                "participant": participant,
                "item": item,
            }),
            event => event.wire_json(),
        },
        MuxEvent::TreeDelta(delta) if tree_deltas => tree_delta_json(delta, mux),
        event => subscribed_event_json(event),
    }
}

/// `value` serialized once, with its `event` name for page filters.
fn encoded_event(value: &Value) -> Option<EncodedEvent> {
    let json = serde_json::to_vec(value).ok()?;
    let name = value.field("event").as_str().unwrap_or_default().into();
    Some(EncodedEvent { name, json: json.into() })
}

/// What one connection's subscription took from the bus and what it
/// started to render to JSON, per event kind: tests read it to see which
/// events a filter kept out of a connection's stream.
#[cfg(test)]
#[derive(Default)]
pub(super) struct SubscribeProbe {
    pub(super) received: KindCounts,
    pub(super) rendered: KindCounts,
    /// Events this connection's subscriptions serialized: one per event per
    /// encode class among all subscribers of the class.
    pub(super) encoded: KindCounts,
}

#[cfg(test)]
pub(super) struct KindCounts([std::sync::atomic::AtomicU64; MuxEventKind::COUNT]);

#[cfg(test)]
impl Default for KindCounts {
    fn default() -> Self {
        Self(std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)))
    }
}

#[cfg(test)]
impl KindCounts {
    fn note(&self, event: &MuxEvent) {
        self.0[MuxEventKind::of(event) as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(super) fn of(&self, kind: MuxEventKind) -> u64 {
        self.0[kind as usize].load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The kinds a page relay hearing the subscribe events `names` needs, or
/// `None` when a name is one this map does not know (the stream is then
/// filtered by name alone, as before).
pub(super) fn page_event_kinds(names: &[String]) -> Option<MuxEventKinds> {
    names.iter().try_fold(MuxEventKinds::NONE, |kinds, name| {
        wire_event_kinds(name).map(|more| kinds.union(more))
    })
}

/// The event kinds that can render as the subscribe event `name`
/// ([`subscribed_event_json`] and the arms of [`subscribe_command`]). Tree
/// delta names are left out: a page relay never hears deltas.
pub(super) fn wire_event_kinds(name: &str) -> Option<MuxEventKinds> {
    use MuxEventKind as Kind;
    let kind = match name {
        "surface-output" => Kind::SurfaceOutput,
        "surface-resized" => Kind::SurfaceResized,
        "surface-resize-failed" => Kind::SurfaceResizeFailed,
        "surface-exited" => Kind::SurfaceExited,
        "size-state" => Kind::SizeStateChanged,
        "title-changed" => Kind::TitleChanged,
        "agent-changed" => Kind::AgentChanged,
        "agent-presence-changed" => Kind::AgentPresenceChanged,
        "bell" => Kind::Bell,
        "program-status-changed" => Kind::ProgramStatusChanged,
        "notification" => Kind::Notification,
        "graphics-status" => Kind::GraphicsStatus,
        "status" => Kind::Status,
        "machine-usage-changed" => Kind::MachineUsageChanged,
        "server-diagnostic-site" => Kind::DiagnosticSite,
        "config-reload-requested" => Kind::ConfigReloadRequested,
        "window-title-requested" => Kind::WindowTitleRequested,
        "scroll-changed" => Kind::ScrollChanged,
        "tree-changed" => {
            return Some(
                MuxEventKinds::NONE
                    .with(Kind::TreeChanged)
                    .with(Kind::TreeSelectionChanged)
                    .with(Kind::TreeDelta),
            );
        }
        "frontend-projection-changed" => Kind::FrontendProjectionChanged,
        "personal-changed" => Kind::PersonalChanged,
        "bookmarks-changed" => Kind::BookmarksChanged,
        "history-changed" => Kind::HistoryChanged,
        "settings-changed" => Kind::SettingsChanged,
        "keybindings-changed" => Kind::KeybindingsChanged,
        "terminal-registry-changed" => Kind::TerminalRegistryChanged,
        "terminal-reaped" => Kind::TerminalReaped,
        "layout-changed" => Kind::LayoutChanged,
        "client-attached" => Kind::ClientAttached,
        "client-changed" => Kind::ClientChanged,
        "client-detached" => Kind::ClientDetached,
        "client-list-invalidated" => Kind::ClientListInvalidated,
        "pairing-requested" => Kind::PairingRequested,
        "pairing-resolved" => Kind::PairingResolved,
        "empty" => Kind::Empty,
        // ConversationEvent::wire_json and the draft arm above.
        "chief-changed" | "participant-avatar-changed" => Kind::Conversation,
        name if name.starts_with("conversation-") => Kind::Conversation,
        // CloudEvent::wire_json.
        name if name.starts_with("cloud-") => Kind::CloudConversation,
        _ => return None,
    };
    Some(MuxEventKinds::NONE.with(kind))
}

pub(super) fn subscribed_event_json(event: &MuxEvent) -> Value {
    match event {
        MuxEvent::SurfaceOutput(id) => json!({"event": "surface-output", "surface": id}),
        MuxEvent::SurfaceResized { surface, cols, rows, reservation_id } => json!({
            "event": "surface-resized",
            "surface": surface,
            "cols": cols,
            "rows": rows,
            "reservation_id": reservation_id,
        }),
        MuxEvent::SurfaceResizeFailed {
            surface,
            cols,
            rows,
            error,
            retry_after_ms,
            reservation_id,
        } => json!({
            "event": "surface-resize-failed",
            "surface": surface,
            "cols": cols,
            "rows": rows,
            "error": error.as_ref(),
            "retry_after_ms": retry_after_ms,
            "reservation_id": reservation_id,
        }),
        MuxEvent::SurfaceExited(id, end) => {
            json!({"event": "surface-exited", "surface": id, "end": end.as_deref()})
        }
        MuxEvent::SizeStateChanged { surface, runtime, state } => {
            size_state_event_json(*surface, *runtime, state, None)
        }
        MuxEvent::TitleChanged { surface, title } => {
            json!({"event": "title-changed", "surface": surface, "title": title.as_ref()})
        }
        MuxEvent::AgentChanged {
            surface,
            state,
            source,
            session,
            agent,
            agent_session_id,
            updated_at_ms,
            since_ms,
            last_prompt,
        } => json!({
            "event": "agent-changed",
            "surface": surface,
            "state": state.as_ref(),
            "source": source.as_ref(),
            "session": session.as_deref(),
            "agent": agent.as_deref(),
            "agent_session_id": agent_session_id.as_deref(),
            "updated_at_ms": updated_at_ms,
            "since_ms": since_ms,
            "last_prompt": last_prompt.as_deref(),
        }),
        MuxEvent::AgentPresenceChanged { surface, presence } => {
            let presence = presence.as_deref();
            json!({
                "event": "agent-presence-changed",
                "surface": surface,
                "activity": presence.map(|presence| presence.activity.as_str()),
                "agent": presence.and_then(|presence| presence.agent.as_deref()),
                "session": presence.and_then(|presence| presence.session.as_deref()),
                "tool": presence.and_then(|presence| presence.tool.as_deref()),
                "path": presence.and_then(|presence| presence.path.as_deref()),
                "line": presence.and_then(|presence| presence.line),
                "updated_at_ms": presence.map(|presence| presence.updated_at_ms),
            })
        }
        MuxEvent::Bell(id) => json!({"event": "bell", "surface": id}),
        MuxEvent::ProgramStatusChanged { surface, status } => json!({
            "event": "program-status-changed",
            "surface": surface,
            "program_status": status.map(|status| status.wire()),
        }),
        MuxEvent::Notification(notification) => json!({
            "event": "notification",
            "notification": notification.notification,
            "title": notification.title,
            "subtitle": notification.subtitle,
            "body": notification.body,
            "preview": notification.preview,
            "id": notification.id,
            "message": notification.message,
            "prompt": notification.prompt,
            "session": notification.session,
            "question": notification.question.as_deref(),
            "level": notification.level.as_str(),
            "surface": notification.surface,
            "workspace_id": notification.workspace_id,
            "source": notification.source.as_str(),
            "desktop": notification.desktop,
            "reply": notification.reply,
        }),
        MuxEvent::GraphicsStatus(status) => match status {
            GraphicsStatus::KittyImageBudgetWorkerStartFailed { error } => json!({
                "event": "graphics-status",
                "kind": "kitty-image-budget-worker-start-failed",
                "error": error.as_ref(),
            }),
            GraphicsStatus::KittyImageBudgetUpdateFailed { retry_exhausted, summary } => json!({
                "event": "graphics-status",
                "kind": "kitty-image-budget-update-failed",
                "retry_exhausted": retry_exhausted,
                "summary": summary.as_ref(),
            }),
            GraphicsStatus::CellPixelUpdateRetriesExhausted {
                attempts,
                remaining,
                cell_pixels,
            } => json!({
                "event": "graphics-status",
                "kind": "cell-pixel-update-retries-exhausted",
                "attempts": attempts,
                "remaining": remaining,
                "cell_width": cell_pixels.0,
                "cell_height": cell_pixels.1,
            }),
        },
        MuxEvent::Status(message) => json!({"event": "status", "message": message}),
        MuxEvent::MachineUsageChanged(usage) => {
            let mut payload = machine_usage_json(usage.as_ref());
            payload.insert("event", json!("machine-usage-changed"));
            payload
        }
        MuxEvent::DiagnosticSite(site) => {
            let mut payload = site.fields();
            payload.insert("event", json!("server-diagnostic-site"));
            payload
        }
        MuxEvent::ConfigReloadRequested => json!({"event": "config-reload-requested"}),
        MuxEvent::WindowTitleRequested(title) => {
            json!({"event": "window-title-requested", "title": title})
        }
        MuxEvent::ScrollChanged { surface, offset, at_bottom } => json!({
            "event": "scroll-changed",
            "surface": surface,
            "offset": offset,
            "at_bottom": at_bottom,
        }),
        MuxEvent::TreeChanged => json!({"event": "tree-changed"}),
        MuxEvent::TreeSelectionChanged => json!({"event": "tree-changed"}),
        MuxEvent::TreeDelta(_) => json!({"event": "tree-changed"}),
        MuxEvent::FrontendProjectionChanged {
            frontend,
            scope,
            subject_key,
            projection_revision,
            origin,
            mutation_id,
        } => json!({
            "event": "frontend-projection-changed",
            "frontend": frontend,
            "scope": scope,
            "subject_key": subject_key,
            "projection_revision": projection_revision,
            "origin": origin,
            "mutation_id": mutation_id,
        }),
        MuxEvent::PersonalChanged { personal_revision } => json!({
            "event": "personal-changed",
            "personal_revision": personal_revision,
        }),
        MuxEvent::Conversation(event) => event.wire_json(),
        MuxEvent::CloudConversation(event) => event.wire_json(),
        MuxEvent::BookmarksChanged(change) => json!({
            "event": "bookmarks-changed",
            "browser_profile_id": change.browser_profile_id,
            "bookmarks_revision": change.bookmarks_revision,
        }),
        MuxEvent::HistoryChanged { revision, kinds } => json!({
            "event": "history-changed",
            "revision": revision,
            "kinds": kinds,
        }),
        MuxEvent::SettingsChanged(change) => json!({
            "event": "settings-changed",
            "revision": change.revision,
            "keys": change.keys,
            "origin": change.origin.as_str(),
        }),
        MuxEvent::KeybindingsChanged(change) => json!({
            "event": "keybindings-changed",
            "revision": change.revision,
            "origin": change.origin.as_str(),
        }),
        MuxEvent::TerminalRegistryChanged { registry_id, generation, terminal_revision } => json!({
            "event":"terminal-registry-changed",
            "registry_id":registry_id,
            "generation":generation,
            "terminal_revision":terminal_revision,
            "refetch":"terminal-events-or-list-terminals",
        }),
        MuxEvent::TerminalReaped { terminal_id, terminal, grace_ms } => json!({
            "event": "terminal-reaped",
            "terminal_id": terminal_id,
            "terminal": terminal,
            "grace_ms": grace_ms,
        }),
        MuxEvent::LayoutChanged(screen, delta) => {
            let mut value = json!({"event": "layout-changed", "screen": screen});
            // `layout-delta-v1`: the screen's layout fields, flat, as in `Screen`.
            if let Some(delta) = delta {
                value.insert("workspace", json!(delta.workspace));
                if let (Value::Object(value), Value::Object(layout)) = (&mut value, &delta.layout) {
                    value.extend(layout.iter().map(|(key, field)| (key.clone(), field.clone())));
                }
                if let Some(transaction) = &delta.transaction {
                    value.insert("transaction", json!(transaction.as_ref()));
                }
            }
            value
        }
        MuxEvent::ClientAttached { client, transport, name, kind } => json!({
            "event": "client-attached",
            "client": client,
            "transport": transport,
            "name": name,
            "kind": kind,
        }),
        MuxEvent::ClientChanged { client, name, kind } => json!({
            "event": "client-changed",
            "client": client,
            "name": name,
            "kind": kind,
        }),
        MuxEvent::ClientDetached(client) => {
            json!({"event": "client-detached", "client": client})
        }
        MuxEvent::ClientListInvalidated => json!({"event": "client-list-invalidated"}),
        MuxEvent::PairingRequested(challenge) => json!({
            "event": "pairing-requested",
            "request": challenge.id,
            "code": challenge.code,
            "peer": challenge.peer,
            "expires_in": challenge.expires_in,
        }),
        MuxEvent::PairingResolved { request } => {
            json!({"event": "pairing-resolved", "request": request})
        }
        MuxEvent::Empty => json!({"event": "empty"}),
    }
}

/// Adds `cause` to an event line for an `event-cause-v1` subscriber. A
/// program cause is left out: a missing field means it.
pub(super) fn stamp_cause(value: &mut Value, cause: EventCause) {
    if cause == EventCause::Program {
        return;
    }
    if let Some(fields) = value.as_object_mut() {
        fields.insert("cause".to_string(), Value::from(cause.wire_name()));
    }
}

pub(super) fn subscription_overflow_json() -> Value {
    json!({
        "event": "overflow",
        "error": "subscriber fell behind; resubscribe to continue receiving events",
    })
}

fn host_replica_needed(event: &crate::cloud_conversations::CloudEvent) -> bool {
    matches!(event, crate::cloud_conversations::CloudEvent::HostReplicaNeeded { .. })
}

#[cfg(all(test, unix))]
#[path = "cmd_subscribe_tests.rs"]
mod tests;
