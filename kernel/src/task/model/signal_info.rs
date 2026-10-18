//! 普通信号首个来源、实时独立载荷与 Linux LP64 siginfo_t 的唯一 codec。

#[derive(Debug, Clone, Copy)]
enum SignalPayload {
    // Linux kernel_siginfo 的 LP64 union 为 32 bytes；显式 user 原生载荷保留 errno/padding，
    // 不把 128-byte userspace expansion 放进每个 pending slot。
    User {
        errno: i32,
        padding: [u8; 4],
        body: [u8; 32],
    },
    Identity {
        pid: i32,
        uid: u32,
        status: i32,
        // 仅 SIGCHLD 使用：child 的 user/system CPU 时间，单位 USER_HZ clock_t；其余来源恒为零。
        utime: u64,
        stime: u64,
    },
    Fault {
        address: u64,
    },
    Timer {
        id: i32,
        overrun: i32,
        value: u64,
    },
    Seccomp {
        value: u16,
        number: i32,
        architecture: u32,
        address: u64,
    },
}

// 单一 consequence：祖先/内核强制只越过 init 的 KILL/STOP 豁免；同步 fault 越过全部默认豁免。
// 顺序用于 coalescing 的单调合并，不能由首个可见 payload 反推后续同步 fault。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SignalForce {
    None,
    Namespace,
    Synchronous,
}

/// 无 allocation 的 native pending bit 只保留不可丢失的内核 force consequence。
/// 所有可见 siginfo 字段统一为 SI_USER 零来源，不保留第二份 ordinary payload。
#[derive(Debug, Clone, Copy)]
pub(super) struct SignalFallback(SignalForce);
impl SignalFallback {
    pub(super) const NONE: Self = Self(SignalForce::None);
    pub(super) fn from_info(info: PendingSignal) -> Self {
        Self(info.force)
    }
    pub(super) fn info(self) -> PendingSignal {
        PendingSignal {
            force: self.0,
            ..PendingSignal::default()
        }
    }
    pub(super) fn merge_force(&mut self, incoming: PendingSignal) {
        self.0 = self.0.max(incoming.force);
    }
}

/// coalesced signal 的首个 siginfo 与不可丢失的 forced consequence。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingSignal {
    code: i32,
    force: SignalForce,
    payload: SignalPayload,
}

// 每个 pending slot 有界；typed union 避免把 fault/child/timer/seccomp 布局字段混用。
const _: () = assert!(core::mem::size_of::<PendingSignal>() == 56);

impl Default for PendingSignal {
    fn default() -> Self {
        Self::identity(0, 0, 0)
    }
}

impl PendingSignal {
    fn identity(code: i32, pid: usize, status: i32) -> Self {
        Self {
            code,
            force: SignalForce::None,
            payload: SignalPayload::Identity {
                pid: pid as i32,
                uid: 0,
                status,
                utime: 0,
                stime: 0,
            },
        }
    }

