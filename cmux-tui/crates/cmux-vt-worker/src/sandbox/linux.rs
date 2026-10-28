//! Linux: seccomp-bpf plus landlock (x86-64 and arm64).
//!
//! Chrome's model: a Linux child sets `no_new_privs` and installs its own
//! BPF policy before it reads untrusted input
//! (`sandbox/linux/seccomp-bpf/sandbox_bpf.cc`). Like Chrome's baseline
//! policy (`sandbox/linux/seccomp-bpf-helpers/baseline_policy.cc`) the
//! filter first kills a call from another architecture (and an x32 call on
//! x86-64), answers `clone3` with `ENOSYS` so libc falls back to `clone`,
//! whose flags it can read, and lets signals reach only the worker itself.
//! `prlimit64`, the pid-taking `sched_*` calls and `get`/`setpriority`
//! reach only the caller or the worker's pid (Chrome's `RestrictPrlimit`,
//! `RestrictSchedTarget`, `RestrictGetSetpriority`). It denies with `EPERM`:
//!
//! - opening a file by path (`open`, `openat`, `openat2`, `creat`,
//!   `open_by_handle_at`): the worker only uses descriptors it was given;
//! - changing the file system (Chrome's `IsFileSystem` and
//!   `IsCurrentDirectory` sets): unlink, rename, mkdir, rmdir, link,
//!   symlink, truncate, mknod, chmod, chown, utime, the xattr writes, the
//!   current directory and root, `inotify_add_watch`, `name_to_handle_at`,
//!   and the descriptor forms `fchmod`, `fchown`, `fsetxattr`,
//!   `fremovexattr`. Seccomp alone carries these where landlock is off, and
//!   landlock never covers chmod, chown, utime or xattrs. `ftruncate` stays
//!   for the worker's own segments; path `stat` and `readlink` stay (they
//!   change nothing; Rust's `fstat` goes through `statx`/`newfstatat`);
//! - sockets (`socket`, `socketpair`): it only speaks on the one it holds;
//! - running programs and making processes (`execve`, `execveat`, `fork`,
//!   `vfork`, a `clone` without `CLONE_THREAD`);
//! - reaching other processes (`ptrace`, `process_vm_readv`/`writev`,
//!   `pidfd_open`, `pidfd_send_signal`, `kcmp`, `move_pages`,
//!   `migrate_pages`, `ioprio_set`);
//! - kernel surfaces a parser never needs: io_uring (its operations would
//!   open files past the filter), `bpf`, `perf_event_open`, `userfaultfd`,
//!   the key rings, namespaces, `mount`, `umount2` and the new mount API.
//!
//! Landlock (`Documentation/userspace-api/landlock.rst`), where the kernel
//! has it, adds a domain that handles every file-system right of the
//! kernel's ABI with no rule, so no path is reachable at all (create,
//! remove, rename, truncate and exec included), and on newer ABIs also TCP
//! bind and connect, abstract Unix sockets and signals out of the domain.
//!
//! Both bind the calling thread; the seccomp filter is synchronized to every
//! thread of the process (`SECCOMP_FILTER_FLAG_TSYNC`). The worker has one.

use std::io;

use super::{SandboxError, SandboxPolicy};

