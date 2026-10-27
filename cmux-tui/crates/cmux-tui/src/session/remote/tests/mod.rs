//! Tests for the remote session, split by topic. Shared writers and session
//! fixtures live here; each child module holds the tests for one topic.

use std::io::Read;
#[cfg(unix)]
use std::io::{BufRead, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Condvar, Mutex, Weak};

use ghostty_vt::{Callbacks, ColorSpec, KeyAction, Mods, RenderState, Terminal};
use serde_json::json;

use super::*;

mod attach;
mod bootstrap;
mod browser;
mod events;
mod geometry;
mod identity;
#[cfg(debug_assertions)]
mod lock_order;
mod mirror;
mod overflow;
mod refresh;
mod requests;
mod resize;
mod subscriptions;
mod transport;
mod writes;

fn attached_surface(outcome: RemoteSurfaceAttach) -> Arc<RemoteSurface> {
    let RemoteSurfaceAttach::Attached(surface) = outcome else {
        panic!("surface attach did not produce a mirror");
    };
    surface
}

pub(super) struct CloseTrackingWriter {
    pub(super) closed: Arc<AtomicBool>,
}

impl RemoteMessageWriter for CloseTrackingWriter {
    fn send(&mut self, _message: &str) -> io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        self.closed.store(true, Ordering::Release);
        Ok(())
    }
}

struct UnexpectedWriteWriter;

impl RemoteMessageWriter for UnexpectedWriteWriter {
    fn send(&mut self, message: &str) -> io::Result<()> {
        panic!("unexpected remote write: {message}")
    }

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct AcknowledgingWriter {
    session: Arc<Mutex<Option<Weak<RemoteSession>>>>,
    requests: Option<Sender<Value>>,
}

impl RemoteMessageWriter for AcknowledgingWriter {
    fn send(&mut self, message: &str) -> io::Result<()> {
        let request: Value = serde_json::from_str(message).map_err(io::Error::other)?;
        if let Some(requests) = self.requests.as_ref() {
            requests
                .send(request.clone())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "request reader exited"))?;
        }
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| io::Error::other("remote request omitted its id"))?;
        let session = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| io::Error::other("test remote session was dropped"))?;
        let response = session
            .pending
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| io::Error::other("remote request was not pending"))?;
        response
            .response
            .send(json!({"id": id, "ok": true, "data": null}))
            .map_err(|_| io::Error::other("remote response receiver was dropped"))
    }

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RecordingAcknowledgingWriter {
    session: Arc<Mutex<Option<Weak<RemoteSession>>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl RemoteMessageWriter for RecordingAcknowledgingWriter {
    fn send(&mut self, message: &str) -> io::Result<()> {
        let request: Value = serde_json::from_str(message).map_err(io::Error::other)?;
        self.requests.lock().unwrap().push(request.clone());
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| io::Error::other("remote request omitted its id"))?;
        let session = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| io::Error::other("test remote session was dropped"))?;
        let response = session
            .pending
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| io::Error::other("remote request was not pending"))?;
        response
            .response
            .send(json!({"id": id, "ok": true, "data": null}))
            .map_err(|_| io::Error::other("remote response receiver was dropped"))
    }

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn test_session_with_provider_context(
    writer: Box<dyn RemoteMessageWriter>,
    capabilities: HashSet<String>,
    provider_workspace_authority: Option<BearerToken>,
) -> Arc<RemoteSession> {
    test_session_with_abort_and_context(
        writer,
        Arc::new(NoopTransportAbort),
        capabilities,
        provider_workspace_authority,
    )
}

