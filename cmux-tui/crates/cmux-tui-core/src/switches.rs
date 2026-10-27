//! The owner's launch switches: one value the owner start parses from its
//! environment and passes by argument to the `Mux` and from there to the
//! stores (plans/cmux-next/chrome-parity/p15-oracle.md, tasks 020 and 185).
//! Nothing reads them from a static.
//!
//! Today the value carries the [`OracleEnv`]: the system clock and id source,
//! or, in debug and `--features oracle` builds started with
//! `CMUX_TUI_ORACLE_SEED`, the virtual clock and the seeded ids. A release
//! build never reads the seed, so its stores always run on the system clock;
//! the oracle tells the two apart with `oracle-advance-clock`, which only a
//! seeded oracle build accepts. Task 020 adds the refactor switch arms from
//! `CMUX_TUI_SWITCHES` to the same value.

use std::ffi::OsString;

use crate::oracle_env::OracleEnv;

/// The oracle's seed for the virtual clock and the seeded ids.
#[cfg(any(debug_assertions, feature = "oracle"))]
pub const ORACLE_SEED_ENV: &str = "CMUX_TUI_ORACLE_SEED";

/// The launch switches of one owner.
#[derive(Clone, Debug, Default)]
pub struct Switches {
    oracle: OracleEnv,
}

/// An owner environment that names switches this build cannot run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchesError {
    /// `CMUX_TUI_ORACLE_SEED` is set but empty or not UTF-8.
    BadOracleSeed,
}

impl std::fmt::Display for SwitchesError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadOracleSeed => {
                formatter.write_str("CMUX_TUI_ORACLE_SEED must be a non-empty UTF-8 string")
            }
        }
    }
}

impl std::error::Error for SwitchesError {}

impl Switches {
    /// The switches `lookup` (the owner's environment) names.
    #[cfg_attr(not(any(debug_assertions, feature = "oracle")), allow(unused_variables))]
    pub fn from_env(lookup: impl Fn(&str) -> Option<OsString>) -> Result<Self, SwitchesError> {
        #[cfg(any(debug_assertions, feature = "oracle"))]
        if let Some(seed) = lookup(ORACLE_SEED_ENV) {
            let seed = seed.into_string().map_err(|_| SwitchesError::BadOracleSeed)?;
            if seed.is_empty() {
                return Err(SwitchesError::BadOracleSeed);
            }
            return Ok(Self::seeded(&seed));
        }
        Ok(Self::default())
    }

    /// The oracle switches of `seed`: the virtual clock and the seeded ids.
    #[cfg(any(debug_assertions, feature = "oracle"))]
    pub(crate) fn seeded(seed: &str) -> Self {
        Self { oracle: OracleEnv::seeded(seed) }
    }

    pub(crate) fn oracle_env(&self) -> &OracleEnv {
        &self.oracle
    }
}

#[cfg(all(test, any(debug_assertions, feature = "oracle")))]
#[path = "switches_tests.rs"]
mod tests;
