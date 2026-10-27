//! Terminal moves between workspaces and their projection into the destination workspace state.

use super::*;
use crate::workspace_registry::{RegistryIntent, RegistryReceipt, TerminalRegistryCommit};

impl Mux {
    #[allow(clippy::too_many_arguments)]
    #[track_caller]
    pub fn move_terminal_with_mutation(
        &self,
        terminal_id: &str,
        workspace_key: &str,
        expected_incarnation: Option<&str>,
        expected_generation: Option<&str>,
        expected_revision: Option<u64>,
        mutation: &WorkspaceMutation,
    ) -> anyhow::Result<TerminalMoveResult> {
        validate_terminal_hex(terminal_id, "invalid_terminal_id")?;
        if let Some(incarnation) = expected_incarnation {
            validate_terminal_hex(incarnation, "invalid_terminal_incarnation")?;
        }
        let fingerprint = serde_json::json!({
            "op":"move-terminal",
            "terminal_id":terminal_id,
            "workspace_key":workspace_key,
            "incarnation":expected_incarnation,
        });
        let (terminal, terminal_revision, replayed, changed, placement, topology_changed) = {
            let mut registry = self.workspace_registry.lock().unwrap();
            // Registry -> state is the global writer order. Holding both from
            // canonical commit through projection prevents move B / move C
            // from projecting C and then stale B, and serializes moves with a
            // concurrent workspace close.
            let mut state = self.lock_state_pinned(&registry).unwrap();
            if let Some(replay) = registry.replay_terminal(mutation, &fingerprint)? {
                let terminal = registry
                    .terminal_record(terminal_id)?
                    .ok_or_else(|| anyhow::anyhow!("unknown terminal {terminal_id}"))?;
                let current_revision = registry.terminal_revision()?;
                let changed = replay.result["changed"].as_bool().unwrap_or(true);
                #[cfg(test)]
                if let Some(hook) = self.terminal_move_before_projection.lock().unwrap().clone() {
                    hook();
                }
                let (placement, topology_changed) =
                    if terminal.lifecycle == TerminalLifecycle::Tombstoned {
                        (None, false)
                    } else {
                        self.project_terminal_to_workspace_in_state(
                            &mut state,
                            terminal_id,
                            &terminal.workspace_key,
                        )?
                    };
                if topology_changed {
                    self.commit_full_resource_projection_locked(
                        &mut registry,
                        &mut state,
                        &mutation.actor,
                        "terminal.move",
                    )?;
                }
                (terminal, current_revision, true, changed, placement, topology_changed)
            } else {
                let mut terminal = registry
                    .terminal_record(terminal_id)?
                    .ok_or_else(|| anyhow::anyhow!("unknown terminal {terminal_id}"))?;
                if terminal.lifecycle == TerminalLifecycle::Tombstoned {
                    anyhow::bail!("terminal is already closed");
                }
                if let Some(expected) = expected_incarnation
                    && terminal.incarnation.as_deref() != Some(expected)
                {
                    anyhow::bail!("terminal_incarnation_mismatch");
                }
                let changed = terminal.workspace_key != workspace_key;
                terminal.workspace_key = workspace_key.to_string();
                let current_revision = registry.terminal_revision()?;
                check_terminal_move(
                    &registry,
                    &state,
                    expected_generation,
                    expected_revision,
                    current_revision,
                    &terminal,
                    changed,
                )?;
                let result = serde_json::json!({
                    "terminal_id":terminal_id,
                    "workspace_key":workspace_key,
                    "incarnation":terminal.incarnation,
                    "state":terminal.lifecycle,
                    "changed":changed,
                });
                let intent = registry.prepare_terminal_commit(
                    mutation,
                    &fingerprint,
                    expected_generation,
                    Some(current_revision),
                    "terminal-moved",
                    &terminal,
                    &result,
                )?;
                let prepared = prepared_terminal_move(&terminal, current_revision, result)?;
                #[cfg(test)]
                if let Some(hook) = self.terminal_move_before_projection.lock().unwrap().clone() {
                    hook();
                }
                let (placement, topology_changed) = self.project_terminal_to_workspace_in_state(
                    &mut state,
                    terminal_id,
                    &terminal.workspace_key,
                )?;
                // The projection moved the terminal's view between panes;
                // the resource topology must record that move under the
                // same locks, or a restore reverts it and later tab moves
                // plan from a placement that memory no longer has. It needs
                // the connection pin, which the admission below releases.
                if topology_changed {
                    self.commit_full_resource_projection_locked(
                        &mut registry,
                        &mut state,
                        &mutation.actor,
                        "terminal.move",
                    )?;
                }
                // Admit then reply (F10): the writer commits the row later,
                // in admit order; the event goes out at admit.
                let commit = registry
                    .admit_registry_intent(
                        RegistryIntent::Terminal(intent),
                        RegistryReceipt::Terminal(prepared),
                        || state.unpin(),
                        None,
                    )?
                    .into_receipt()
                    .into_terminal()?;
                self.emit_terminal_registry_changed(&registry, commit.revision);
                (terminal, commit.revision, false, changed, placement, topology_changed)
            }
        };
        if placement.is_some()
            && let Some(surface) = placement.and_then(|placement| self.surface(placement.surface))
        {
            let _ = surface.persist_host_workspace(&terminal.workspace_key);
        }
        if topology_changed {
            self.publish_resource_event();
            self.publish_pending_terminal_directories();
            self.emit(MuxEvent::TreeChanged);
        }
        Ok(TerminalMoveResult { placement, terminal, terminal_revision, replayed, changed })
    }

