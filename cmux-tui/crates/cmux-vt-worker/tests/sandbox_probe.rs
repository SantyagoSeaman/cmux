//! The worker's sandbox, applied as the worker applies it, denies what a
//! compromised parser would reach for: `open()`, `fork()` and
//! `socket(AF_INET)` everywhere; `bootstrap_look_up("com.apple.pasteboard.1")`
//! on macOS (Seatbelt); `execve`, signals to other processes, reading the
//! parent's limits, and `mkdir`, `unlink`, `rename`, `truncate` and `chmod`
//! on Linux (seccomp-bpf, with and without landlock: a kernel without
//! landlock gets seccomp alone, so the probe applies both). It still allows
//! what the worker needs: mapping a segment it was given read-only, writing
//! to the socket it holds and, on Linux, a new thread. Every check also runs before the sandbox and must succeed there,
//! so a denial is the sandbox's; on macOS notifyd's lookup is the Mach
//! control where an SSH session cannot reach the pasteboard at all.
//!
//! A sandbox is for the life of the process, so the probe runs in a child:
//! the test re-runs its own binary with only `probe_child` selected and
//! [`PROBE_ENV`] set (Chrome's `MultiProcessTest` pattern in
//! `sandbox/mac/sandbox_mac_seatbelt_exec_unittest.cc`), and reads one
//! result line per check.

#![cfg(any(
    target_os = "macos",
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))
))]

use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use cmux_process::{Command, Stdio};
use cmux_vt_worker::ring_view::RingView;
use cmux_vt_worker::sandbox::{self, SandboxMechanism, SandboxPolicy};

const PROBE_ENV: &str = "CMUX_VT_WORKER_SANDBOX_PROBE";
/// A private directory the parent made, where the Linux probe changes files.
const SCRATCH_ENV: &str = "CMUX_VT_WORKER_SANDBOX_SCRATCH";

/// `open("/etc/hosts")` succeeded.
fn can_open() -> bool {
    // SAFETY: open with a NUL-terminated path; the descriptor is closed.
    let fd = unsafe { libc::open(c"/etc/hosts".as_ptr(), libc::O_RDONLY) };
    if fd >= 0 {
        // SAFETY: closes the descriptor just opened.
        unsafe { libc::close(fd) };
    }
    fd >= 0
}

/// `socket(AF_INET, SOCK_STREAM)` succeeded.
fn can_open_inet_socket() -> bool {
    // SAFETY: socket takes no pointers; the descriptor is closed.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd >= 0 {
        // SAFETY: closes the descriptor just made.
        unsafe { libc::close(fd) };
    }
    fd >= 0
}

/// `fork()` succeeded (the child exits at once).
fn can_fork() -> bool {
    // SAFETY: the child calls only `_exit`, which is async-signal-safe.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // SAFETY: ends the forked child without running any handler.
        unsafe { libc::_exit(0) };
    }
    if pid > 0 {
        let mut status = 0;
        // SAFETY: reaps the child just forked.
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
    pid > 0
}

