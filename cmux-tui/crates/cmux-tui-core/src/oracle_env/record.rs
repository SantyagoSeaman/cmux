//! The seeded owner's launch record in its session state directory, so an
//! owner that restarts on the same root continues its clock and its id
//! stream instead of starting both again (a restart step of an oracle case,
//! a crash walk).
//!
//! The record holds how many seeded owners have opened the directory and the
//! virtual clock's offset. An owner opens with the count as its ChaCha20
//! stream id (the 64-bit nonce of RFC 8439, so its blocks never repeat an
//! earlier launch's), writes the count plus one before it mints anything, and
//! resumes the clock at the offset, which it writes again on every advance.
//! A first launch has stream 0 and offset 0, so two fresh roots under one
//! seed still give the same bytes.

use cmux_threads::checked::fs as files;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The record's file name in the session state directory.
pub(super) const FILE: &str = "oracle-env.json";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Launch {
    /// Seeded owners that opened the directory before this one: this
    /// owner's id stream.
    pub(super) launches: u64,
    /// The virtual clock's offset from the fixture epoch.
    pub(super) clock_offset_ms: u64,
}

/// Where one owner writes its record.
#[derive(Debug)]
pub(super) struct RecordFile {
    path: PathBuf,
    /// The count this owner writes: its own launch included.
    launches: u64,
}

impl RecordFile {
    /// Reads the record of `directory` (none yet is the first launch),
    /// counts this launch in it and returns the launch the owner resumes.
    pub(super) fn open(directory: &Path) -> io::Result<(Self, Launch)> {
        let path = directory.join(FILE);
        let launch = match files::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Launch::default(),
            Err(error) => return Err(error),
        };
        let launches = launch.launches.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "the oracle launch count is exhausted")
        })?;
        let record = Self { path, launches };
        record.write(launch.clock_offset_ms)?;
        Ok((record, launch))
    }

    /// Writes the record with `clock_offset_ms`: a temporary file beside it
    /// (0600), synced, renamed over it, so a reader sees a whole record.
    pub(super) fn write(&self, clock_offset_ms: u64) -> io::Result<()> {
        let launch = Launch { launches: self.launches, clock_offset_ms };
        let bytes = serde_json::to_vec(&launch).map_err(io::Error::other)?;
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).map_err(io::Error::other)?;
        let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = self.path.with_file_name(format!(".{FILE}.{suffix}.tmp"));
        let result = (|| {
            let mut options = files::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = files::open_with(&options, &temporary)?;
            crate::request_path::note(crate::request_path::BlockingKind::FsWrite);
            file.write_all(&bytes)?;
            files::sync_data(&file)?;
            drop(file);
            files::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = files::remove_file(&temporary);
        }
        result
    }
}
