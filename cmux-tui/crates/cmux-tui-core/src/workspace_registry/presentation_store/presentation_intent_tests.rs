//! p05-ipc-135: presentation writes reach SQLite only through the registry
//! intent sink (the journal writer), never in a request-side transaction.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use super::*;
use crate::journal_ingress::EffectSend;
use crate::request_path::{BlockingKind, RequestPathMode, RequestPathScope, RequestPathStats};
use crate::workspace_registry::{RegistryConnection, RegistryIntent, RegistryIntentSink};

const BROWSER: &str = "browser_0123456789abcdef0123456789abcdef";

/// A stand-in journal writer: applies each intent under its SAVEPOINT on its
/// own thread, as the real writer does in its batch.
struct ThreadWriter {
    connection: Weak<RegistryConnection>,
    sent: AtomicUsize,
}

impl RegistryIntentSink for ThreadWriter {
    fn accepts(&self) -> bool {
        true
    }

    fn send(
        &self,
        intent: RegistryIntent,
        _site: &'static std::panic::Location<'static>,
    ) -> EffectSend {
        self.sent.fetch_add(1, Ordering::SeqCst);
        let connection = self.connection.upgrade().expect("registry connection");
        let applied = std::thread::spawn(move || -> anyhow::Result<_> {
            let db = connection.get();
            let tx = db.unchecked_transaction()?;
            let receipt = intent.apply_in_savepoint(&tx)?;
            tx.commit()?;
            Ok(receipt)
        })
        .join()
        .expect("writer thread");
        EffectSend::Sent(applied.and_then(|receipt| receipt.map_err(anyhow::Error::msg)))
    }
}

struct Fixture {
    root: std::path::PathBuf,
    registry: WorkspaceRegistry,
    writer: Arc<ThreadWriter>,
}

impl Fixture {
    fn open(label: &str) -> Self {
        let root = std::env::temp_dir()
            .join(format!("cmux-presentation-intent-{label}-{}", new_uuid_v4()));
        let registry = WorkspaceRegistry::open(&root, "session").expect("registry");
        let writer = Arc::new(ThreadWriter {
            connection: Arc::downgrade(&registry.connection),
            sent: AtomicUsize::new(0),
        });
        registry.connection.install_intent_sink(writer.clone());
        Self { root, registry, writer }
    }

    fn journal_records(&self) -> i64 {
        self.registry
            .connection
            .get()
            .query_row("SELECT COUNT(*) FROM session_journal", [], |row| row.get(0))
            .expect("journal count")
    }