/// The values of `key=bool` pairs after `prefix` on its line (libtest may
/// print `test probe_child ... ` before the probe's first line).
fn checks(stdout: &str, prefix: &str) -> BTreeMap<String, bool> {
    let line = stdout
        .lines()
        .find_map(|line| line.find(prefix).map(|at| &line[at + prefix.len()..]))
        .unwrap_or_else(|| panic!("no {prefix:?} line in {stdout}"));
    line.split_whitespace()
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_owned(), value == "true"))
        .collect()
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{CStr, c_char, c_int};

    use super::{
        BTreeMap, SandboxMechanism, SandboxPolicy, can_fork, can_open, can_open_inet_socket,
    };

    unsafe extern "C" {
        static bootstrap_port: u32;
        fn bootstrap_look_up(port: u32, name: *const c_char, service: *mut u32) -> c_int;
    }

    /// `bootstrap_look_up(name)` found the service.
    fn can_look_up(name: &CStr) -> bool {
        let mut service = 0u32;
        // SAFETY: a NUL-terminated name and a writable out-parameter;
        // reading the process's bootstrap port global.
        let result = unsafe { bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut service) };
        result == 0
    }

    /// The pasteboard server: only in a GUI (Aqua) bootstrap session, so an
    /// SSH session may not reach it even unsandboxed.
    const PASTEBOARD: &CStr = c"com.apple.pasteboard.1";
    /// notifyd, in the system bootstrap every session inherits: it resolves
    /// unsandboxed everywhere, so its denial proves the profile denies
    /// `mach-lookup` even where the pasteboard check proves nothing.
    const NOTIFYD: &CStr = c"com.apple.system.notification_center";

    pub fn cases() -> Vec<(&'static str, Vec<SandboxMechanism>)> {
        vec![("compiled", vec![SandboxMechanism::Seatbelt])]
    }

    pub fn policy(_case: &str) -> SandboxPolicy {
        cmux_vt_worker::sandbox::compile_policy().expect("compile")
    }

    pub fn prepare() {}

    pub fn probe(_phase: &str) -> String {
        format!(
            "open={} fork={} socket={} pasteboard={} notifyd={}",
            can_open(),
            can_fork(),
            can_open_inet_socket(),
            can_look_up(PASTEBOARD),
            can_look_up(NOTIFYD)
        )
    }

    pub fn judge(before: &BTreeMap<String, bool>, after: &BTreeMap<String, bool>, stdout: &str) {
        // The pasteboard may be out of reach of a non-GUI (SSH) session;
        // notifyd never is, so the Mach check always has a positive control.
        for check in ["open", "fork", "socket", "notifyd"] {
            assert_eq!(before.get(check), Some(&true), "{check} before the sandbox: {stdout}");
        }
        for check in ["open", "fork", "socket", "pasteboard", "notifyd"] {
            assert_eq!(after.get(check), Some(&false), "{check} after the sandbox: {stdout}");
        }
        if before.get("pasteboard") != Some(&true) {
            eprintln!(
                "the pasteboard is not in this bootstrap session; notifyd proves the Mach denial"
            );
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use super::{
        BTreeMap, SCRATCH_ENV, SandboxMechanism, SandboxPolicy, can_fork, can_open,
        can_open_inet_socket,
    };

    /// The policies the probe applies: the daemon's own choice, and seccomp
    /// alone, which is what a kernel without landlock gets.
    pub fn cases() -> Vec<(&'static str, Vec<SandboxMechanism>)> {
        vec![
            ("compiled", vec![SandboxMechanism::Seccomp, SandboxMechanism::SeccompLandlock]),
            ("seccomp", vec![SandboxMechanism::Seccomp]),
        ]
    }

    pub fn policy(case: &str) -> SandboxPolicy {
        match case {
            "seccomp" => SandboxPolicy::Seccomp { landlock: false },
            _ => cmux_vt_worker::sandbox::compile_policy().expect("compile"),
        }
    }

    /// A file of the parent's scratch directory, named for the phase.
    fn scratch(phase: &str, name: &str) -> Option<PathBuf> {
        let scratch = std::env::var_os(SCRATCH_ENV)?;
        Some(Path::new(&scratch).join(format!("{phase}-{name}")))
    }

    fn c_path(path: &Path) -> Option<CString> {
        CString::new(path.as_os_str().as_bytes()).ok()
    }

    /// The files each phase's checks change, made before the sandbox.
    pub fn prepare() {
        for phase in ["before", "after"] {
            for name in ["unlink", "rename", "truncate", "chmod"] {
                let path = scratch(phase, name).expect("scratch directory");
                std::fs::write(path, b"cmux").expect("scratch file");
            }
        }
    }

    /// The path call `call` on the phase's file `name` succeeded.
    fn can_change(phase: &str, name: &str, call: impl FnOnce(&CString) -> libc::c_int) -> bool {
        scratch(phase, name).as_deref().and_then(c_path).is_some_and(|path| call(&path) == 0)
    }

    /// `execve` reached the path lookup (anything but `EPERM` from the
    /// filter): the path does not exist, so nothing runs either way.
    fn can_exec() -> bool {
        let argv = [c"cmux-probe".as_ptr(), std::ptr::null()];
        let envp = [std::ptr::null()];
        // SAFETY: NUL-terminated path, argv and envp arrays that end in
        // null; the call fails (no such file, or the filter).
        unsafe {
            libc::execve(
                c"/nonexistent/cmux-vt-worker-probe".as_ptr(),
                argv.as_ptr(),
                envp.as_ptr(),
            )
        };
        std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
    }

    /// Signal 0 (an existence check) to the parent test process.
    fn can_signal_parent() -> bool {
        // SAFETY: getppid and kill with signal 0 deliver nothing.
        unsafe { libc::kill(libc::getppid(), 0) == 0 }
    }

    /// `prlimit64` read the parent's descriptor limit (it sets nothing).
    fn can_read_parent_limit() -> bool {
        let mut limit = libc::rlimit64 { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: no new limit (null) and a writable out-parameter.
        unsafe {
            libc::prlimit64(libc::getppid(), libc::RLIMIT_NOFILE, std::ptr::null(), &mut limit) == 0
        }
    }

    /// A new thread (`clone` with `CLONE_THREAD`) started and ran.
    fn can_spawn_thread() -> bool {
        std::thread::Builder::new().spawn(|| 7).is_ok_and(|thread| thread.join().ok() == Some(7))
    }

    pub fn probe(phase: &str) -> String {
        // SAFETY (each call below): NUL-terminated paths in the parent's
        // scratch directory.
        let mkdir = can_change(phase, "dir", |path| unsafe { libc::mkdir(path.as_ptr(), 0o700) });
        let unlink = can_change(phase, "unlink", |path| unsafe { libc::unlink(path.as_ptr()) });
        let renamed = scratch(phase, "renamed").as_deref().and_then(c_path);
        let rename = can_change(phase, "rename", |path| match &renamed {
            Some(to) => unsafe { libc::rename(path.as_ptr(), to.as_ptr()) },
            None => -1,
        });
        let truncate =
            can_change(phase, "truncate", |path| unsafe { libc::truncate(path.as_ptr(), 0) });
        let chmod = can_change(phase, "chmod", |path| unsafe { libc::chmod(path.as_ptr(), 0o600) });
        format!(
            "open={} fork={} socket={} exec={} signal={} prlimit={} mkdir={mkdir} \
             unlink={unlink} rename={rename} truncate={truncate} chmod={chmod} thread={}",
            can_open(),
            can_fork(),
            can_open_inet_socket(),
            can_exec(),
            can_signal_parent(),
            can_read_parent_limit(),
            can_spawn_thread()
        )
    }

    /// Denied with seccomp alone, landlock or not: Chrome's baseline
    /// denies the file-system set by syscall, and landlock never covers
    /// chmod.
    const DENIED: [&str; 11] = [
        "open", "fork", "socket", "exec", "signal", "prlimit", "mkdir", "unlink", "rename",
        "truncate", "chmod",
    ];

    pub fn judge(before: &BTreeMap<String, bool>, after: &BTreeMap<String, bool>, stdout: &str) {
        for check in DENIED.iter().chain(&["thread"]) {
            assert_eq!(before.get(*check), Some(&true), "{check} before the sandbox: {stdout}");
        }
        for check in DENIED {
            assert_eq!(after.get(check), Some(&false), "{check} after the sandbox: {stdout}");
        }
        assert_eq!(after.get("thread"), Some(&true), "a thread after the sandbox: {stdout}");
    }
}

#[test]
fn probe_child() {
    let Some(case) = std::env::var_os(PROBE_ENV) else { return };
    let case = case.to_string_lossy().into_owned();
    // What the worker gets before its sandbox: a ring and its socket.
    let ring = cmux_holder::ring::Ring::new(0, 4096);
    let segment = ring.segment().expect("shared ring").try_clone().expect("dup ring");
    let (socket, peer) = UnixStream::pair().expect("socketpair");
    platform::prepare();
    // The same checks before the sandbox: each "after" denial below means
    // something only where the call works without it.
    println!("before {}", platform::probe("before"));

    let policy = platform::policy(&case);
    assert!(matches!(policy, SandboxPolicy::Seatbelt(_) | SandboxPolicy::Seccomp { .. }));
    let mechanism = sandbox::apply(&policy).expect("apply");
    assert_eq!(mechanism, policy.mechanism());
    println!("mechanism={mechanism:?}");

    println!("after {}", platform::probe("after"));
    let mapped = RingView::open(segment, 4096).is_ok();
    // SAFETY: writes one byte from a live local to a live socket.
    let sent = unsafe { libc::write(socket.as_raw_fd(), b"x".as_ptr().cast(), 1) } == 1;
    drop(peer);
    println!("needs map={mapped} socket_write={sent}");
}

#[test]
fn the_worker_sandbox_denies_files_processes_network_and_services() {
    for (case, mechanisms) in platform::cases() {
        let scratch = std::env::temp_dir()
            .join(format!("cmux-vt-worker-probe-{}-{case}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("scratch directory");
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "probe_child", "--nocapture", "--test-threads=1"])
            .env(PROBE_ENV, case)
            .env(SCRATCH_ENV, &scratch)
            .stdin(Stdio::null())
            .output()
            .expect("run the probe");
        let _ = std::fs::remove_dir_all(&scratch);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{case} probe failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            mechanisms.iter().any(|mechanism| stdout.contains(&format!("mechanism={mechanism:?}"))),
            "{case}: one of {mechanisms:?} applied: {stdout}"
        );
        platform::judge(&checks(&stdout, "before "), &checks(&stdout, "after "), &stdout);
        assert!(stdout.contains("needs map=true socket_write=true"), "{case}: {stdout}");
    }
}
