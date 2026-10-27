//! Where the session owner's panic log lives (cx-urd.59). The owner's panic
//! hook appends to it (cmux-tui `owner_start/owner_panic.rs`) and
//! `oracle-state` reports it, so both read the one path rule here. The
//! owner's hang log lives beside it; the request watchdog writes it
//! (server/request_watchdog.rs). So does its bad-message log, apart from the
//! panics so the app never sends a peer's fault as an owner crash.

use std::path::PathBuf;

/// The log for `session`: `owner-panics-<session>.jsonl` at the state root
/// (the parent of the sessions directory). A session name that needs
/// escaping gets a hash suffix, so two names never share a log.
pub fn owner_panic_log_path(session: &str) -> Option<PathBuf> {
    owner_log_path("owner-panics", session)
}

/// The hang log for `session`: `owner-hangs-<session>.jsonl` beside the
/// panic log. The owner's hang watcher (cmux-panic-log `hang`) appends one
/// line per scope busy past its hang tier.
pub fn owner_hang_log_path(session: &str) -> Option<PathBuf> {
    owner_log_path("owner-hangs", session)
}

/// The bad-message log for `session`: `owner-bad-messages-<session>.jsonl`
/// beside the panic log. One line per reason per launch for a client the
/// owner closed for breaking the wire (cmux-panic-log `bad_message`). It is
/// not a panic: the app's panic forwarder never reads it.
pub fn owner_bad_message_log_path(session: &str) -> Option<PathBuf> {
    owner_log_path("owner-bad-messages", session)
}

fn owner_log_path(prefix: &str, session: &str) -> Option<PathBuf> {
    let sessions = crate::platform::workspace_state_dir()?;
    let root = sessions.parent().map(PathBuf::from).unwrap_or(sessions);
    let safe: String = session
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    let name = if safe == session {
        safe
    } else {
        // FNV-1a: stable across builds, no dependency.
        let hash = session.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        });
        format!("{safe}-{:08x}", hash as u32)
    };
    Some(root.join(format!("{prefix}-{name}.jsonl")))
}

/// Debug builds only: block a thread in a scope of the process's request
/// watchdog past its 500 ms hang tier, so its hang is written to the hang
/// log (cmux-tui tests/owner_panic.rs).
#[cfg(debug_assertions)]
pub fn spawn_owner_hang_test() {
    crate::server::spawn_owner_hang_test();
}

#[cfg(test)]
#[path = "owner_panic_log_tests.rs"]
mod tests;