/// The syscall numbers the filter names, for this architecture
/// (`arch/x86/entry/syscalls/syscall_64.tbl`, `include/uapi/asm-generic/unistd.h`).
#[cfg(target_arch = "x86_64")]
mod arch {
    /// `AUDIT_ARCH_X86_64`: `EM_X86_64` | 64-bit | little-endian.
    pub const AUDIT_ARCH: u32 = 0xC000_003E;
    /// x32 calls carry this bit in their number; the filter kills them.
    pub const X32_SYSCALL_BIT: Option<u32> = Some(0x4000_0000);
    pub const DENIED: &[u32] = &[
        2,   // open
        85,  // creat
        257, // openat
        437, // openat2
        304, // open_by_handle_at
        // Changing the file system by path (Chrome's IsFileSystem), which
        // landlock covers only in part and not at all where it is off.
        87,  // unlink
        263, // unlinkat
        82,  // rename
        264, // renameat
        316, // renameat2
        83,  // mkdir
        258, // mkdirat
        84,  // rmdir
        86,  // link
        265, // linkat
        88,  // symlink
        266, // symlinkat
        76,  // truncate
        133, // mknod
        259, // mknodat
        90,  // chmod
        268, // fchmodat
        452, // fchmodat2
        92,  // chown
        94,  // lchown
        260, // fchownat
        132, // utime
        235, // utimes
        280, // utimensat
        261, // futimesat
        188, // setxattr
        189, // lsetxattr
        197, // removexattr
        198, // lremovexattr
        303, // name_to_handle_at
        254, // inotify_add_watch
        // The same changes through a descriptor (the copy's in-use lock is
        // one); `ftruncate` stays for the worker's own segments.
        91,  // fchmod
        93,  // fchown
        190, // fsetxattr
        199, // fremovexattr
        // The current directory and the root (Chrome's IsCurrentDirectory).
        80,  // chdir
        81,  // fchdir
        161, // chroot
        41,  // socket
        53,  // socketpair
        59,  // execve
        322, // execveat
        57,  // fork
        58,  // vfork
        101, // ptrace
        310, // process_vm_readv
        311, // process_vm_writev
        434, // pidfd_open
        424, // pidfd_send_signal
        312, // kcmp
        279, // move_pages
        256, // migrate_pages
        251, // ioprio_set
        425, // io_uring_setup
        426, // io_uring_enter
        427, // io_uring_register
        321, // bpf
        298, // perf_event_open
        323, // userfaultfd
        248, // add_key
        249, // request_key
        250, // keyctl
        272, // unshare
        308, // setns
        165, // mount
        166, // umount2
        428, // open_tree
        429, // move_mount
        430, // fsopen
        431, // fsconfig
        432, // fsmount
        433, // fspick
        442, // mount_setattr
    ];
    pub const CLONE: u32 = 56;
    pub const CLONE3: u32 = 435;
    /// Calls whose first argument is the target process: allowed only for
    /// the worker's own pid (`kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`,
    /// `rt_tgsigqueueinfo`).
    pub const SIGNALS: &[u32] = &[62, 200, 234, 129, 297];
    /// Calls whose first argument is the target process, 0 meaning the
    /// caller: `prlimit64` and the `sched_*` calls that take a pid
    /// (Chrome's `RestrictPrlimit`, `RestrictSchedTarget`).
    pub const SELF_OR_CALLER: &[u32] = &[302, 142, 144, 203, 314, 143, 145, 204, 315, 148];
    /// `getpriority`, `setpriority`: `PRIO_PROCESS` and the caller only
    /// (Chrome's `RestrictGetSetpriority`).
    pub const PRIORITY: &[u32] = &[140, 141];
    pub const SECCOMP: libc::c_long = 317;
}

#[cfg(target_arch = "aarch64")]
mod arch {
    /// `AUDIT_ARCH_AARCH64`: `EM_AARCH64` | 64-bit | little-endian.
    pub const AUDIT_ARCH: u32 = 0xC000_00B7;
    pub const X32_SYSCALL_BIT: Option<u32> = None;
    pub const DENIED: &[u32] = &[
        56,  // openat
        437, // openat2
        265, // open_by_handle_at
        35,  // unlinkat
        38,  // renameat
        276, // renameat2
        34,  // mkdirat
        37,  // linkat
        36,  // symlinkat
        45,  // truncate
        33,  // mknodat
        53,  // fchmodat
        452, // fchmodat2
        54,  // fchownat
        88,  // utimensat
        5,   // setxattr
        6,   // lsetxattr
        14,  // removexattr
        15,  // lremovexattr
        264, // name_to_handle_at
        27,  // inotify_add_watch
        52,  // fchmod
        55,  // fchown
        7,   // fsetxattr
        16,  // fremovexattr
        49,  // chdir
        50,  // fchdir
        51,  // chroot
        198, // socket
        199, // socketpair
        221, // execve
        281, // execveat
        117, // ptrace
        270, // process_vm_readv
        271, // process_vm_writev
        434, // pidfd_open
        424, // pidfd_send_signal
        272, // kcmp
        239, // move_pages
        238, // migrate_pages
        30,  // ioprio_set
        425, // io_uring_setup
        426, // io_uring_enter
        427, // io_uring_register
        280, // bpf
        241, // perf_event_open
        282, // userfaultfd
        217, // add_key
        218, // request_key
        219, // keyctl
        97,  // unshare
        268, // setns
        40,  // mount
        39,  // umount2
        428, // open_tree
        429, // move_mount
        430, // fsopen
        431, // fsconfig
        432, // fsmount
        433, // fspick
        442, // mount_setattr
    ];
    pub const CLONE: u32 = 220;
    pub const CLONE3: u32 = 435;
    /// `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`.
    pub const SIGNALS: &[u32] = &[129, 130, 131, 138, 240];
    /// `prlimit64` and the `sched_*` calls that take a pid.
    pub const SELF_OR_CALLER: &[u32] = &[261, 118, 119, 122, 274, 121, 120, 123, 275, 127];
    /// `getpriority`, `setpriority`.
    pub const PRIORITY: &[u32] = &[141, 140];
    pub const SECCOMP: libc::c_long = 277;
}

