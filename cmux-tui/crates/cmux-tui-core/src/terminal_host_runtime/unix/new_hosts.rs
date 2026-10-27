//! New terminals run on the holder (`holder_runtime`). A terminal host
//! process starts for a new terminal only in the hosts model, the opt-out
//! kept for one release (`CMUX_TUI_TERMINAL_HOST_MODEL=hosts`, or
//! `terminalHolder` in the disable list). Hosts that already run, adopted
//! after a daemon restart or an upgrade, are not new terminals and stay; so
//! does the replacement of an adopted host on its held PTY master
//! (`adopt_launch`), which keeps that shell alive.
//!
//! The one spawn every new host goes through, [`super::StandbyTerminalHost`],
//! asks [`TerminalModel::admit_new_host`] first, so no create, split, spare
//! refill or restore path starts a host in the holder model.

use std::fmt;

/// The model new session-owned terminals run on, fixed for the process
/// (`holder_runtime::enabled`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalModel {
    /// The install's one holder process keeps every PTY: the default.
    Holder,
    /// One terminal host process per terminal: the opt-out.
    Hosts,
}

impl TerminalModel {
    pub(crate) fn current() -> Self {
        if crate::holder_runtime::enabled() { Self::Holder } else { Self::Hosts }
    }

    /// Ok only in the hosts model: a new terminal host process is frozen
    /// behind the opt-out.
    pub(crate) fn admit_new_host(self) -> Result<(), HostSpawnFrozen> {
        match self {
            Self::Hosts => Ok(()),
            Self::Holder => Err(HostSpawnFrozen),
        }
    }
}

/// A new terminal host process was refused: new terminals run on the holder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostSpawnFrozen;

impl fmt::Display for HostSpawnFrozen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "new terminals run on the terminal holder; a terminal host process starts only with \
             CMUX_TUI_TERMINAL_HOST_MODEL=hosts",
        )
    }
}

impl std::error::Error for HostSpawnFrozen {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_hosts_model_admits_a_new_terminal_host() {
        assert_eq!(TerminalModel::Hosts.admit_new_host(), Ok(()));
        assert_eq!(TerminalModel::Holder.admit_new_host(), Err(HostSpawnFrozen));
    }

    #[test]
    fn the_holder_model_starts_no_host_process() {
        let refused = super::super::StandbyTerminalHost::spawn_in(TerminalModel::Holder)
            .err()
            .expect("the holder model refuses a new terminal host process");
        assert_eq!(refused.downcast_ref::<HostSpawnFrozen>(), Some(&HostSpawnFrozen), "{refused}");
    }
}