    /// 解码 LP64 kernel_siginfo 的 48-byte 原生前缀；force 来源不能由用户输入伪造。
    pub(crate) fn from_user_native(bytes: &[u8; 48]) -> Self {
        Self {
            code: i32::from_ne_bytes(bytes[8..12].try_into().expect("si_code width")),
            force: SignalForce::None,
            payload: SignalPayload::User {
                errno: i32::from_ne_bytes(bytes[4..8].try_into().expect("si_errno width")),
                padding: bytes[12..16].try_into().expect("LP64 siginfo alignment"),
                body: bytes[16..48].try_into().expect("LP64 native siginfo union"),
            },
        }
    }
    /// 固定 Linux known_siginfo_layout；未知 layout 才要求扩展区全零。
    pub(crate) fn known_user_layout(self, signal: usize) -> bool {
        if self.code == syscall_abi::signal::SI_KERNEL {
            return true;
        }
        if self.code > 0 {
            return self.code <= Self::specific_code_limit(signal).unwrap_or(6);
        }
        self.code >= -7 || self.code == -60
    }
    fn specific_code_limit(signal: usize) -> Option<i32> {
        match signal {
            4 => Some(11),
            8 => Some(15),
            11 => Some(10),
            7 => Some(5),
            5 | 17 | 29 => Some(6),
            31 => Some(2),
            _ => None,
        }
    }
    fn user_has_identity(self, signal: usize) -> bool {
        if self.code < 0 {
            return !matches!(self.code, -2 | -5);
        }
        if self.code == 0 || self.code >= syscall_abi::signal::SI_KERNEL {
            return true;
        }
        if Self::specific_code_limit(signal).is_some_and(|limit| self.code <= limit) {
            return signal == 17;
        }
        self.code > 6
    }
    /// 只有当前 TID 的显式发送允许 positive code/SI_TKILL；root 也不能冒充其他线程的内核来源。
    pub(crate) fn restricted_user_source(self) -> bool {
        self.code >= 0 || self.code == -6
    }
    pub(crate) fn user_sender_uid(self, signal: usize) -> Option<u32> {
        if !self.user_has_identity(signal) {
            return None;
        }
        let SignalPayload::User { body, .. } = self.payload else {
            panic!("queued identity needs user payload")
        };
        Some(u32::from_ne_bytes(
            body[4..8].try_into().expect("si_uid width"),
        ))
    }
    /// 按 actual recipient UserNS 翻译用户提供的 si_uid；si_pid 只在 ancestor sender 不可见时清零。
    pub(crate) fn project_user_identity(mut self, signal: usize, uid: u32, ancestor: bool) -> Self {
        if self.user_has_identity(signal) {
            let SignalPayload::User { body, .. } = &mut self.payload else {
                panic!("queued identity needs user payload")
            };
            body[4..8].copy_from_slice(&uid.to_ne_bytes());
            if ancestor {
                body[..4].fill(0);
            }
            if ancestor || self.code == syscall_abi::signal::SI_KERNEL {
                self.force = SignalForce::Namespace;
            }
        }
        self
    }

    /// 构造指定 sender TGID 的 SI_TKILL 来源。
    pub(crate) fn thread_directed(pid: usize) -> Self {
        Self::identity(-6, pid, 0)
    }

    /// 构造指定 sender TGID 的 SI_USER 来源。
    pub(crate) fn process_directed(pid: usize) -> Self {
        Self::identity(0, pid, 0)
    }

    /// 构造 CLD_EXITED，status 为 child exit code。
    pub(crate) fn child_exited(pid: usize, status: i32) -> Self {
        Self::identity(1, pid, status)
    }

    /// 记录 SIGCHLD 的 `si_utime/si_stime`（USER_HZ clock_t）。
    pub(crate) fn with_child_cpu_times(mut self, user_ticks: u64, system_ticks: u64) -> Self {
        let SignalPayload::Identity { utime, stime, .. } = &mut self.payload else {
            panic!("child CPU times require identity siginfo");
        };
        *utime = user_ticks;
        *stime = system_ticks;
        self
    }

    /// 构造 CLD_KILLED，status 为终止 child 的 signal。
    pub(crate) fn child_killed(pid: usize, signal: usize) -> Self {
        Self::identity(2, pid, signal as i32)
    }

    /// 构造 CLD_STOPPED，status 为触发 group stop 的 signal。
    pub(crate) fn child_stopped(pid: usize, signal: usize) -> Self {
        Self::identity(5, pid, signal as i32)
    }

    /// 构造 CLD_CONTINUED，status 为 SIGCONT。
    pub(crate) fn child_continued(pid: usize) -> Self {
        Self::identity(6, pid, syscall_abi::signal::SIGCONT as i32)
    }

    /// 构造无 sender identity 的 SI_KERNEL 来源。
    pub(crate) fn kernel() -> Self {
        let mut info = Self::identity(syscall_abi::signal::SI_KERNEL, 0, 0);
        info.force = SignalForce::Namespace;
        info
    }

