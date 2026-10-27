//! The owner's clock and id source: the system pair by default, the virtual
//! clock and a ChaCha20 stream keyed by the seed under the oracle.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;

/// RFC 8439 section 2.3.2: key 00..1f, block counter 1, nonce
/// 000000090000004a00000000.
#[test]
fn the_block_function_matches_the_rfc_8439_test_vector() {
    let mut key = [0_u32; 8];
    for (index, word) in key.iter_mut().enumerate() {
        let base = u8::try_from(index * 4).unwrap();
        *word = u32::from_le_bytes([base, base + 1, base + 2, base + 3]);
    }
    let block = chacha20_block(&key, [1, 0x0900_0000, 0x4a00_0000, 0]);
    let hex: String = block.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        hex,
        "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
         d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
    );
}

fn drawn(env: &OracleEnv, lengths: &[usize]) -> Vec<Vec<u8>> {
    lengths
        .iter()
        .map(|length| {
            let mut bytes = vec![0_u8; *length];
            env.ids().fill(&mut bytes).unwrap();
            bytes
        })
        .collect()
}

#[test]
fn one_seed_gives_one_id_stream_and_another_seed_another() {
    let lengths = [10, 16, 100, 16];
    let first = drawn(&OracleEnv::seeded("alpha/case-001"), &lengths);
    assert_eq!(first, drawn(&OracleEnv::seeded("alpha/case-001"), &lengths));
    assert_ne!(first, drawn(&OracleEnv::seeded("alpha/case-002"), &lengths));
    assert_ne!(first[1], first[3], "each fill takes fresh blocks");
    assert_ne!(first[2][..64], first[2][64..], "a long fill spans distinct blocks");
}

#[test]
fn clones_share_the_clock_and_the_id_stream() {
    let env = OracleEnv::seeded("shared");
    let clone = env.clone();
    let mut one = [0_u8; 16];
    let mut two = [0_u8; 16];
    env.ids().fill(&mut one).unwrap();
    clone.ids().fill(&mut two).unwrap();
    assert_ne!(one, two, "the clone continues the stream, not restarts it");
    env.virtual_clock().unwrap().advance(250).unwrap();
    assert_eq!(clone.wall().unix_ms(), FIXTURE_EPOCH_MS + 250);
}

#[test]
fn the_virtual_clock_starts_at_the_fixture_epoch_and_moves_only_when_advanced() {
    let env = OracleEnv::seeded("clock");
    let clock = env.virtual_clock().expect("a seeded env has a virtual clock");
    assert_eq!(env.wall().try_unix_ms(), Ok(FIXTURE_EPOCH_MS));
    assert_eq!(env.wall().unix_ms(), env.wall().unix_ms(), "no read moves it");
    assert_eq!(clock.advance(0), Ok(0));
    assert_eq!(clock.advance(1_500), Ok(1_500));
    assert_eq!(clock.advance(500), Ok(2_000));
    assert_eq!(env.wall().now(), UNIX_EPOCH + Duration::from_millis(FIXTURE_EPOCH_MS + 2_000));
    assert_eq!(clock.advance(u64::MAX), Err(ClockError::OutOfRange));
    assert_eq!(clock.offset_ms(), 2_000, "a refused move leaves the clock");
}

/// Without a seed the owner runs on the system pair: real time, OS ids.
#[test]
fn the_default_env_is_the_system_clock_and_random_source() {
    let env = OracleEnv::default();
    assert!(env.virtual_clock().is_none());
    let before = SystemTime::now();
    let now = env.wall().now();
    assert!(now >= before && now <= SystemTime::now(), "{now:?}");
    let ids = drawn(&env, &[16, 16]);
    assert_ne!(ids[0], ids[1]);
    let repeat = drawn(&OracleEnv::default(), &[16, 16]);
    assert_ne!(ids, repeat, "two system sources share no stream");
}
