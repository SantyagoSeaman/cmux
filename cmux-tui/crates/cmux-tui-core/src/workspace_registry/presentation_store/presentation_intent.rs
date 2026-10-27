//! Presentation writes as journal writer intents (p05-ipc-135, queue Q13).
//!
//! A request validates its input and prepares a [`PresentationCommitIntent`];
//! the journal writer applies it inside its batch transaction under a
//! SAVEPOINT (`RegistryIntent::Presentation`), so the request thread opens no
//! SQLite transaction for it. The reads a write depends on (the group order,
//! whether a browser id is fresh, the record an update merges into) run in
//! that same transaction, after every earlier commit of the batch.
//!
//! This is Chromium's split for profile state: the UI thread validates and
//! posts the write to the one sequence that owns the store
//! (components/prefs/json_pref_store.cc `ImportantFileWriter`,
//! components/sessions/core/command_storage_manager.cc `ScheduleCommand`).
//! `apply` is repeatable: the writer can retry or split a failed batch.

use super::*;
use crate::workspace_registry::OwnedTransactionWrite;

/// One prepared presentation write.
#[derive(Debug)]
pub(crate) struct PresentationCommitIntent {
    write: PresentationWrite,
}

#[derive(Debug)]
enum PresentationWrite {
    CreateWorkspaceGroup {
        id: String,
        name: String,
        color: Option<String>,
        collapsed: bool,
        index: Option<usize>,
    },
    UpdateWorkspaceGroup {
        id: String,
        name: Option<String>,
        color: Option<Option<String>>,
        collapsed: Option<bool>,
    },
    DeleteWorkspaceGroup {
        id: String,
    },
    MoveWorkspaceGroup {
        id: String,
        index: usize,
    },
    PutFrontendBrowser {
        browser_id: String,
        record: FrontendBrowserRecord,
        extra: Option<ExtraRows>,
    },
    UpdateFrontendBrowser {
        browser_id: String,
        url: Option<String>,
        title: Option<String>,
        favicon_url: Option<Option<String>>,
        profile_id: Option<Option<String>>,
    },
    SetFrontendBrowserHistory {
        browser_id: String,
        history: Option<String>,
    },
    RegisterKeyedFrontendBrowser {
        key: String,
        fingerprint: Value,
        browser_id: String,
        record: FrontendBrowserRecord,
    },
}

/// [`OwnedTransactionWrite`] with a `Debug` for the intent.
struct ExtraRows(OwnedTransactionWrite);

impl std::fmt::Debug for ExtraRows {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExtraRows")
    }
}

/// What a presentation write committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PresentationReceipt {
    /// The group, and `false` for an idempotent retry of an existing one.
    GroupCreated(WorkspaceGroupRecord, bool),
    GroupUpdated(WorkspaceGroupRecord),
    /// Keys of the workspaces the delete ungrouped.
    GroupDeleted(Vec<String>),
    /// The group's final index.
    GroupMoved(usize),
    FrontendBrowserPut,
    /// The stored record, and whether the write changed it.
    FrontendBrowserUpdated(FrontendBrowserRecord, bool),
    FrontendBrowserHistorySet,
    KeyedFrontendBrowser(KeyedFrontendBrowserRegistration),
}

/// What a keyed frontend browser registration found in the writer
/// transaction (state/frontend_browser_keys.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyedFrontendBrowserRegistration {
    /// The key was new: the record and the key's row were put under the
    /// request's fresh browser id.
    Registered,
    /// The key was recorded before; nothing was written.
    Recorded {
        browser_id: String,
        /// The request fingerprint the key recorded.
        fingerprint: Value,
        /// A tab ever committed the browser id (live or closed).
        committed: bool,
        /// The stored frontend browser record of the browser id.
        record: Option<Box<FrontendBrowserRecord>>,
    },
}

/// A receipt of another write than the one the caller committed.
#[derive(Debug)]
pub(crate) struct PresentationReceiptMismatch {
    wanted: &'static str,
    got: PresentationReceipt,
}

