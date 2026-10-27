//! `Switches::from_env`: no seed is the system clock; a seed is the oracle's
//! virtual clock and seeded ids; a bad seed is a start error.

use std::collections::BTreeMap;
use std::ffi::OsString;

use super::*;

fn parse(env: &[(&str, OsString)]) -> Result<Switches, SwitchesError> {
    let env: BTreeMap<_, _> = env.iter().cloned().collect();
    Switches::from_env(|name| env.get(name).cloned())
}

#[test]
fn no_seed_runs_the_system_clock() {
    let switches = parse(&[("CMUX_TUI_SWITCHES", "".into())]).unwrap();
    assert!(switches.oracle_env().virtual_clock().is_none());
}

#[test]
fn a_seed_runs_the_virtual_clock_and_the_seeded_ids() {
    let switches = parse(&[(ORACLE_SEED_ENV, "alpha/case-001".into())]).unwrap();
    assert!(switches.oracle_env().virtual_clock().is_some());
    let draw = |switches: &Switches| {
        let mut bytes = [0_u8; 16];
        switches.oracle_env().ids().fill(&mut bytes).unwrap();
        bytes
    };
    assert_eq!(draw(&switches), draw(&Switches::seeded("alpha/case-001")));
}

#[test]
fn an_empty_or_non_utf8_seed_is_a_start_error() {
    assert_eq!(parse(&[(ORACLE_SEED_ENV, "".into())]).unwrap_err(), SwitchesError::BadOracleSeed);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let bytes = OsString::from_vec(vec![0x66, 0xff, 0x6f]);
        assert_eq!(parse(&[(ORACLE_SEED_ENV, bytes)]).unwrap_err(), SwitchesError::BadOracleSeed);
    }
}
