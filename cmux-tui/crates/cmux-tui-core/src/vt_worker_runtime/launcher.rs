//! Starting a shard's worker process.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use cmux_process::{Command, Stdio};

/// The descriptor number of the worker's link socket in the worker.
pub const WORKER_FD: RawFd = 3;
/// The env entry that tells a worker its link descriptor ([`WORKER_FD`]).
pub const WORKER_FD_ENV: &str = "CMUX_VT_WORKER_FD";
/// The env entry that tells a worker its shard (for its logs and crash
/// reports; the broker's HELLO says it too).
pub const WORKER_SHARD_ENV: &str = "CMUX_VT_WORKER_SHARD";

/// A started worker, as the broker owns it.
pub trait WorkerProcess: Send {
    /// Its process id (0 when it is not a process: a test's stand-in).
    fn pid(&self) -> u32;
    /// End it now (SIGKILL). One that already ended is no error.
    fn kill(&mut self);
    /// Wait for it after [`Self::kill`]: it ends at once, so this is short.
    fn reap(&mut self);
}

/// A worker and the broker's end of its link.
pub struct LaunchedWorker {
    pub link: UnixStream,
    pub process: Box<dyn WorkerProcess>,
}

/// How the broker starts a shard's worker: [`ProcessLauncher`], or a
/// test's stand-in.
pub trait WorkerLauncher: Send + 'static {
    fn launch(&mut self, shard: usize) -> io::Result<LaunchedWorker>;
}

/// Starts `cmux-vt-worker` processes through `cmux_process`: the child gets
/// descriptors 0-2 (null, null, the daemon's stderr) and its link socket at
/// [`WORKER_FD`], nothing else, and an environment of only the entries
/// named here. Rings reach it later, one descriptor per ASSIGN.
pub struct ProcessLauncher {
    exe: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

impl ProcessLauncher {
    pub fn new(exe: impl Into<PathBuf>) -> Self {
        Self { exe: exe.into(), args: Vec::new(), env: Vec::new() }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

impl WorkerLauncher for ProcessLauncher {
    fn launch(&mut self, shard: usize) -> io::Result<LaunchedWorker> {
        let (ours, theirs) = UnixStream::pair()?;
        cmux_holder::wire::tune_socket(ours.as_raw_fd());
        cmux_holder::wire::tune_socket(theirs.as_raw_fd());
        let mut command = Command::new(&self.exe);
        command
            .args(&self.args)
            .env_clear()
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .env(WORKER_FD_ENV, WORKER_FD.to_string())
            .env(WORKER_SHARD_ENV, shard.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .isolate();
        command.inherit_fd(theirs.as_fd(), WORKER_FD)?;
        let child = command.spawn()?;
        // The child's end lives on only in the child: its exit ends the link.
        drop(theirs);
        Ok(LaunchedWorker { link: ours, process: Box::new(SpawnedWorker(child)) })
    }
}

struct SpawnedWorker(cmux_process::Child);

impl WorkerProcess for SpawnedWorker {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn kill(&mut self) {
        // Unreaped until `reap`, so the pid cannot have been reused.
        let _ = self.0.kill();
    }

    fn reap(&mut self) {
        let _ = self.0.wait();
    }
}