    pub(super) fn project_terminal_to_workspace_in_state(
        &self,
        state: &mut State,
        terminal_id: &str,
        workspace_key: &str,
    ) -> anyhow::Result<(Option<RunPlacement>, bool)> {
        let Some(runtime) = self.catalog_terminal_by_host(state, terminal_id)? else {
            // Still validate the in-memory projection while both writer locks
            // are held; a missing destination indicates registry/state drift.
            if state.workspaces.iter().all(|workspace| workspace.key != workspace_key) {
                anyhow::bail!("unknown workspace key {workspace_key}");
            }
            return Ok((None, false));
        };
        let surface = terminal_placement_for_runtime(state, &runtime);
        let Some(surface) = surface else {
            // A terminal with no view remains alive. Creating another view is
            // an explicit `terminal.project` operation, not an implicit move.
            return Ok((None, false));
        };
        let active_at = self.next_active_at();
        let preserved_focus = current_focus_identity(state);
        let destination = state
            .workspaces
            .iter()
            .position(|workspace| workspace.key == workspace_key)
            .ok_or_else(|| anyhow::anyhow!("unknown workspace key {workspace_key}"))?;
        if let Some(current) = run_placement_for_surface(state, surface)
            && current.workspace == state.workspaces[destination].id
        {
            return Ok((Some(current), false));
        }
        let target_pane = if let Some(pane) =
            state.workspaces[destination].active_screen_ref().map(|screen| screen.active_pane)
        {
            pane
        } else {
            let pane = self.next_id();
            let screen = self.next_id();
            state.insert_pane(Pane {
                id: pane,
                public_id: PanePublicId::random()?,
                name: None,
                tabs: Vec::new(),
                active_tab: 0,
                active_at,
                // The projection preserves the user's existing focus
                // identity below; this destination starts unfocused.
                focused_at: 0,
            });
            state.workspaces[destination].screens.push(Screen {
                id: screen,
                public_id: ScreenPublicId::random()?,
                name: None,
                root: Node::Leaf(pane),
                active_pane: pane,
                zoomed_pane: None,
                creation_order_auto_layout: Some(vec![pane]),
                viewport_splits: Default::default(),
                viewport_base_width: None,
                layout_columns: Vec::new(),
                layout_revision: 0,
                layout_undo: Default::default(),
            });
            state.workspaces[destination].active_screen = 0;
            pane
        };
        if state.pane_of(surface).is_some() {
            let (moved, topology_changed) =
                move_tab_in_state(self, state, surface, target_pane, usize::MAX);
            if !moved {
                anyhow::bail!("terminal topology changed during move");
            }
            if topology_changed {
                Self::rebuild_split_screen_index(state);
            }
        } else {
            let pane = state
                .panes
                .get_mut(&target_pane)
                .ok_or_else(|| anyhow::anyhow!("destination pane disappeared"))?;
            pane.tabs.push(surface);
            pane.active_tab = pane.tabs.len() - 1;
            pane.active_at = active_at;
            state.resource_indexes.tab_pane.insert(surface, target_pane);
            state.register_placed_tab_identity(surface);
            fence_layout_undo_for_tab_membership(state, &[target_pane]);
        }
        restore_focus_identity(state, preserved_focus);
        let placement = run_placement_for_surface(state, surface)
            .ok_or_else(|| anyhow::anyhow!("terminal move did not produce a binding"))?;
        Ok((Some(placement), true))
    }
}

/// The `terminal-moved` checks the journal writer runs at apply, run at
/// prepare so the admitted reply is what the writer commits (F10,
/// p05-ipc-123): generation, revision CAS, live destination. The caller
/// holds the registry and the pinned state, so no other terminal row or
/// workspace close commits in between.
fn check_terminal_move(
    registry: &WorkspaceRegistry,
    state: &State,
    expected_generation: Option<&str>,
    expected_revision: Option<u64>,
    current_revision: u64,
    terminal: &RegistryTerminal,
    changed: bool,
) -> anyhow::Result<()> {
    if let Some(expected) = expected_generation
        && expected != registry.generation()
    {
        anyhow::bail!(
            "terminal generation conflict: expected {expected}, current {}",
            registry.generation()
        );
    }
    if let Some(expected) = expected_revision
        && expected != current_revision
    {
        anyhow::bail!(
            "terminal revision conflict: expected {expected}, current {current_revision}"
        );
    }
    let key = terminal.workspace_key.as_str();
    if changed && !detached_terminals::is_detached_key(key) && state.workspace_by_key(key).is_none()
    {
        anyhow::bail!("terminal workspace is missing or closed: {key}");
    }
    Ok(())
}

/// The receipt the writer gives a checked `terminal-moved` intent at
/// `current_revision`: the next revision, or for an exited terminal the exit
/// latch's replayed receipt (`TerminalCommitIntent::apply`).
fn prepared_terminal_move(
    terminal: &RegistryTerminal,
    current_revision: u64,
    result: Value,
) -> anyhow::Result<TerminalRegistryCommit> {
    if terminal.lifecycle == TerminalLifecycle::Exited {
        return Ok(TerminalRegistryCommit { revision: current_revision, result, replayed: true });
    }
    let revision = current_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("terminal revision exhausted"))?;
    Ok(TerminalRegistryCommit { revision, result, replayed: false })
}

#[cfg(test)]
#[path = "terminal_move_tests.rs"]
mod tests;