    /// generation 在 graph 下按准确 recipient PID/user namespace 固定历史身份。
    /// `force` 仅允许祖先 PID namespace 或内核来源；不改变首个 coalesced siginfo。
    pub(crate) fn with_identity(mut self, number: u32, displayed_uid: u32, force: bool) -> Self {
        let SignalPayload::Identity { pid, uid, .. } = &mut self.payload else {
            panic!("identity projection requires identity siginfo");
        };
        *pid = number as i32;
        *uid = displayed_uid;
        if force {
            self.force = self.force.max(SignalForce::Namespace);
        }
        self
    }

    /// 构造 positive code/用户 fault address 的 forced 来源，绕过 PID 1 默认豁免。
    pub(crate) fn synchronous_fault(code: i32, address: usize) -> Self {
        assert!(code > 0, "synchronous fault si_code must be positive");
        Self {
            code,
            force: SignalForce::Synchronous,
            payload: SignalPayload::Fault {
                address: address as u64,
            },
        }
    }

    /// 消费周期 timer 通知时更新最终 SI_TIMER overrun。
    pub(crate) fn set_timer_overrun(&mut self, overrun: i32) {
        if let SignalPayload::Timer {
            overrun: stored, ..
        } = &mut self.payload
        {
            *stored = overrun;
        }
    }
    /// 构造 SI_TIMER，携带 timer ID、最近 overrun 与完整 sigev_value。
    pub(crate) fn timer(id: i32, overrun: i32, value: u64) -> Self {
        Self {
            code: -2,
            force: SignalForce::None,
            payload: SignalPayload::Timer { id, overrun, value },
        }
    }

    /// 构造 SYS_SECCOMP 的 forced SIGSYS 来源。
    ///
    /// `value` 写 si_errno；其余字段为 syscall low i32、AUDIT_ARCH 与 syscall 后 PC。
    pub(crate) fn seccomp(value: u16, number: usize, architecture: u32, address: usize) -> Self {
        Self {
            code: 1,
            force: SignalForce::Synchronous,
            payload: SignalPayload::Seccomp {
                value,
                number: number as i32,
                architecture,
                address: address as u64,
            },
        }
    }

    /// 编码 Linux LP64 的 128-byte siginfo_t；未使用 union 字节全部为零。
    pub(crate) fn encode(self, signal: usize) -> [u8; 128] {
        let mut bytes = [0u8; 128];
        bytes[..4].copy_from_slice(&(signal as i32).to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.code.to_ne_bytes());
        match self.payload {
            SignalPayload::User {
                errno,
                padding,
                body,
            } => {
                bytes[4..8].copy_from_slice(&errno.to_ne_bytes());
                bytes[12..16].copy_from_slice(&padding);
                bytes[16..48].copy_from_slice(&body);
            }
            SignalPayload::Identity {
                pid,
                uid,
                status,
                utime,
                stime,
            } => {
                bytes[16..20].copy_from_slice(&pid.to_ne_bytes());
                bytes[20..24].copy_from_slice(&uid.to_ne_bytes());
                bytes[24..28].copy_from_slice(&status.to_ne_bytes());
                bytes[32..40].copy_from_slice(&utime.to_ne_bytes());
                bytes[40..48].copy_from_slice(&stime.to_ne_bytes());
            }
            SignalPayload::Fault { address } => {
                bytes[16..24].copy_from_slice(&address.to_ne_bytes());
            }
            SignalPayload::Timer { id, overrun, value } => {
                bytes[16..20].copy_from_slice(&id.to_ne_bytes());
                bytes[20..24].copy_from_slice(&overrun.to_ne_bytes());
                bytes[24..32].copy_from_slice(&value.to_ne_bytes());
            }
            SignalPayload::Seccomp {
                value,
                number,
                architecture,
                address,
            } => {
                bytes[4..8].copy_from_slice(&(value as i32).to_ne_bytes());
                bytes[16..24].copy_from_slice(&address.to_ne_bytes());
                bytes[24..28].copy_from_slice(&number.to_ne_bytes());
                bytes[28..32].copy_from_slice(&architecture.to_ne_bytes());
            }
        }
        bytes
    }