    fn local_commits(&self) -> u64 {
        self.registry.connection.write_path_stats().snapshot().request_effect_commits
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn record(url: &str) -> FrontendBrowserRecord {
    FrontendBrowserRecord {
        engine: "webkit".into(),
        url: url.into(),
        title: None,
        favicon_url: None,
        profile_id: None,
        owner: None,
    }
}

/// Every presentation write goes to the writer; the request thread takes
/// the registry connection zero times (`request_path` sqlite == 0).
#[test]
fn presentation_writes_commit_through_the_writer_with_no_request_side_sqlite() {
    let mut fixture = Fixture::open("writer");
    let stats = RequestPathStats::new(RequestPathMode::RECORD);
    {
        let _scope = RequestPathScope::enter(&stats);
        let registry = &mut fixture.registry;
        let (group, created) =
            registry.create_workspace_group("work", "Work", None, false, None).unwrap();
        assert!(created);
        assert_eq!(group.name, "Work");
        let (_, created) =
            registry.create_workspace_group("work", "Work", None, false, None).unwrap();
        assert!(!created, "a retried create is idempotent");
        registry.create_workspace_group("play", "Play", Some("#112233"), false, Some(0)).unwrap();
        let group = registry.update_workspace_group("work", Some("Job"), None, Some(true)).unwrap();
        assert_eq!((group.name.as_str(), group.collapsed), ("Job", true));
        assert_eq!(registry.move_workspace_group("work", 0).unwrap(), 0);
        assert_eq!(registry.delete_workspace_group("play").unwrap(), Vec::<String>::new());
        registry.put_frontend_browser(BROWSER, &record("https://a.test/"), None).unwrap();
        let (updated, changed) = registry
            .update_frontend_browser(BROWSER, Some("https://b.test/"), Some("B"), None, None)
            .unwrap();
        assert!(changed);
        assert_eq!(updated.url, "https://b.test/");
        let (_, changed) = registry
            .update_frontend_browser(BROWSER, Some("https://b.test/"), Some("B"), None, None)
            .unwrap();
        assert!(!changed, "an unchanged record writes nothing");
        registry.set_frontend_browser_history(BROWSER, Some("{}")).unwrap();
    }
    assert_eq!(stats.count(BlockingKind::Sqlite), 0, "{:?}", stats.sites());
    assert_eq!(fixture.writer.sent.load(Ordering::SeqCst), 10);
    assert_eq!(fixture.local_commits(), 0);

    let snapshot = fixture.registry.presentation_snapshot().unwrap();
    let groups = snapshot.groups.iter().map(|group| group.id.as_str()).collect::<Vec<_>>();
    assert_eq!(groups, ["work"]);
    assert_eq!(snapshot.frontend_browsers[BROWSER].url, "https://b.test/");
    assert_eq!(fixture.registry.frontend_browser_history(BROWSER).unwrap().as_deref(), Some("{}"));
}

/// A failed intent rolls back only its own SAVEPOINT: no row, no journal
/// record, and the error reaches the request.
#[test]
fn a_failed_presentation_intent_leaves_no_rows_or_journal_records() {
    let mut fixture = Fixture::open("rollback");
    fixture.registry.put_frontend_browser(BROWSER, &record("https://a.test/"), None).unwrap();
    let journal = fixture.journal_records();
    // The browser row exists: a second put violates its primary key.
    let error = fixture
        .registry
        .put_frontend_browser(BROWSER, &record("https://c.test/"), None)
        .unwrap_err();
    assert!(format!("{error:#}").contains("UNIQUE"), "{error:#}");
    let error =
        fixture.registry.update_workspace_group("missing", Some("x"), None, None).unwrap_err();
    assert!(error.to_string().contains("unknown workspace group missing"));
    assert_eq!(fixture.journal_records(), journal);
    let snapshot = fixture.registry.presentation_snapshot().unwrap();
    assert_eq!(snapshot.frontend_browsers[BROWSER].url, "https://a.test/");
}

/// Owned extra rows share the record's SAVEPOINT: a failing extra write
/// takes the record with it.
#[test]
fn owned_extra_rows_commit_or_roll_back_with_the_record() {
    let mut fixture = Fixture::open("extra");
    let failing: OwnedTransactionWrite = Box::new(|_| anyhow::bail!("extra rows refused"));
    let error = fixture
        .registry
        .put_frontend_browser_owned(BROWSER, &record("https://a.test/"), failing)
        .unwrap_err();
    assert!(error.to_string().contains("extra rows refused"));
    assert!(
        !fixture.registry.presentation_snapshot().unwrap().frontend_browsers.contains_key(BROWSER)
    );

    let history: OwnedTransactionWrite = Box::new(|tx| {
        tx.execute(
            "INSERT INTO frontend_browser_history(browser_id, history) VALUES(?1, '{}')",
            [BROWSER],
        )?;
        Ok(())
    });
    fixture
        .registry
        .put_frontend_browser_owned(BROWSER, &record("https://a.test/"), history)
        .unwrap();
    assert_eq!(fixture.registry.frontend_browser_history(BROWSER).unwrap().as_deref(), Some("{}"));
    assert_eq!(fixture.writer.sent.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.local_commits(), 0);
}

/// A borrowed extra cannot leave the request thread: that put commits
/// locally and is counted (`request_effect_commits`).
#[test]
fn a_borrowed_extra_commits_locally_and_is_counted() {
    let mut fixture = Fixture::open("borrowed");
    let write = |_: &Transaction<'_>| Ok(());
    fixture
        .registry
        .put_frontend_browser(BROWSER, &record("https://a.test/"), Some(&write))
        .unwrap();
    assert_eq!(fixture.writer.sent.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.local_commits(), 1);
}
