//! The daemon-owned embedded Tailscale node.
//!
//! This controller owns the node lifecycle and its event-driven state.  The
//! ingress server is deliberately separate: it receives the same `Arc<Node>`
//! and the daemon's `Arc<Hub>`, so ticket minting and redemption remain in one
//! acpmux pairing store.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cmux_tailnet::{BackendState, Node, NodeConfig, Notify};
use serde_json::{Value, json};
use tokio::sync::broadcast;

/// A snapshot reported by the embedded node's IPN bus.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub state: &'static str,
    pub browse_to_url: Option<String>,
    pub dns_name: Option<String>,
    pub ips: Vec<String>,
    pub error: Option<String>,
}

impl Snapshot {
    /// The sign-in URL is only included for an explicitly person-authorized
    /// local request. Ingress and all remote callers use `to_json(false)`.
    pub fn to_json(&self, include_sign_in_url: bool) -> Value {
        let mut value = json!({"state": self.state});
        if include_sign_in_url
            && self.state == "needs_login"
            && let Some(url) = &self.browse_to_url
        {
            value["browse_to_url"] = json!(url);
        }
        if let Some(name) = &self.dns_name {
            value["dns_name"] = json!(name);
        }
        if !self.ips.is_empty() {
            value["ips"] = json!(self.ips);
        }
        if let Some(error) = &self.error {
            value["error"] = json!(error);
        }
        value
    }

    pub fn running(&self) -> bool {
        self.state == "running"
    }

    fn apply_notify(&mut self, notify: &Notify, node: Option<&Node>) {
        if let Some(url) = notify.browse_to_url.as_deref().filter(|url| !url.is_empty()) {
            self.browse_to_url = Some(url.to_owned());
        }
        if let Some(error) = notify.err_message.as_deref().filter(|error| !error.is_empty()) {
            self.error = Some(error.to_owned());
        }
        if let Some(state) = notify.backend_state() {
            self.state = state_name(state);
            if state != BackendState::NeedsLogin {
                self.browse_to_url = None;
            }
            if state != BackendState::Running {
                self.dns_name = None;
                self.ips.clear();
            }
            if state == BackendState::Running
                && let Some(node) = node
            {
                if let Ok(status) = node.status() {
                    self.dns_name = status.dns_name();
                }
                self.ips = node.ips().unwrap_or_default();
                self.error = None;
            }
        }
    }
}

fn state_name(state: BackendState) -> &'static str {
    match state {
        BackendState::NoState => "no_state",
        BackendState::InUseOtherUser => "in_use_other_user",
        BackendState::NeedsLogin => "needs_login",
        BackendState::NeedsMachineAuth => "needs_machine_auth",
        BackendState::Stopped => "stopped",
        BackendState::Starting => "starting",
        BackendState::Running => "running",
        BackendState::Unknown => "unknown",
    }
}

struct Inner {
    snapshot: Snapshot,
    node: Option<Arc<Node>>,
    starting: bool,
    generation: u64,
}

/// Owns one embedded node per daemon install. The controller is cheap to
/// clone; all state and node handles remain shared and serialized.
pub struct Controller {
    state_dir: PathBuf,
    hostname: String,
    inner: Mutex<Inner>,
    changes: broadcast::Sender<Snapshot>,
    lifecycle: tokio::sync::Mutex<()>,
    stopping: Arc<AtomicBool>,
}

impl Controller {
    pub fn new(state_dir: impl Into<PathBuf>, hostname: impl Into<String>) -> Arc<Self> {
        let (changes, _) = broadcast::channel(32);
        Arc::new(Self {
            state_dir: state_dir.into(),
            hostname: hostname.into(),
            inner: Mutex::new(Inner {
                snapshot: Snapshot { state: "not_started", ..Snapshot::default() },
                node: None,
                starting: false,
                generation: 0,
            }),
            changes,
            lifecycle: tokio::sync::Mutex::new(()),
            stopping: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Snapshot> {
        self.changes.subscribe()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).snapshot.clone()
    }

    /// The live node for the ingress server. It remains valid through a
    /// restart; callers should subscribe and discard it when state stops.
    pub fn node(&self) -> Option<Arc<Node>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).node.clone()
    }

    /// Resume only a saved explicit opt-in; an untouched installation creates nothing.
    pub async fn resume(self: &Arc<Self>) -> Result<(), String> {
        let controller = self.clone();
        tokio::spawn(async move { controller.resume_serialized().await })
            .await
            .map_err(|error| error.to_string())?
    }