/// The landlock calls have the same numbers on both architectures.
const LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;

// Classic BPF opcodes (linux/filter.h, linux/bpf_common.h).
const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
const RET_K: u16 = 0x06; // BPF_RET | BPF_K

// `struct seccomp_data` offsets (linux/seccomp.h): nr, arch, ip, args[6];
// both architectures are little-endian, so an argument's low word comes first.
const NR: u32 = 0;
const ARCH: u32 = 4;
const ARGS: u32 = 16;

const RET_ALLOW: u32 = 0x7fff_0000; // SECCOMP_RET_ALLOW
const RET_KILL_PROCESS: u32 = 0x8000_0000; // SECCOMP_RET_KILL_PROCESS
const RET_ERRNO: u32 = 0x0005_0000; // SECCOMP_RET_ERRNO
const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;
const CLONE_THREAD: u32 = 0x0001_0000;
const PRIO_PROCESS: u32 = 0;

/// The daemon's choice: seccomp always, landlock when this kernel has it.
/// `Unsupported` when the kernel has no seccomp filter mode.
pub(super) fn policy() -> Result<SandboxPolicy, SandboxError> {
    if !seccomp_available() {
        return Err(SandboxError::Unsupported);
    }
    Ok(SandboxPolicy::Seccomp { landlock: landlock_abi() >= 1 })
}

/// The worker's side: `no_new_privs`, the landlock domain when asked, then
/// the seccomp filter.
pub(super) fn apply(landlock: bool) -> Result<(), SandboxError> {
    // SAFETY: prctl with integer arguments only.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(SandboxError::NoNewPrivs(errno()));
    }
    if landlock {
        enter_landlock_domain()?;
    }
    install_filter(&filter(std::process::id())?)
}

/// The landlock ABI version of the running kernel; 0 when it has none or
/// landlock is off (not in the LSM list, or a container's filter refuses
/// the call).
pub fn landlock_abi() -> u32 {
    // SAFETY: the version query takes no attribute (null, size 0) and
    // makes no descriptor.
    let version = unsafe {
        libc::syscall(
            LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    u32::try_from(version).unwrap_or(0)
}

/// Whether the kernel has the seccomp filter mode (`PR_GET_SECCOMP`
/// answers anything but `EINVAL`).
fn seccomp_available() -> bool {
    // SAFETY: prctl with integer arguments only.
    unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) >= 0 }
}

/// `struct landlock_ruleset_attr` (linux/landlock.h); the kernel reads as
/// many fields as the size it is given says.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RulesetAttr {
    pub handled_access_fs: u64,
    pub handled_access_net: u64,
    pub scoped: u64,
}

/// The ruleset that handles every right landlock ABI `abi` knows, and the
/// attribute size that ABI reads (a kernel refuses unknown bits).
pub fn ruleset_for(abi: u32) -> (RulesetAttr, usize) {
    // ABI 1: execute through make_sym (bits 0-12); 2: refer; 3: truncate;
    // 5: ioctl_dev.
    let mut fs = (1u64 << 13) - 1;
    for (since, bit) in [(2, 13), (3, 14), (5, 15)] {
        if abi >= since {
            fs |= 1 << bit;
        }
    }
    // ABI 4: bind_tcp | connect_tcp; ABI 6: abstract Unix sockets | signals.
    let net = if abi >= 4 { 0b11 } else { 0 };
    let scoped = if abi >= 6 { 0b11 } else { 0 };
    let size = match abi {
        0..=3 => 8,
        4 | 5 => 16,
        _ => 24,
    };
    (RulesetAttr { handled_access_fs: fs, handled_access_net: net, scoped }, size)
}