impl std::fmt::Display for PresentationReceiptMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "presentation receipt {:?} does not answer a {} write",
            self.got, self.wanted
        )
    }
}

impl std::error::Error for PresentationReceiptMismatch {}

impl PresentationReceipt {
    pub(crate) fn mismatch(self, wanted: &'static str) -> PresentationReceiptMismatch {
        PresentationReceiptMismatch { wanted, got: self }
    }
}

impl PresentationCommitIntent {
    fn new(write: PresentationWrite) -> Self {
        Self { write }
    }

    pub(super) fn create_workspace_group(
        id: &str,
        name: &str,
        color: Option<&str>,
        collapsed: bool,
        index: Option<usize>,
    ) -> Self {
        Self::new(PresentationWrite::CreateWorkspaceGroup {
            id: id.to_string(),
            name: name.to_string(),
            color: color.map(str::to_string),
            collapsed,
            index,
        })
    }

    pub(super) fn update_workspace_group(
        id: &str,
        name: Option<&str>,
        color: Option<Option<&str>>,
        collapsed: Option<bool>,
    ) -> Self {
        Self::new(PresentationWrite::UpdateWorkspaceGroup {
            id: id.to_string(),
            name: name.map(str::to_string),
            color: color.map(|color| color.map(str::to_string)),
            collapsed,
        })
    }

    pub(super) fn delete_workspace_group(id: &str) -> Self {
        Self::new(PresentationWrite::DeleteWorkspaceGroup { id: id.to_string() })
    }

    pub(super) fn move_workspace_group(id: &str, index: usize) -> Self {
        Self::new(PresentationWrite::MoveWorkspaceGroup { id: id.to_string(), index })
    }

    pub(super) fn put_frontend_browser(
        browser_id: &str,
        record: &FrontendBrowserRecord,
        extra: Option<OwnedTransactionWrite>,
    ) -> Self {
        Self::new(PresentationWrite::PutFrontendBrowser {
            browser_id: browser_id.to_string(),
            record: record.clone(),
            extra: extra.map(ExtraRows),
        })
    }

    pub(super) fn update_frontend_browser(
        browser_id: &str,
        url: Option<&str>,
        title: Option<&str>,
        favicon_url: Option<Option<&str>>,
        profile_id: Option<Option<&str>>,
    ) -> Self {
        Self::new(PresentationWrite::UpdateFrontendBrowser {
            browser_id: browser_id.to_string(),
            url: url.map(str::to_string),
            title: title.map(str::to_string),
            favicon_url: favicon_url.map(|value| value.map(str::to_string)),
            profile_id: profile_id.map(|value| value.map(str::to_string)),
        })
    }

    pub(super) fn set_frontend_browser_history(browser_id: &str, history: Option<&str>) -> Self {
        Self::new(PresentationWrite::SetFrontendBrowserHistory {
            browser_id: browser_id.to_string(),
            history: history.map(str::to_string),
        })
    }

    pub(super) fn register_keyed_frontend_browser(
        key: &str,
        fingerprint: &Value,
        browser_id: &str,
        record: &FrontendBrowserRecord,
    ) -> Self {
        Self::new(PresentationWrite::RegisterKeyedFrontendBrowser {
            key: key.to_string(),
            fingerprint: fingerprint.clone(),
            browser_id: browser_id.to_string(),
            record: record.clone(),
        })
    }