    async fn resume_serialized(self: &Arc<Self>) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock().await;
        let path = self.state_dir.join("enabled");
        let enabled = tokio::task::spawn_blocking(move || saved_opt_in(&path))
            .await
            .map_err(|e| e.to_string())??;
        if enabled {
            self.start_locked().await?;
        }
        Ok(())
    }

    pub(crate) fn note_listener_error(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stopping.load(Ordering::Acquire) {
            return;
        }
        inner.snapshot.state = "error";
        inner.snapshot.error =
            Some("The tailnet listener stopped. Start it again to retry.".into());
        self.publish_locked(&inner.snapshot);
    }

    pub(crate) fn begin_shutdown(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        self.stopping.store(true, Ordering::Release);
        inner.generation = inner.generation.wrapping_add(1);
        inner.snapshot.state = "stopped";
        self.publish_locked(&inner.snapshot);
    }

    pub(crate) async fn listener_failed(&self, failed: &Arc<Node>) {
        let _lifecycle = self.lifecycle.lock().await;
        if !self.node().is_some_and(|node| Arc::ptr_eq(&node, failed)) {
            return;
        }
        let _ = self.shutdown_locked().await;
        self.note_listener_error();
    }

    /// Start the node without waiting for login. A saved login resumes; an
    /// unsigned node reports `needs_login` and its URL through `state`.
    pub async fn start(self: &Arc<Self>) -> Result<Snapshot, String> {
        // Complete the state transition even when a requesting connection goes
        // away. Dropping spawn_blocking's waiter cannot cancel node creation.
        let controller = self.clone();
        tokio::spawn(async move {
            let _lifecycle = controller.lifecycle.lock().await;
            controller.start_locked().await
        })
        .await
        .map_err(|error| error.to_string())?
    }

    async fn start_locked(self: &Arc<Self>) -> Result<Snapshot, String> {
        if self.stopping.load(Ordering::Acquire) {
            return Err("tailnet service is stopping".into());
        }
        let generation;
        {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if inner.node.is_some() || inner.starting {
                return Ok(inner.snapshot.clone());
            }
            inner.generation = inner.generation.wrapping_add(1);
            generation = inner.generation;
            inner.starting = true;
            inner.snapshot.state = "starting";
            self.publish_locked(&inner.snapshot);
        }
        let state_dir = self.state_dir.clone();
        let hostname = self.hostname.clone();
        let stopping = self.stopping.clone();
        let node = match tokio::task::spawn_blocking(move || {
            if stopping.load(Ordering::Acquire) {
                return Err("tailnet service is stopping".to_owned());
            }
            crate::config::private_state_dir(&state_dir).map_err(|e| e.to_string())?;
            let enabled = state_dir.join("enabled");
            let config = NodeConfig::new(state_dir, hostname);
            let node = Node::new(&config).map_err(|error| error.0)?;
            node.start().map_err(|error| error.0)?;
            crate::config::write_atomic(&enabled, b"1\n").map_err(|e| e.to_string())?;
            if stopping.load(Ordering::Acquire) {
                let _ = node.close();
                return Err("tailnet service is stopping".to_owned());
            }
            Ok::<_, String>(Arc::new(node))
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string()))
        {
            Ok(node) => node,
            Err(error) => {
                self.start_failed(generation, &error);
                return Err(error);
            }
        };
        {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if self.stopping.load(Ordering::Acquire) {
                inner.starting = false;
                tokio::task::spawn_blocking(move || {
                    let _ = node.close();
                });
                return Err("tailnet service is stopping".into());
            }
            inner.starting = false;
            inner.node = Some(node.clone());
        }
        let this = Arc::clone(self);
        let watch_node = node.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("acpmux-tailnet-watch".into())
            .spawn(move || this.watch(watch_node, generation))
        {
            release_node(node);
            self.clear_node("error");
            self.update(|snapshot| snapshot.error = Some(error.to_string()));
            return Err(error.to_string());
        }
        Ok(self.snapshot())
    }

    fn start_failed(&self, generation: u64, error: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.generation != generation || self.stopping.load(Ordering::Acquire) {
            return;
        }
        inner.starting = false;
        inner.snapshot =
            Snapshot { state: "error", error: Some(error.to_owned()), ..Snapshot::default() };
        self.publish_locked(&inner.snapshot);
    }

    /// Request an explicit browser sign-in URL for a running unsigned node.
    pub async fn login(&self) -> Result<Snapshot, String> {
        let _lifecycle = self.lifecycle.lock().await;
        let node = self.node().ok_or_else(|| "tailnet is not started".to_owned())?;
        let result = tokio::task::spawn_blocking(move || node.login_interactive().map_err(|e| e.0))
            .await
            .map_err(|error| error.to_string())?;
        result?;
        Ok(self.snapshot())
    }

    /// Sign out and close all ingress listeners that hold this node.
    pub async fn logout(&self) -> Result<Snapshot, String> {
        let _lifecycle = self.lifecycle.lock().await;
        let path = self.state_dir.join("enabled");
        tokio::task::spawn_blocking(move || clear_opt_in(&path))
            .await
            .map_err(|e| e.to_string())??;
        if let Some(node) = self.retire_node() {
            let result = tokio::task::spawn_blocking(move || {
                let logout = node.logout().map_err(|e| e.0);
                let close = node.close().map_err(|e| e.0);
                logout.and(close)
            })
            .await
            .map_err(|error| error.to_string())?;
            if let Err(error) = result {
                self.clear_node("error");
                self.update(|snapshot| snapshot.error = Some(error.clone()));
                return Err(error);
            }
        }
        self.clear_node("stopped");
        Ok(self.snapshot())
    }

    /// Close without deleting the saved login, used by daemon shutdown.
    pub async fn shutdown(&self) -> Result<(), String> {
        let _lifecycle = self.lifecycle.lock().await;
        self.shutdown_locked().await
    }

    async fn shutdown_locked(&self) -> Result<(), String> {
        if let Some(node) = self.retire_node() {
            let result = tokio::task::spawn_blocking(move || node.close().map_err(|e| e.0))
                .await
                .map_err(|error| error.to_string())?;
            if let Err(error) = result {
                self.clear_node("error");
                self.update(|snapshot| snapshot.error = Some(error.clone()));
                return Err(error);
            }
        }
        self.clear_node("stopped");
        Ok(())
    }

    // Publish stop before awaiting network work. Cancelling the caller must
    // never leave a closed node registered or its listener advertised as live.
    fn retire_node(&self) -> Option<Arc<Node>> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.generation = inner.generation.wrapping_add(1);
        inner.starting = false;
        inner.snapshot = Snapshot { state: "stopped", ..Snapshot::default() };
        let node = inner.node.take();
        self.publish_locked(&inner.snapshot);
        node
    }

    fn watch(self: Arc<Self>, node: Arc<Node>, generation: u64) {
        let result = node.watch();
        match result {
            Ok(watch) => {
                for notify in watch {
                    let mut snapshot = self.snapshot();
                    snapshot.apply_notify(&notify, Some(&node));
                    self.publish_if_current(generation, snapshot);
                }
                self.clear_if_same(&node, generation, "stopped");
            }
            Err(error) => {
                self.update_if_current(generation, |snapshot| {
                    snapshot.state = "error";
                    snapshot.error = Some(error.0);
                });
                self.clear_if_same(&node, generation, "error");
            }
        }
    }

    fn clear_node(&self, state: &'static str) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let node = inner.node.take();
        inner.starting = false;
        inner.snapshot.state = state;
        inner.snapshot.browse_to_url = None;
        inner.snapshot.dns_name = None;
        inner.snapshot.ips.clear();
        self.publish_locked(&inner.snapshot);
        drop(inner);
        if let Some(node) = node {
            release_node(node);
        }
    }

    fn clear_if_same(&self, node: &Arc<Node>, generation: u64, state: &'static str) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.generation == generation
            && inner.node.as_ref().is_some_and(|current| Arc::ptr_eq(current, node))
        {
            inner.node = None;
            inner.snapshot.state = state;
            self.publish_locked(&inner.snapshot);
        }
    }

    fn update(&self, change: impl FnOnce(&mut Snapshot)) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        change(&mut inner.snapshot);
        self.publish_locked(&inner.snapshot);
    }

    fn update_if_current(&self, generation: u64, change: impl FnOnce(&mut Snapshot)) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.generation != generation {
            return;
        }
        change(&mut inner.snapshot);
        self.publish_locked(&inner.snapshot);
    }

    fn publish_if_current(&self, generation: u64, snapshot: Snapshot) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.generation != generation {
            return;
        }
        inner.snapshot = snapshot;
        self.publish_locked(&inner.snapshot);
    }

    fn publish_locked(&self, snapshot: &Snapshot) {
        let _ = self.changes.send(snapshot.clone());
    }
}

