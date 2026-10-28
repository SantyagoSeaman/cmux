//! Linux: seccomp-bpf plus landlock (x86-64 and arm64).
//!
//! Chrome's model: a Linux child sets `no_new_privs` and installs its own
//! BPF policy before it reads untrusted input
//! (`sandbox/linux/seccomp-bpf/sandbox_bpf.cc`). Like Chrome's baseline
//! policy (`sandbox/linux/seccomp-bpf-helpers/baseline_policy.cc`) the
//! filter first kills a call from another architecture (and an x32 call on
//! x86-64), answers `clone3` with `ENOSYS` so libc falls back to `clone`,
//! whose flags it can read, and lets signals reach only the worker itself.
//! It denies with `EPERM`:
//!
//! - opening a file by path (`open`, `openat`, `openat2`, `creat`,
//!   `open_by_handle_at`): the worker only uses descriptors it was given;
//! - sockets (`socket`, `socketpair`): it only speaks on the one it holds;
//! - running programs and making processes (`execve`, `execveat`, `fork`,
//!   `vfork`, a `clone` without `CLONE_THREAD`);
//! - reaching other processes (`ptrace`, `process_vm_readv`/`writev`,
//!   `pidfd_open`, `pidfd_send_signal`);
//! - kernel surfaces a parser never needs: io_uring (its operations would
//!   open files past the filter), `bpf`, `perf_event_open`, `userfaultfd`,
//!   the key rings, namespaces and `mount`.
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
    ];
    pub const CLONE: u32 = 56;
    pub const CLONE3: u32 = 435;
    /// Calls whose first argument is the target process: allowed only for
    /// the worker's own pid (`kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`,
    /// `rt_tgsigqueueinfo`).
    pub const SIGNALS: &[u32] = &[62, 200, 234, 129, 297];
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
        198, // socket
        199, // socketpair
        221, // execve
        281, // execveat
        117, // ptrace
        270, // process_vm_readv
        271, // process_vm_writev
        434, // pidfd_open
        424, // pidfd_send_signal
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
    ];
    pub const CLONE: u32 = 220;
    pub const CLONE3: u32 = 435;
    /// `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`, `rt_tgsigqueueinfo`.
    pub const SIGNALS: &[u32] = &[129, 130, 131, 138, 240];
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
const ARG0_LOW: u32 = 16;
const ARG0_HIGH: u32 = 20;

const RET_ALLOW: u32 = 0x7fff_0000; // SECCOMP_RET_ALLOW
const RET_KILL_PROCESS: u32 = 0x8000_0000; // SECCOMP_RET_KILL_PROCESS
const RET_ERRNO: u32 = 0x0005_0000; // SECCOMP_RET_ERRNO
const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;
const CLONE_THREAD: u32 = 0x0001_0000;

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
    install_filter(&filter(std::process::id()))
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
pub fn filter(pid: u32) -> Vec<Insn> {
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
        stmt(LD_W_ABS, ARG0_LOW),
        jump(JSET_K, CLONE_THREAD, 0, 1),
        stmt(RET_K, RET_ALLOW),
        errno_ret(libc::EPERM),
    ]);
    // Signals: to this process only (the whole first argument equals `pid`).
    for &nr in arch::SIGNALS {
        program.extend([
            jump(JEQ_K, nr, 0, 6),
            stmt(LD_W_ABS, ARG0_HIGH),
            jump(JEQ_K, 0, 0, 3),
            stmt(LD_W_ABS, ARG0_LOW),
            jump(JEQ_K, pid, 0, 1),
            stmt(RET_K, RET_ALLOW),
            errno_ret(libc::EPERM),
        ]);
    }
    program.push(stmt(RET_K, RET_ALLOW));
    program
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
        let word = |offset: u32| match offset {
            NR => nr,
            ARCH => audit_arch,
            ARG0_LOW => arg0 as u32,
            ARG0_HIGH => (arg0 >> 32) as u32,
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
        let program = filter(PID);
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
        let program = filter(PID);
        for nr in
            [libc::SYS_read, libc::SYS_write, libc::SYS_mmap, libc::SYS_munmap, libc::SYS_ppoll]
        {
            let nr = u32::try_from(nr).expect("syscall number");
            assert_eq!(run(&program, nr, arch::AUDIT_ARCH, 0), RET_ALLOW, "syscall {nr}");
        }
    }

    #[test]
    fn signals_reach_only_the_worker_itself() {
        let program = filter(PID);
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
    fn another_architecture_is_killed() {
        let program = filter(PID);
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
        ];
        #[cfg(target_arch = "aarch64")]
        let named = [
            libc::SYS_openat,
            libc::SYS_openat2,
            libc::SYS_open_by_handle_at,
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_pidfd_open,
            libc::SYS_pidfd_send_signal,
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