    /// Resident size for the writer's durable batch byte cap.
    pub(crate) fn estimated_bytes(&self) -> usize {
        let text = |value: &Option<String>| value.as_ref().map_or(0, String::len);
        let payload = match &self.write {
            PresentationWrite::CreateWorkspaceGroup { id, name, color, .. } => {
                id.len() + name.len() + text(color)
            }
            PresentationWrite::UpdateWorkspaceGroup { id, name, color, .. } => {
                id.len() + text(name) + color.as_ref().map_or(0, text)
            }
            PresentationWrite::DeleteWorkspaceGroup { id }
            | PresentationWrite::MoveWorkspaceGroup { id, .. } => id.len(),
            PresentationWrite::PutFrontendBrowser { browser_id, record, .. } => {
                browser_id.len()
                    + record.url.len()
                    + text(&record.title)
                    + text(&record.favicon_url)
                    + text(&record.profile_id)
            }
            PresentationWrite::UpdateFrontendBrowser { browser_id, url, title, .. } => {
                browser_id.len() + text(url) + text(title)
            }
            PresentationWrite::SetFrontendBrowserHistory { browser_id, history } => {
                browser_id.len() + text(history)
            }
            PresentationWrite::RegisterKeyedFrontendBrowser {
                key,
                fingerprint,
                browser_id,
                record,
            } => {
                key.len()
                    + fingerprint.to_string().len()
                    + browser_id.len()
                    + record.url.len()
                    + text(&record.title)
                    + text(&record.favicon_url)
                    + text(&record.profile_id)
            }
        };
        // The journal record repeats the payload as JSON.
        payload.saturating_mul(2).saturating_add(512)
    }

    /// Apply inside `transaction`: the writer's SAVEPOINT, or a request-side
    /// transaction when the writer does not run.
    pub(crate) fn apply(
        &self,
        transaction: &Transaction<'_>,
    ) -> anyhow::Result<PresentationReceipt> {
        Ok(match &self.write {
            PresentationWrite::CreateWorkspaceGroup { id, name, color, collapsed, index } => {
                let (group, created) = create_workspace_group_in(
                    transaction,
                    id,
                    name,
                    color.as_deref(),
                    *collapsed,
                    *index,
                )?;
                PresentationReceipt::GroupCreated(group, created)
            }
            PresentationWrite::UpdateWorkspaceGroup { id, name, color, collapsed } => {
                PresentationReceipt::GroupUpdated(update_workspace_group_in(
                    transaction,
                    id,
                    name.as_deref(),
                    color.as_ref().map(Option::as_deref),
                    *collapsed,
                )?)
            }
            PresentationWrite::DeleteWorkspaceGroup { id } => {
                PresentationReceipt::GroupDeleted(delete_workspace_group_in(transaction, id)?)
            }
            PresentationWrite::MoveWorkspaceGroup { id, index } => {
                PresentationReceipt::GroupMoved(move_workspace_group_in(transaction, id, *index)?)
            }
            PresentationWrite::PutFrontendBrowser { browser_id, record, extra } => {
                put_frontend_browser_in(transaction, browser_id, record)?;
                if let Some(ExtraRows(extra)) = extra {
                    extra(transaction)?;
                }
                PresentationReceipt::FrontendBrowserPut
            }
            PresentationWrite::UpdateFrontendBrowser {
                browser_id,
                url,
                title,
                favicon_url,
                profile_id,
            } => {
                let (record, changed) = update_frontend_browser_in(
                    transaction,
                    browser_id,
                    url.as_deref(),
                    title.as_deref(),
                    favicon_url.as_ref().map(Option::as_deref),
                    profile_id.as_ref().map(Option::as_deref),
                )?;
                PresentationReceipt::FrontendBrowserUpdated(record, changed)
            }
            PresentationWrite::SetFrontendBrowserHistory { browser_id, history } => {
                frontend_browser_history::set_frontend_browser_history_in(
                    transaction,
                    browser_id,
                    history.as_deref(),
                )?;
                PresentationReceipt::FrontendBrowserHistorySet
            }
            PresentationWrite::RegisterKeyedFrontendBrowser {
                key,
                fingerprint,
                browser_id,
                record,
            } => PresentationReceipt::KeyedFrontendBrowser(register_keyed_frontend_browser_in(
                transaction,
                key,
                fingerprint,
                browser_id,
                record,
            )?),
        })
    }
}