/// libtailscale owns a background runtime. Never let its final `Arc<Node>`
/// drop on a Tokio worker or request task.
fn release_node(node: Arc<Node>) {
    let _ =
        std::thread::Builder::new().name("acpmux-tailnet-release".into()).spawn(move || drop(node));
}

pub fn state_dir(home: &Path) -> PathBuf {
    home.join("tailnet")
}

fn saved_opt_in(path: &Path) -> Result<bool, String> {
    match std::fs::read(path) {
        Ok(value) => Ok(value == b"1\n"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn clear_opt_in(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_late_start_failure_cannot_replace_shutdown_state() {
        let temporary = tempfile::tempdir().unwrap();
        let controller = Controller::new(temporary.path(), "test");
        let generation = controller.inner.lock().unwrap().generation;
        controller.start_failed(generation, "start failed");
        assert_eq!(controller.snapshot().state, "error");
        controller.begin_shutdown();
        controller.start_failed(generation, "late start failed");
        assert_eq!(controller.snapshot().state, "stopped");
    }

    #[tokio::test]
    async fn tailnet_shutdown_fences_a_queued_start_before_node_creation() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("tailnet");
        let controller = Controller::new(&path, "test");
        let lock = controller.lifecycle.lock().await;
        let queued = controller.clone();
        let start = tokio::spawn(async move { queued.start().await });
        tokio::task::yield_now().await;
        controller.begin_shutdown();
        controller.note_listener_error();
        assert_eq!(controller.snapshot().state, "stopped");
        drop(lock);
        assert!(start.await.unwrap().is_err());
        assert!(controller.node().is_none());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn tailnet_resume_does_not_create_state_without_opt_in() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("tailnet");
        let controller = Controller::new(&path, "test");
        controller.resume().await.unwrap();
        assert!(controller.node().is_none());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn tailnet_logout_clears_opt_in_before_any_later_resume() {
        let temporary = tempfile::tempdir().unwrap();
        let marker = temporary.path().join("enabled");
        std::fs::write(&marker, b"1\n").unwrap();
        let controller = Controller::new(temporary.path(), "test");
        controller.shutdown().await.unwrap();
        assert!(saved_opt_in(&marker).unwrap());
        controller.logout().await.unwrap();
        assert!(!saved_opt_in(&marker).unwrap());
        controller.resume().await.unwrap();
        assert!(controller.node().is_none());
    }

    #[tokio::test]
    async fn tailnet_resume_rechecks_opt_in_after_waiting_for_lifecycle() {
        let temporary = tempfile::tempdir().unwrap();
        let marker = temporary.path().join("enabled");
        std::fs::write(&marker, b"1\n").unwrap();
        let controller = Controller::new(temporary.path(), "test");
        let lock = controller.lifecycle.lock().await;
        let queued = controller.clone();
        let resume = tokio::spawn(async move { queued.resume().await });
        tokio::task::yield_now().await;
        clear_opt_in(&marker).unwrap();
        drop(lock);
        resume.await.unwrap().unwrap();
        assert!(controller.node().is_none());
    }

    #[test]
    fn sign_in_url_is_redacted_for_non_person_consumers() {
        let snapshot = Snapshot {
            state: "needs_login",
            browse_to_url: Some("https://login.tailscale.com/fake".into()),
            ..Snapshot::default()
        };
        assert!(snapshot.to_json(false).get("browse_to_url").is_none());
        assert_eq!(snapshot.to_json(true)["browse_to_url"], "https://login.tailscale.com/fake");
    }

    #[test]
    fn state_change_clears_stale_login_url_and_network_identity() {
        let mut snapshot = Snapshot {
            state: "needs_login",
            browse_to_url: Some("https://login.tailscale.com/fake".into()),
            dns_name: Some("cmux.example.ts.net".into()),
            ips: vec!["100.64.0.1".into()],
            ..Snapshot::default()
        };
        let notify = Notify { state: Some(5), ..Notify::default() };
        snapshot.apply_notify(&notify, None);
        assert_eq!(snapshot.state, "starting");
        assert!(snapshot.browse_to_url.is_none());
        assert!(snapshot.dns_name.is_none());
        assert!(snapshot.ips.is_empty());
    }

    #[test]
    fn stale_watch_generation_cannot_publish_after_logout() {
        let controller = Controller::new("/tmp/cmux-tailnet-test", "test");
        {
            let mut inner = controller.inner.lock().unwrap();
            inner.generation = 2;
        }
        controller.update_if_current(1, |snapshot| snapshot.state = "running");
        assert_eq!(controller.snapshot().state, "not_started");
    }
}