    /// 编码固定 Linux `signalfd_siginfo`；只投影当前布局的字段，全部 padding 为零。
    /// riscv64/aarch64 没有 SPARC/Alpha 的 fault trapno 布局。
    pub(crate) fn encode_signalfd(self, signal: usize) -> [u8; 128] {
        let native = self.encode(signal);
        let mut output = [0u8; 128];
        output[..12].copy_from_slice(&native[..12]);
        let copy = |output: &mut [u8; 128], destination: usize, source: usize, size: usize| {
            output[destination..destination + size].copy_from_slice(&native[source..source + size]);
        };
        if self.code == -2 {
            copy(&mut output, 24, 16, 4); // ssi_tid
            copy(&mut output, 32, 20, 4); // ssi_overrun
            copy(&mut output, 44, 24, 4); // ssi_int
            copy(&mut output, 48, 24, 8); // ssi_ptr
        } else if self.code == -5
            || self.code > 0
                && self.code < syscall_abi::signal::SI_KERNEL
                && (signal == 29 && self.code <= 6
                    || !Self::specific_code_limit(signal).is_some_and(|limit| self.code <= limit)
                        && self.code <= 6)
        {
            copy(&mut output, 28, 16, 4); // ssi_band 是 native long 的 low u32
            copy(&mut output, 20, 24, 4); // ssi_fd
        } else if self.code > 0
            && self.code < syscall_abi::signal::SI_KERNEL
            && Self::specific_code_limit(signal).is_some_and(|limit| self.code <= limit)
        {
            match signal {
                17 => {
                    copy(&mut output, 12, 16, 8); // pid/uid
                    copy(&mut output, 40, 24, 4); // status
                    copy(&mut output, 56, 32, 16); // utime/stime
                }
                31 => {
                    copy(&mut output, 88, 16, 8); // call_addr
                    copy(&mut output, 84, 24, 4); // syscall
                    copy(&mut output, 96, 28, 4); // arch
                }
                _ => {
                    copy(&mut output, 72, 16, 8); // fault addr
                    if signal == 7 && matches!(self.code, 4 | 5) {
                        copy(&mut output, 80, 24, 2); // MCEERR addr_lsb
                    }
                }
            }
        } else {
            copy(&mut output, 12, 16, 8); // kill/RT pid/uid
            if self.code < 0 {
                copy(&mut output, 44, 24, 4);
                copy(&mut output, 48, 24, 8);
            }
        }
        output
    }

    /// Linux kill 与 SEND_SIG_PRIV 不能因实时 record OOM 返回 EAGAIN；其他来源必须保留完整记录。
    /// Linux ordinary special/nonnegative source 可越过 soft/ancestor maximum，仍逐条收费。
    pub(super) fn overrides_pending_limit(self, signal: usize) -> bool {
        signal < 32 && self.code >= 0
    }
    pub(super) fn is_timer_notification(self) -> bool {
        self.code == -2
    }
    pub(super) fn allows_queue_fallback(self) -> bool {
        self.code == 0
            || self.code == syscall_abi::signal::SI_KERNEL
                && matches!(self.payload, SignalPayload::Identity { .. })
    }

    pub(super) fn is_forced(self) -> bool {
        self.force != SignalForce::None
    }
    pub(super) fn bypasses_init(self, signal: usize) -> bool {
        self.force == SignalForce::Synchronous
            || self.force == SignalForce::Namespace && matches!(signal, 9 | 19)
    }
    /// 同号后续 source 不改首个 siginfo，只单调增强投递 consequence。
    pub(super) fn merge_force(&mut self, incoming: Self) {
        self.force = self.force.max(incoming.force);
    }
    pub(super) fn is_forced_fault(self) -> bool {
        self.force == SignalForce::Synchronous
            && matches!(
                self.payload,
                SignalPayload::Fault { .. } | SignalPayload::Seccomp { .. }
            )
    }
}
