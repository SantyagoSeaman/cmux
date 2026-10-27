//! Frontend-rendered browser tabs after creation: the location and
//! presentation they report (`update-frontend-browser-tab`) and their opaque
//! session history (`frontend-browser-history-v1`). The history is not
//! presentation state: it is never journaled, emitted, or serialized into
//! the tree.

use super::*;

impl Mux {
    /// Install a frontend browser record that `registry` just put
    /// (`put_frontend_browser`) or read in its writer transaction (a keyed
    /// creation's recorded browser), without reading the store back: the
    /// snapshot row is that stored record. The caller holds the registry
    /// lock, as for [`Self::reload_presentation`].
    pub(crate) fn note_frontend_browser_put(
        &self,
        _registry: &WorkspaceRegistry,
        browser_id: &str,
        record: FrontendBrowserRecord,
    ) {
        let mut presentation = self.presentation.lock().recover_poison();
        Arc::make_mut(&mut presentation).frontend_browsers.insert(browser_id.to_string(), record);
    }

    /// Record the URL, title, or favicon a frontend-rendered browser
    /// reports. `favicon_url: Some(None)` clears the favicon.
    #[track_caller]
    pub fn update_frontend_browser_tab(
        &self,
        surface: SurfaceId,
        url: Option<String>,
        title: Option<String>,
        favicon_url: Option<Option<String>>,
    ) -> anyhow::Result<(FrontendBrowserRecord, bool)> {
        let owner = None;
        self.update_frontend_browser_tab_with_owner_as(
            &Actor::Daemon,
            surface,
            url,
            title,
            favicon_url,
            owner,
        )
    }

    /// Record a frontend-rendered browser's location and optional hosting app
    /// owner; `actor` sets the owner.
    #[track_caller]
    pub fn update_frontend_browser_tab_with_owner_as(
        &self,
        actor: &Actor,
        surface: SurfaceId,
        url: Option<String>,
        title: Option<String>,
        favicon_url: Option<Option<String>>,
        owner: Option<String>,
    ) -> anyhow::Result<(FrontendBrowserRecord, bool)> {
        self.update_frontend_browser_tab_with_owner_and_profile_as(
            actor,
            surface,
            url,
            title,
            favicon_url,
            None,
            owner,
        )
    }

    /// Record a frontend-rendered browser's location, optional hosting app
    /// owner, and browser profile assignment. A profile is addressed by its
    /// stable id or exact display name and `Some(None)` clears it.
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub fn update_frontend_browser_tab_with_owner_and_profile_as(
        &self,
        actor: &Actor,
        surface: SurfaceId,
        url: Option<String>,
        title: Option<String>,
        favicon_url: Option<Option<String>>,
        profile_id: Option<Option<String>>,
        owner: Option<String>,
    ) -> anyhow::Result<(FrontendBrowserRecord, bool)> {
        let runtime =
            self.surface(surface).ok_or_else(|| anyhow::anyhow!("unknown surface {surface}"))?;
        let browser_id = self.frontend_browser_id(&runtime).ok_or_else(|| {
            anyhow::anyhow!("surface {surface} is not a frontend-rendered browser")
        })?;
        self.refuse_conversation_tab(&runtime)?;
        if let Some(owner) = &owner {
            crate::state::window_record_store::validate_key("owner", owner)?;
        }
        let previous_url = runtime.browser_url();
        let (mut record, mut changed) = {
            let mut registry = self.workspace_registry.lock().unwrap();
            let result = registry.update_frontend_browser(
                browser_id.as_str(),
                url.as_deref(),
                title.as_deref(),
                favicon_url.as_ref().map(Option::as_deref),
                profile_id.as_ref().map(|value| value.as_deref()),
            )?;
            if result.1 {
                self.reload_presentation(&registry)?;
            }
            self.settle_presentation_receipts(&registry);
            result
        };
        if let Some(owner) = owner
            && record.owner.as_deref() != Some(owner.as_str())
        {
            let tab = runtime
                .resource_identity()
                .map(|identity| identity.tab_id.to_string())
                .ok_or_else(|| anyhow::anyhow!("surface {surface} has no public tab id"))?;
            self.commit_browser_owner(actor, &tab, &owner)?;
            record.owner = Some(owner);
            changed = true;
        }
        // tab-attention-v1: the page the agent asked the person to act on is gone.
        let navigated_to = url.clone().filter(|url| previous_url.as_ref() != Some(url));
        if changed {
            if let Some(browser) = runtime.as_browser() {
                browser.set_frontend_location(url, title.clone());
            }
            self.publish_journal_event();
            if let Some(title) = title {
                self.emit(MuxEvent::TitleChanged { surface, title: Arc::from(title) });
            }
            self.emit_tab_changed(surface);
        }
        if let Some(page) = &navigated_to {
            self.clear_attention_after_navigation(actor, surface, page);
        }
        Ok((record, changed))
    }