fn enter_landlock_domain() -> Result<(), SandboxError> {
    let abi = landlock_abi();
    if abi == 0 {
        return Err(SandboxError::Landlock(libc::EOPNOTSUPP));
    }
    let (attr, size) = ruleset_for(abi);
    // SAFETY: `attr` is a live ruleset attribute at least `size` bytes long.
    let ruleset =
        unsafe { libc::syscall(LANDLOCK_CREATE_RULESET, &attr as *const RulesetAttr, size, 0u32) };
    let Ok(fd) = libc::c_int::try_from(ruleset) else {
        return Err(SandboxError::Landlock(errno()));
    };
    if fd < 0 {
        return Err(SandboxError::Landlock(errno()));
    }
    // SAFETY: restricts this thread to the ruleset just made; no pointers.
    let restricted = unsafe { libc::syscall(LANDLOCK_RESTRICT_SELF, fd, 0u32) };
    let error = errno();
    // SAFETY: closes the ruleset descriptor made above; the domain stays.
    unsafe { libc::close(fd) };
    if restricted != 0 {
        return Err(SandboxError::Landlock(error));
    }
    Ok(())
}

/// One BPF instruction (`struct sock_filter`).
pub type Insn = libc::sock_filter;

fn stmt(code: u16, k: u32) -> Insn {
    Insn { code, jt: 0, jf: 0, k }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
    Insn { code, jt, jf, k }
}

fn errno_ret(errno: libc::c_int) -> Insn {
    // `errno` is a small positive constant; the action keeps its low 16 bits.
    stmt(RET_K, RET_ERRNO | (errno as u32 & 0xffff))
}

/// The worker's filter for process `pid` (see the module doc). Each check
/// ends in its own return, so every jump is short and forward.
pub fn filter(pid: u32) -> Result<Vec<Insn>, SandboxError> {
    let mut program = vec![
        stmt(LD_W_ABS, ARCH),
        jump(JEQ_K, arch::AUDIT_ARCH, 1, 0),
        stmt(RET_K, RET_KILL_PROCESS),
        stmt(LD_W_ABS, NR),
    ];
    if let Some(bit) = arch::X32_SYSCALL_BIT {
        program.extend([jump(JSET_K, bit, 0, 1), stmt(RET_K, RET_KILL_PROCESS)]);
    }
    for &nr in arch::DENIED {
        program.extend([jump(JEQ_K, nr, 0, 1), errno_ret(libc::EPERM)]);
    }
    program.extend([jump(JEQ_K, arch::CLONE3, 0, 1), errno_ret(libc::ENOSYS)]);
    // clone: a new thread only.
    program.extend([
        jump(JEQ_K, arch::CLONE, 0, 4),
        stmt(LD_W_ABS, ARGS),
        jump(JSET_K, CLONE_THREAD, 0, 1),
        stmt(RET_K, RET_ALLOW),
        errno_ret(libc::EPERM),
    ]);
    // Signals: to this process only (the whole first argument equals `pid`).
    for &nr in arch::SIGNALS {
        allow_only_if(&mut program, nr, &[(0, &[pid])])?;
    }
    // Limits and scheduling: the caller (0) or this process.
    for &nr in arch::SELF_OR_CALLER {
        allow_only_if(&mut program, nr, &[(0, &[0, pid])])?;
    }
    for &nr in arch::PRIORITY {
        allow_only_if(&mut program, nr, &[(0, &[PRIO_PROCESS]), (1, &[0, pid])])?;
    }
    program.push(stmt(RET_K, RET_ALLOW));
    Ok(program)
}

/// Allow call `nr` only when each `(argument, values)` holds, the whole
/// 64-bit argument being one of `values`; `EPERM` otherwise. Every jump is
/// forward and inside the block.
fn allow_only_if(
    program: &mut Vec<Insn>,
    nr: u32,
    conditions: &[(u32, &[u32])],
) -> Result<(), SandboxError> {
    let mut block = Vec::new();
    let mut to_deny = Vec::new();
    for &(argument, values) in conditions {
        let low = ARGS + 8 * argument;
        block.push(stmt(LD_W_ABS, low + 4));
        to_deny.push(block.len());
        block.push(jump(JEQ_K, 0, 0, 0));
        block.push(stmt(LD_W_ABS, low));
        for (index, &value) in values.iter().enumerate() {
            let later = values.len() - 1 - index;
            if later == 0 {
                to_deny.push(block.len());
                block.push(jump(JEQ_K, value, 0, 0));
            } else {
                block.push(jump(JEQ_K, value, short(later)?, 0));
            }
        }
    }
    block.push(stmt(RET_K, RET_ALLOW));
    let deny = block.len();
    block.push(errno_ret(libc::EPERM));
    for at in to_deny {
        block[at].jf = short(deny - at - 1)?;
    }
    program.push(jump(JEQ_K, nr, 0, short(block.len())?));
    program.extend(block);
    Ok(())
}