fn test_session_with_abort_and_context(
    writer: Box<dyn RemoteMessageWriter>,
    abort: Arc<dyn RemoteTransportAbort>,
    capabilities: HashSet<String>,
    provider_workspace_authority: Option<BearerToken>,
) -> Arc<RemoteSession> {
    Arc::new(RemoteSession {
        interactive_writer: InteractiveWriter::spawn(writer, abort).unwrap(),
        disconnect_state: disconnect::DisconnectCell::default(),
        pending: RankedMutex::new(
            LockRank::SessionPending,
            "remote_session.pending",
            PendingRemoteRequests::default(),
        ),
        next_id: AtomicU64::new(1),
        attach_progress: AtomicU64::new(0),
        shutdown: AtomicBool::new(false),
        surfaces: RankedMutex::new(
            LockRank::SessionSurfaces,
            "remote_session.surfaces",
            HashMap::new(),
        ),
        exited_surfaces: RankedMutex::new(
            LockRank::SessionExitedSurfaces,
            "remote_session.exited_surfaces",
            ExitedSurfaceState::default(),
        ),
        surface_leases: RankedMutex::new(
            LockRank::SessionSurfaceLeases,
            "remote_session.surface_leases",
            HashMap::new(),
        ),
        retired_surfaces: RankedMutex::new(
            LockRank::SessionRetiredSurfaces,
            "remote_session.retired_surfaces",
            HashSet::new(),
        ),
        retire_surface_test_marker: RankedMutex::new(
            LockRank::Leaf,
            "remote_session.retire_surface_test_marker",
            None,
        ),
        tree: RankedMutex::new(
            LockRank::SessionTree,
            "remote_session.tree",
            RemoteTreeCache::default(),
        ),
        browser_sources: RankedMutex::new(
            LockRank::SessionBrowserSources,
            "remote_session.browser_sources",
            HashMap::new(),
        ),
        tree_refresh: RankedMutex::new(
            LockRank::SessionTreeRefresh,
            "remote_session.tree_refresh",
            (),
        ),
        tree_stale: AtomicBool::new(true),
        subscription_started: AtomicBool::new(false),
        event_surface_filter: AtomicU64::new(0),
        subscription_recovery: RankedMutex::new(
            LockRank::SessionSubscriptionRecovery,
            "remote_session.subscription_recovery",
            SubscriptionRecoveryState::default(),
        ),
        subscribers: MuxEventBroadcaster::default(),
        primed_subscription: RankedMutex::new(
            LockRank::SessionPrimedSubscription,
            "remote_session.primed_subscription",
            None,
        ),
        frame_dump_dir: None,
        frame_logs: RankedMutex::new(
            LockRank::SessionFrameLogs,
            "remote_session.frame_logs",
            RemoteFrameLogs::default(),
        ),
        surface_overflow_recovery: RankedMutex::new(
            LockRank::SessionSurfaceOverflowRecovery,
            "remote_session.surface_overflow_recovery",
            HashMap::new(),
        ),
        surface_overflow_reconnect_required: AtomicBool::new(false),
        cell_pixel_lifecycle: RankedMutex::new(
            LockRank::SessionCellPixelLifecycle,
            "remote_session.cell_pixel_lifecycle",
            (),
        ),
        cell_pixels: RankedMutex::new(
            LockRank::SessionCellPixels,
            "remote_session.cell_pixels",
            (8, 16),
        ),
        capabilities: RankedMutex::new(
            LockRank::SessionCapabilities,
            "remote_session.capabilities",
            capabilities,
        ),
        size_states: RankedMutex::new(
            LockRank::SessionSizeStates,
            "remote_session.size_states",
            HashMap::new(),
        ),
        provider_workspace_authority,
        provider_workspaces_guarded: AtomicBool::new(false),
        sidebar_doc: sidebar_doc::RemoteSidebarDoc::default(),
    })
}

pub(super) fn test_session(writer: Box<dyn RemoteMessageWriter>) -> Arc<RemoteSession> {
    test_session_with_provider_context(writer, HashSet::new(), None)
}

fn test_remote_pty_surface(
    id: SurfaceId,
    cols: u16,
    rows: u16,
    cell_pixels: (u16, u16),
) -> Arc<RemoteSurface> {
    let mut term = Terminal::new(cols, rows, 100, Callbacks::default()).unwrap();
    term.resize(cols, rows, u32::from(cell_pixels.0), u32::from(cell_pixels.1)).unwrap();
    Arc::new(RemoteSurface::mirror(
        id,
        SurfaceKind::Pty,
        term,
        MouseEncoders::new().unwrap(),
        cell_pixels,
    ))
}

struct RejectingWriter {
    session: Arc<Mutex<Option<Weak<RemoteSession>>>>,
}

impl RemoteMessageWriter for RejectingWriter {
    fn send(&mut self, message: &str) -> io::Result<()> {
        let request: Value = serde_json::from_str(message).map_err(io::Error::other)?;
        let id = request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| io::Error::other("remote request omitted its id"))?;
        let session = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| io::Error::other("test remote session was dropped"))?;
        let response = session
            .pending
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| io::Error::other("remote request was not pending"))?;
        response
            .response
            .send(json!({"id": id, "ok": false, "error": "injected rejection"}))
            .map_err(|_| io::Error::other("remote response receiver was dropped"))
    }

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct SilentWriter;

impl RemoteMessageWriter for SilentWriter {
    fn send(&mut self, _message: &str) -> io::Result<()> {
        Ok(())
    }

    fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn socket_test_session(stream: UnixStream) -> Arc<RemoteSession> {
    stream.set_write_timeout(Some(remote_write_timeout())).unwrap();
    test_session(Box::new(JsonLineWriter { inner: Box::new(stream) }))
}

fn test_remote_surface(id: SurfaceId) -> Arc<RemoteSurface> {
    Arc::new(RemoteSurface::mirror(
        id,
        SurfaceKind::Pty,
        Terminal::new(80, 24, 100, Callbacks::default()).unwrap(),
        MouseEncoders::new().unwrap(),
        (8, 16),
    ))
}