fn register_keyed_frontend_browser_in(
    tx: &Transaction<'_>,
    key: &str,
    fingerprint: &Value,
    browser_id: &str,
    record: &FrontendBrowserRecord,
) -> anyhow::Result<KeyedFrontendBrowserRegistration> {
    use crate::state::frontend_browser_keys::{browser_committed, browser_for_key, insert_key};
    if let Some((recorded, stored)) = browser_for_key(tx, key)? {
        return Ok(KeyedFrontendBrowserRegistration::Recorded {
            committed: browser_committed(tx, &recorded)?,
            record: read_frontend_browser(tx, &recorded)?.map(Box::new),
            browser_id: recorded,
            fingerprint: stored,
        });
    }
    put_frontend_browser_in(tx, browser_id, record)?;
    insert_key(tx, key, browser_id, fingerprint)?;
    Ok(KeyedFrontendBrowserRegistration::Registered)
}

fn create_workspace_group_in(
    tx: &Transaction<'_>,
    id: &str,
    name: &str,
    color: Option<&str>,
    collapsed: bool,
    index: Option<usize>,
) -> anyhow::Result<(WorkspaceGroupRecord, bool)> {
    if let Some(existing) = read_group(tx, id)? {
        anyhow::ensure!(
            existing.name == name,
            "workspace group {id} already exists with a different name"
        );
        return Ok((existing, false));
    }
    let mut order = read_groups(tx)?.into_iter().map(|group| group.id).collect::<Vec<_>>();
    let index = index.unwrap_or(order.len()).min(order.len());
    tx.execute(
        "INSERT INTO workspace_groups(group_id, name, color, position, collapsed)
         VALUES(?1, ?2, ?3, ?4, ?5)",
        params![id, name, color, i64::try_from(order.len())?, i64::from(collapsed)],
    )?;
    order.insert(index, id.to_string());
    write_group_order(tx, &order)?;
    let group = WorkspaceGroupRecord {
        id: id.to_string(),
        name: name.to_string(),
        color: color.map(str::to_string),
        collapsed,
    };
    append_presentation_record(
        tx,
        "workspace.group.created",
        vec![group_subject(id)],
        &json!({"group": group, "index": index}),
    )?;
    Ok((group, true))
}

fn update_workspace_group_in(
    tx: &Transaction<'_>,
    id: &str,
    name: Option<&str>,
    color: Option<Option<&str>>,
    collapsed: Option<bool>,
) -> anyhow::Result<WorkspaceGroupRecord> {
    let mut group =
        read_group(tx, id)?.ok_or_else(|| anyhow::anyhow!("unknown workspace group {id}"))?;
    if let Some(name) = name {
        group.name = name.to_string();
    }
    if let Some(color) = color {
        group.color = color.map(str::to_string);
    }
    if let Some(collapsed) = collapsed {
        group.collapsed = collapsed;
    }
    tx.execute(
        "UPDATE workspace_groups SET name = ?2, color = ?3, collapsed = ?4 WHERE group_id = ?1",
        params![id, group.name, group.color, i64::from(group.collapsed)],
    )?;
    append_presentation_record(
        tx,
        "workspace.group.updated",
        vec![group_subject(id)],
        &json!({"group": group}),
    )?;
    Ok(group)
}

