//! The registry commit of a terminal lifecycle change (moved out of mux.rs).

use super::*;
use crate::workspace_registry::RegistryIntent;

/// Advance only renderer lifecycle fields from the latest row. This
/// deliberately re-reads under the registry mutex and uses the terminal
/// revision as a CAS: a GUI move committed while a host launch or adoption
/// was in flight can never be overwritten by a stale row clone.
///
/// The row is admitted, not waited for (F10, p05-ipc-120): the journal
/// writer commits it later in admit order, so a later tombstone lands after
/// it. What the writer would refuse or latch is decided here instead, so the
/// prepare-time receipt is the one the writer commits: a tombstoned row
/// takes no later lifecycle, and a second exit keeps the first.
#[track_caller]
pub(super) fn commit_terminal_lifecycle(
    registry: &mut WorkspaceRegistry,
    event_kind: &str,
    operation: &str,
    terminal_id: &str,
    lifecycle: TerminalLifecycle,
    incarnation: Option<&str>,
    exit: Option<Value>,
) -> anyhow::Result<(RegistryTerminal, u64)> {
    // Only the revision fence is needed: reading every terminal row here made
    // each launch O(terminals) under the registry lock.
    let (generation, revision) = (registry.generation().to_string(), registry.terminal_revision()?);
    let mut terminal = registry
        .terminal_record(terminal_id)?
        .ok_or_else(|| anyhow::anyhow!("unknown terminal {terminal_id}"))?;
    match terminal.lifecycle {
        TerminalLifecycle::Tombstoned => {
            anyhow::bail!("tombstoned terminal id cannot be reused: {terminal_id}");
        }
        // Exit is a latch (TerminalCommitIntent::apply): the first observed
        // exit stays and no revision is minted.
        TerminalLifecycle::Exited if lifecycle == TerminalLifecycle::Exited => {
            if incarnation
                .is_some_and(|incarnation| terminal.incarnation.as_deref() != Some(incarnation))
            {
                anyhow::bail!("terminal_incarnation_mismatch");
            }
            return Ok((terminal, revision));
        }
        _ => {}
    }
    terminal.lifecycle = lifecycle;
    if let Some(incarnation) = incarnation {
        terminal.incarnation = Some(incarnation.to_string());
    }
    terminal.exit = exit;
    let mutation = WorkspaceMutation::daemon_local("cmux-tui-runtime");
    let intent = registry.prepare_terminal_commit(
        &mutation,
        &serde_json::json!({
            "op": operation,
            "terminal_id": terminal.terminal_id,
            "incarnation": terminal.incarnation,
            "lifecycle": terminal.lifecycle,
        }),
        Some(&generation),
        Some(revision),
        event_kind,
        &terminal,
        &serde_json::json!({
            "terminal_id": terminal.terminal_id,
            "workspace_key": terminal.workspace_key,
            "incarnation": terminal.incarnation,
            "state": terminal.lifecycle,
        }),
    )?;
    let commit =
        registry.admit_and_enqueue(RegistryIntent::Terminal(intent))?.receipt.into_terminal()?;
    Ok((terminal, commit.revision))
}

/// Each terminal's own result, then the last revision committed, if any.
pub(super) type AdoptingCommit = (Vec<Result<(), String>>, Option<u64>);

/// Mark every terminal in `terminals` adopting with its host's incarnation
/// in one registry transaction: one commit each cost a daemon start 2.6 ms
/// per surviving host before its first answer
/// (plans/cmux-next/service-start-PERF.md 5b). Each result is that
/// terminal's own; a failed one keeps its row. Returns the last revision
/// committed, if any.
#[track_caller]
pub(super) fn commit_terminals_adopting(
    registry: &mut WorkspaceRegistry,
    terminals: &[(RegistryTerminal, String)],
) -> anyhow::Result<AdoptingCommit> {
    let generation = registry.generation().to_string();
    let mut intents = Vec::with_capacity(terminals.len());
    for (terminal, incarnation) in terminals {
        // One mutation each: a shared mutation id would replay the first.
        let mutation = WorkspaceMutation::daemon_local("cmux-tui-runtime");
        let mut terminal = terminal.clone();
        terminal.lifecycle = TerminalLifecycle::Adopting;
        terminal.incarnation = Some(incarnation.clone());
        terminal.exit = None;
        intents.push(registry.prepare_terminal_commit(
            &mutation,
            &serde_json::json!({
                "op": "adopt-terminal",
                "terminal_id": terminal.terminal_id,
                "incarnation": terminal.incarnation,
                "lifecycle": terminal.lifecycle,
            }),
            Some(&generation),
            None,
            "terminal-adopting",
            &terminal,
            &serde_json::json!({
                "terminal_id": terminal.terminal_id,
                "workspace_key": terminal.workspace_key,
                "incarnation": terminal.incarnation,
                "state": terminal.lifecycle,
            }),
        )?);
    }
    let mut last_revision = None;
    let results = registry
        .commit_terminals_together(intents)?
        .into_iter()
        .map(|commit| {
            commit.map(|commit| {
                last_revision = Some(commit.revision);
            })
        })
        .collect();
    Ok((results, last_revision))
}