/// A forward jump offset; classic BPF has 8 bits for one.
fn short(offset: usize) -> Result<u8, SandboxError> {
    u8::try_from(offset).map_err(|_| SandboxError::Seccomp(libc::E2BIG))
}

fn install_filter(program: &[Insn]) -> Result<(), SandboxError> {
    let Ok(len) = u16::try_from(program.len()) else {
        return Err(SandboxError::Seccomp(libc::E2BIG));
    };
    let prog = libc::sock_fprog { len, filter: program.as_ptr().cast_mut() };
    // SAFETY: `prog` points at `len` live instructions for the call; the
    // kernel copies the program and keeps no pointer to it.
    let installed = unsafe {
        libc::syscall(
            arch::SECCOMP,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &prog as *const libc::sock_fprog,
        )
    };
    // TSYNC answers a positive thread id when another thread blocks it.
    if installed != 0 {
        return Err(SandboxError::Seccomp(if installed < 0 { errno() } else { libc::ESRCH }));
    }
    Ok(())
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `program` on one `seccomp_data` the way the kernel does (only
    /// the opcodes the filter uses).
    fn run(program: &[Insn], nr: u32, audit_arch: u32, arg0: u64) -> u32 {
        run_args(program, nr, audit_arch, [arg0, 0])
    }

    fn run_args(program: &[Insn], nr: u32, audit_arch: u32, args: [u64; 2]) -> u32 {
        let word = |offset: u32| match offset {
            NR => nr,
            ARCH => audit_arch,
            ARGS..=31 => {
                let arg = args[((offset - ARGS) / 8) as usize];
                if (offset - ARGS).is_multiple_of(8) { arg as u32 } else { (arg >> 32) as u32 }
            }
            other => panic!("the filter loads offset {other}"),
        };
        let (mut pc, mut acc) = (0usize, 0u32);
        loop {
            let insn = program[pc];
            pc += 1;
            match insn.code {
                LD_W_ABS => acc = word(insn.k),
                JEQ_K => pc += usize::from(if acc == insn.k { insn.jt } else { insn.jf }),
                JSET_K => pc += usize::from(if acc & insn.k != 0 { insn.jt } else { insn.jf }),
                RET_K => return insn.k,
                other => panic!("unexpected opcode {other:#x}"),
            }
        }
    }

    const PID: u32 = 4242;
    const EPERM: u32 = RET_ERRNO | libc::EPERM as u32;

    #[test]
    fn the_filter_denies_opens_sockets_exec_and_new_processes() {
        let program = filter(PID).expect("filter");
        for &nr in arch::DENIED {
            assert_eq!(run(&program, nr, arch::AUDIT_ARCH, 0), EPERM, "syscall {nr}");
        }
        assert_eq!(
            run(&program, arch::CLONE3, arch::AUDIT_ARCH, 0),
            RET_ERRNO | libc::ENOSYS as u32
        );
        let fork_flags = u64::from(libc::SIGCHLD as u32);
        assert_eq!(run(&program, arch::CLONE, arch::AUDIT_ARCH, fork_flags), EPERM);
        let thread_flags = u64::from(CLONE_THREAD) | 0x0000_0100 | 0x0000_0800;
        assert_eq!(run(&program, arch::CLONE, arch::AUDIT_ARCH, thread_flags), RET_ALLOW);
    }

    #[test]
    fn the_filter_allows_reads_writes_maps_and_polls() {
        let program = filter(PID).expect("filter");
        for nr in
            [libc::SYS_read, libc::SYS_write, libc::SYS_mmap, libc::SYS_munmap, libc::SYS_ppoll]
        {
            let nr = u32::try_from(nr).expect("syscall number");
            assert_eq!(run(&program, nr, arch::AUDIT_ARCH, 0), RET_ALLOW, "syscall {nr}");
        }
    }

    #[test]
    fn signals_reach_only_the_worker_itself() {
        let program = filter(PID).expect("filter");
        for &nr in arch::SIGNALS {
            assert_eq!(run(&program, nr, arch::AUDIT_ARCH, u64::from(PID)), RET_ALLOW);
            assert_eq!(run(&program, nr, arch::AUDIT_ARCH, 1), EPERM, "init, syscall {nr}");
            assert_eq!(run(&program, nr, arch::AUDIT_ARCH, 0), EPERM, "process group");
            assert_eq!(run(&program, nr, arch::AUDIT_ARCH, u64::MAX), EPERM, "every process");
            let high = u64::from(PID) | (1 << 32);
            assert_eq!(
                run(&program, nr, arch::AUDIT_ARCH, high),
                EPERM,
                "pid in the low word only"
            );
        }
    }

    #[test]
    fn limits_scheduling_and_priority_reach_only_the_caller_or_the_worker() {
        let program = filter(PID).expect("filter");
        for &nr in arch::SELF_OR_CALLER {
            for target in [0, u64::from(PID)] {
                assert_eq!(run(&program, nr, arch::AUDIT_ARCH, target), RET_ALLOW, "{nr}");
            }
            for target in [1, u64::from(PID) + 1, u64::from(PID) | (1 << 32)] {
                assert_eq!(run(&program, nr, arch::AUDIT_ARCH, target), EPERM, "{nr} {target}");
            }
        }
        for &nr in arch::PRIORITY {
            for who in [0, u64::from(PID)] {
                assert_eq!(run_args(&program, nr, arch::AUDIT_ARCH, [0, who]), RET_ALLOW);
            }
            // Another process, or a process group or user (PRIO_PGRP, PRIO_USER).
            assert_eq!(run_args(&program, nr, arch::AUDIT_ARCH, [0, 1]), EPERM, "{nr}");
            assert_eq!(run_args(&program, nr, arch::AUDIT_ARCH, [1, 0]), EPERM, "{nr}");
            assert_eq!(run_args(&program, nr, arch::AUDIT_ARCH, [2, 0]), EPERM, "{nr}");
        }
    }

    #[test]
    fn another_architecture_is_killed() {
        let program = filter(PID).expect("filter");
        // AUDIT_ARCH_I386: the 32-bit table numbers calls differently.
        assert_eq!(run(&program, 5, 0x4000_0003, 0), RET_KILL_PROCESS);
        if let Some(bit) = arch::X32_SYSCALL_BIT {
            assert_eq!(run(&program, bit | 2, arch::AUDIT_ARCH, 0), RET_KILL_PROCESS);
        }
    }

    #[cfg(target_env = "gnu")]
    #[test]
    fn the_numbers_match_libc() {
        #[cfg(target_arch = "x86_64")]
        let named = [
            libc::SYS_open,
            libc::SYS_creat,
            libc::SYS_openat,
            libc::SYS_openat2,
            libc::SYS_open_by_handle_at,
            libc::SYS_unlink,
            libc::SYS_unlinkat,
            libc::SYS_rename,
            libc::SYS_renameat,
            libc::SYS_renameat2,
            libc::SYS_mkdir,
            libc::SYS_mkdirat,
            libc::SYS_rmdir,
            libc::SYS_link,
            libc::SYS_linkat,
            libc::SYS_symlink,
            libc::SYS_symlinkat,
            libc::SYS_truncate,
            libc::SYS_mknod,
            libc::SYS_mknodat,
            libc::SYS_chmod,
            libc::SYS_fchmodat,
            libc::SYS_fchmodat2,
            libc::SYS_chown,
            libc::SYS_lchown,
            libc::SYS_fchownat,
            libc::SYS_utime,
            libc::SYS_utimes,
            libc::SYS_utimensat,
            libc::SYS_futimesat,
            libc::SYS_setxattr,
            libc::SYS_lsetxattr,
            libc::SYS_removexattr,
            libc::SYS_lremovexattr,
            libc::SYS_name_to_handle_at,
            libc::SYS_inotify_add_watch,
            libc::SYS_fchmod,
            libc::SYS_fchown,
            libc::SYS_fsetxattr,
            libc::SYS_fremovexattr,
            libc::SYS_chdir,
            libc::SYS_fchdir,
            libc::SYS_chroot,
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_pidfd_open,
            libc::SYS_pidfd_send_signal,
            libc::SYS_kcmp,
            libc::SYS_move_pages,
            libc::SYS_migrate_pages,
            libc::SYS_ioprio_set,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_keyctl,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_open_tree,
            libc::SYS_move_mount,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_fspick,
            libc::SYS_mount_setattr,
        ];
        #[cfg(target_arch = "aarch64")]
        let named = [
            libc::SYS_openat,
            libc::SYS_openat2,
            libc::SYS_open_by_handle_at,
            libc::SYS_unlinkat,
            38, // renameat: not in libc for this target
            libc::SYS_renameat2,
            libc::SYS_mkdirat,
            libc::SYS_linkat,
            libc::SYS_symlinkat,
            libc::SYS_truncate,
            libc::SYS_mknodat,
            libc::SYS_fchmodat,
            452, // fchmodat2: not in libc for this target
            libc::SYS_fchownat,
            libc::SYS_utimensat,
            libc::SYS_setxattr,
            libc::SYS_lsetxattr,
            libc::SYS_removexattr,
            libc::SYS_lremovexattr,
            libc::SYS_name_to_handle_at,
            libc::SYS_inotify_add_watch,
            libc::SYS_fchmod,
            libc::SYS_fchown,
            libc::SYS_fsetxattr,
            libc::SYS_fremovexattr,
            libc::SYS_chdir,
            libc::SYS_fchdir,
            libc::SYS_chroot,
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_pidfd_open,
            libc::SYS_pidfd_send_signal,
            libc::SYS_kcmp,
            libc::SYS_move_pages,
            libc::SYS_migrate_pages,
            libc::SYS_ioprio_set,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_keyctl,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_open_tree,
            libc::SYS_move_mount,
            libc::SYS_fsopen,
            libc::SYS_fsconfig,
            libc::SYS_fsmount,
            libc::SYS_fspick,
            libc::SYS_mount_setattr,
        ];
        let named: Vec<u32> = named.iter().map(|&nr| u32::try_from(nr).expect("nr")).collect();
        assert_eq!(named, arch::DENIED);
        let signals = [
            libc::SYS_kill,
            libc::SYS_tkill,
            libc::SYS_tgkill,
            libc::SYS_rt_sigqueueinfo,
            libc::SYS_rt_tgsigqueueinfo,
        ];
        let signals: Vec<u32> = signals.iter().map(|&nr| u32::try_from(nr).expect("nr")).collect();
        assert_eq!(signals, arch::SIGNALS);
        let self_or_caller = [
            libc::SYS_prlimit64,
            libc::SYS_sched_setparam,
            libc::SYS_sched_setscheduler,
            libc::SYS_sched_setaffinity,
            libc::SYS_sched_setattr,
            libc::SYS_sched_getparam,
            libc::SYS_sched_getscheduler,
            libc::SYS_sched_getaffinity,
            libc::SYS_sched_getattr,
            libc::SYS_sched_rr_get_interval,
        ];
        let self_or_caller: Vec<u32> =
            self_or_caller.iter().map(|&nr| u32::try_from(nr).expect("nr")).collect();
        assert_eq!(self_or_caller, arch::SELF_OR_CALLER);
        let priority = [libc::SYS_getpriority, libc::SYS_setpriority];
        let priority: Vec<u32> =
            priority.iter().map(|&nr| u32::try_from(nr).expect("nr")).collect();
        assert_eq!(priority, arch::PRIORITY);
        assert_eq!(i64::from(arch::CLONE), libc::SYS_clone);
        assert_eq!(i64::from(arch::CLONE3), libc::SYS_clone3);
        assert_eq!(arch::SECCOMP, libc::SYS_seccomp);
        assert_eq!(LANDLOCK_CREATE_RULESET, libc::SYS_landlock_create_ruleset);
        assert_eq!(LANDLOCK_RESTRICT_SELF, libc::SYS_landlock_restrict_self);
    }

    #[test]
    fn each_landlock_abi_handles_every_right_it_knows() {
        assert_eq!(
            ruleset_for(1),
            (RulesetAttr { handled_access_fs: 0x1fff, ..Default::default() }, 8)
        );
        assert_eq!(ruleset_for(3).0.handled_access_fs, 0x7fff);
        assert_eq!(
            ruleset_for(4),
            (RulesetAttr { handled_access_fs: 0x7fff, handled_access_net: 3, scoped: 0 }, 16)
        );
        assert_eq!(
            ruleset_for(6),
            (RulesetAttr { handled_access_fs: 0xffff, handled_access_net: 3, scoped: 3 }, 24)
        );
        assert_eq!(ruleset_for(9).1, 24);
    }
}