fn delete_workspace_group_in(tx: &Transaction<'_>, id: &str) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(read_group(tx, id)?.is_some(), "unknown workspace group {id}");
    let members = {
        let mut statement = tx.prepare(
            "SELECT workspace_key FROM workspace_presentation WHERE group_id = ?1
             ORDER BY workspace_key",
        )?;
        statement.query_map([id], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?
    };
    tx.execute("DELETE FROM workspace_groups WHERE group_id = ?1", [id])?;
    tx.execute("UPDATE workspace_presentation SET group_id = NULL WHERE group_id = ?1", [id])?;
    tx.execute(
        "DELETE FROM workspace_presentation
         WHERE group_id IS NULL AND color IS NULL AND icon IS NULL AND title IS NULL
           AND pinned = 0 AND marked_unread = 0 AND description IS NULL
           AND task_status IS NULL",
        [],
    )?;
    let order = read_groups(tx)?.into_iter().map(|group| group.id).collect::<Vec<_>>();
    write_group_order(tx, &order)?;
    append_presentation_record(
        tx,
        "workspace.group.deleted",
        vec![group_subject(id)],
        &json!({"group_id": id, "ungrouped_workspace_keys": members}),
    )?;
    Ok(members)
}

fn move_workspace_group_in(tx: &Transaction<'_>, id: &str, index: usize) -> anyhow::Result<usize> {
    let mut order = read_groups(tx)?.into_iter().map(|group| group.id).collect::<Vec<_>>();
    let old_index = order
        .iter()
        .position(|candidate| candidate == id)
        .ok_or_else(|| anyhow::anyhow!("unknown workspace group {id}"))?;
    let new_index = if index > old_index { index.saturating_sub(1) } else { index }
        .min(order.len().saturating_sub(1));
    if new_index != old_index {
        let moved = order.remove(old_index);
        order.insert(new_index, moved);
        write_group_order(tx, &order)?;
        append_presentation_record(
            tx,
            "workspace.group.moved",
            vec![group_subject(id)],
            &json!({"group_id": id, "index": new_index}),
        )?;
    }
    Ok(new_index)
}

fn put_frontend_browser_in(
    tx: &Transaction<'_>,
    browser_id: &str,
    record: &FrontendBrowserRecord,
) -> anyhow::Result<()> {
    let exists = tx
        .query_row("SELECT 1 FROM resource_browsers WHERE public_id = ?1", [browser_id], |_| Ok(()))
        .optional()?
        .is_some();
    anyhow::ensure!(!exists, "browser {browser_id} already exists");
    tx.execute(
        "INSERT INTO frontend_browser_tabs(
           browser_id, engine, url, title, favicon_url, profile_id, owner
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            browser_id,
            record.engine,
            record.url,
            record.title,
            record.favicon_url,
            record.profile_id,
            record.owner
        ],
    )?;
    append_presentation_record(
        tx,
        "browser.frontend.registered",
        vec![browser_subject(browser_id)],
        &json!({"browser_id": browser_id, "browser": record}),
    )
}

fn update_frontend_browser_in(
    tx: &Transaction<'_>,
    browser_id: &str,
    url: Option<&str>,
    title: Option<&str>,
    favicon_url: Option<Option<&str>>,
    profile_id: Option<Option<&str>>,
) -> anyhow::Result<(FrontendBrowserRecord, bool)> {
    let before = read_frontend_browser(tx, browser_id)?
        .ok_or_else(|| anyhow::anyhow!("browser {browser_id} is not frontend-rendered"))?;
    let mut record = before.clone();
    if let Some(url) = url {
        record.url = url.to_string();
    }
    if let Some(title) = title {
        record.title = Some(title.to_string());
    }
    if let Some(favicon_url) = favicon_url {
        record.favicon_url = favicon_url.map(str::to_string);
    }
    if let Some(profile_id) = profile_id {
        record.profile_id = profile_id
            .map(|profile_id| resolve_browser_profile_id_in(tx, profile_id))
            .transpose()?;
    }
    record.validate()?;
    if record == before {
        return Ok((record, false));
    }
    tx.execute(
        "UPDATE frontend_browser_tabs SET url = ?2, title = ?3, favicon_url = ?4, profile_id = ?5
         WHERE browser_id = ?1",
        params![browser_id, record.url, record.title, record.favicon_url, record.profile_id],
    )?;
    append_presentation_record(
        tx,
        "browser.frontend.updated",
        vec![browser_subject(browser_id)],
        &json!({"browser_id": browser_id, "browser": record}),
    )?;
    Ok((record, true))
}

#[cfg(test)]
#[path = "presentation_intent_tests.rs"]
mod tests;