    /// The frontend browser id behind a surface, with the errors
    /// `update_frontend_browser_tab` reports.
    #[track_caller]
    fn require_frontend_browser_id(&self, surface: SurfaceId) -> anyhow::Result<BrowserPublicId> {
        let runtime =
            self.surface(surface).ok_or_else(|| anyhow::anyhow!("unknown surface {surface}"))?;
        self.frontend_browser_id(&runtime)
            .ok_or_else(|| anyhow::anyhow!("surface {surface} is not a frontend-rendered browser"))
    }

    /// Store (`Some`) or clear (`None`) the opaque session history (back and
    /// forward entries, scroll) a frontend keeps for a frontend-rendered
    /// browser.
    #[track_caller]
    pub fn set_frontend_browser_history(
        &self,
        surface: SurfaceId,
        history: Option<String>,
    ) -> anyhow::Result<()> {
        let browser_id = self.require_frontend_browser_id(surface)?;
        let mut registry = self.workspace_registry.lock().unwrap();
        registry.set_frontend_browser_history(browser_id.as_str(), history.as_deref())?;
        self.settle_presentation_receipts(&registry);
        Ok(())
    }

    /// The session history last stored for a frontend-rendered browser.
    #[track_caller]
    pub fn frontend_browser_history(&self, surface: SurfaceId) -> anyhow::Result<Option<String>> {
        let browser_id = self.require_frontend_browser_id(surface)?;
        let registry = self.workspace_registry.lock().unwrap();
        registry.frontend_browser_history(browser_id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::PresentationTestSession;
    use super::*;

    /// p05-ipc-135: a new frontend browser tab (plain and keyed) installs
    /// its record without reading the store back; the in-memory snapshot
    /// equals the store's.
    #[test]
    fn new_frontend_browser_tabs_keep_the_snapshot_equal_to_the_store() {
        let session = PresentationTestSession::new("frontend-browser-put-snapshot");
        let mux = session.open();
        let terminal = mux.new_workspace(None, None).unwrap().id;
        let pane = mux.with_state(|state| state.pane_of(terminal)).unwrap();
        let record = |url: &str| FrontendBrowserRecord {
            engine: "webkit".into(),
            url: url.into(),
            title: Some("Page".into()),
            favicon_url: None,
            profile_id: None,
            owner: None,
        };
        mux.new_frontend_browser_tab(Some(pane), record("https://a.test/"), None).unwrap();
        let keyed = mux
            .new_frontend_browser_tab_keyed_as(
                &Actor::Daemon,
                Some(pane),
                record("https://b.test/"),
                None,
                "snapshot-key",
                FrontendTabPlacement::default(),
            )
            .unwrap();
        assert!(!keyed.replayed);
        let stored = mux.workspace_registry.lock().unwrap().presentation_snapshot().unwrap();
        let live = mux.presentation_snapshot();
        assert_eq!(live.frontend_browsers.len(), 2);
        assert_eq!(live.frontend_browsers, stored.frontend_browsers);
        assert!(mux.has_frontend_browser_key("snapshot-key").unwrap());
    }

    /// p05-ipc-135: `new-frontend-browser-tab` (plain and keyed) opens no
    /// request-side SQLite connection of its own: its registration goes to
    /// the journal writer, so the op notes exactly the `sqlite` of the
    /// browser tab commit it shares with `new-browser-tab`, and a keyed
    /// replay notes none. The shared commit's notes (creation recovery and
    /// lookup, the executing mark, the state pin, the public fold) belong to
    /// p05-ipc-407, 133 and 136; when they land, `shared` is 0 here too.
    #[test]
    fn new_frontend_browser_tab_adds_no_request_side_sqlite() {
        use crate::request_path::{
            BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats,
        };
        let session = PresentationTestSession::new("frontend-browser-tab-sqlite");
        let mux = session.open();
        let terminal = mux.new_workspace(None, None).unwrap().id;
        let pane = mux.with_state(|state| state.pane_of(terminal)).unwrap();
        let record = |url: &str| FrontendBrowserRecord {
            engine: "webkit".into(),
            url: url.into(),
            title: None,
            favicon_url: None,
            profile_id: None,
            owner: None,
        };
        let sqlite = |op: &dyn Fn()| {
            let stats = RequestPathStats::new(RequestPathMode::RECORD);
            {
                let _scope = RequestPathScope::enter(&stats);
                op();
            }
            (stats.count(BlockingKind::Sqlite), stats.sites())
        };
        let keyed = || {
            mux.new_frontend_browser_tab_keyed_as(
                &Actor::Daemon,
                Some(pane),
                record("https://keyed.test/"),
                None,
                "sqlite-key",
                FrontendTabPlacement::default(),
            )
            .unwrap()
        };
        mux.new_browser_tab("https://warm.test/".into(), Some(pane), None).unwrap();

        let (shared, shared_sites) = sqlite(&|| {
            mux.new_browser_tab("https://cdp.test/".into(), Some(pane), None).unwrap();
        });
        let (plain, plain_sites) = sqlite(&|| {
            mux.new_frontend_browser_tab(Some(pane), record("https://a.test/"), None).unwrap();
        });
        let (fresh, fresh_sites) = sqlite(&|| assert!(!keyed().replayed));
        let (replay, replay_sites) = sqlite(&|| assert!(keyed().replayed));
        assert_eq!(plain, shared, "plain {plain_sites:?}\nshared {shared_sites:?}");
        assert_eq!(fresh, shared, "keyed {fresh_sites:?}\nshared {shared_sites:?}");
        assert_eq!(replay, 0, "{replay_sites:?}");
    }

    #[test]
    fn cmux_next_frontend_browser_history_is_kept_out_of_the_tree() {
        let session = PresentationTestSession::new("frontend-browser-history");
        let mux = session.open();
        let terminal = mux.new_workspace(None, None).unwrap().id;
        let pane = mux.with_state(|state| state.pane_of(terminal)).unwrap();
        let record = FrontendBrowserRecord {
            engine: "cef".into(),
            url: "https://example.com/".into(),
            title: None,
            favicon_url: None,
            profile_id: None,
            owner: None,
        };
        let browser = mux.new_frontend_browser_tab(Some(pane), record, None).unwrap().id;
        assert_eq!(mux.frontend_browser_history(browser).unwrap(), None);
        let history = r#"{"entries":["https://example.com/"],"index":0,"scroll_y":120}"#;
        mux.set_frontend_browser_history(browser, Some(history.into())).unwrap();
        assert_eq!(mux.frontend_browser_history(browser).unwrap().as_deref(), Some(history));
        // The history never reaches the tree.
        let decorations = mux.tree_decorations();
        let tree = mux.with_state(|state| crate::server::workspaces_json(state, &decorations));
        assert!(!tree.to_string().contains("scroll_y"));

        // A PTY tab or an unknown surface has no frontend browser history.
        let error = mux.set_frontend_browser_history(terminal, Some("{}".into())).unwrap_err();
        assert!(error.to_string().contains("not a frontend-rendered browser"));
        assert!(mux.frontend_browser_history(terminal).is_err());
        let error = mux.frontend_browser_history(999_999).unwrap_err();
        assert!(error.to_string().contains("unknown surface"));
        assert!(mux.set_frontend_browser_history(browser, Some("[]".into())).is_err());
        let tab_id = mux.with_state(|state| state.resource_indexes.tab_ids[&browser].clone());
        drop(mux);

        // It survives a daemon restart, and `None` clears it.
        let mux = session.open();
        let restored = mux
            .with_state(|state| state.resource_indexes.tabs.get(&tab_id).copied())
            .expect("frontend browser tab restored");
        assert_eq!(mux.frontend_browser_history(restored).unwrap().as_deref(), Some(history));
        mux.set_frontend_browser_history(restored, None).unwrap();
        assert_eq!(mux.frontend_browser_history(restored).unwrap(), None);
    }
}
