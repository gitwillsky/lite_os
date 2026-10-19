use alloc::{sync::Arc, vec::Vec};

/// 单一 Thread credential lock 下取得的不可变快照；group Arc 不复制内容、不分配。
#[derive(Clone)]
pub(crate) struct ProcCredentialSnapshot {
    pub(crate) uids: [u32; 3],
    pub(crate) gids: [u32; 3],
    /// 同一 credential snapshot 的 filesystem UID/GID，供 proc 第四列与FSCREDS。
    pub(crate) fsids: [u32; 2],
    pub(crate) groups: Option<Arc<Vec<u32>>>,
    pub(crate) capabilities: syscall_abi::capability::CapabilitySnapshot,
    /// 权限使用目标实际 user namespace；显示由 reader 的 namespace 决定。
    pub(crate) user_namespace: Arc<crate::namespace::UserNamespace>,
}

#[derive(Clone)]
pub(crate) struct ProcThreadSnapshot {
    pub(crate) tid: Arc<crate::namespace::PidIdentity>,
    pub(crate) has_mm: bool,
    pub(crate) fd_size: usize,
    pub(crate) credentials: ProcCredentialSnapshot,
    pub(crate) state: u8,
    pub(crate) nice: i32,
    pub(crate) priority: i32,
    pub(crate) runtime_us: u64,
    pub(crate) start_time_us: u64,
    pub(crate) last_cpu: usize,
    pub(crate) io: ProcIoSnapshot,
}

/// procfs adapter 使用的 Linux task I/O counters。
#[derive(Clone, Copy, Default)]
pub(crate) struct ProcIoSnapshot {
    pub(crate) read_characters: u64,
    pub(crate) written_characters: u64,
    pub(crate) read_syscalls: u64,
    pub(crate) write_syscalls: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

#[derive(Clone)]
pub(crate) struct ProcProcessSnapshot {
    /// 来源 execution kind 的只读投影，用于 PF_KTHREAD/Kthread，不能由 mm absence 推断。
    pub(crate) kernel_thread: bool,
    pub(crate) pid: Arc<crate::namespace::PidIdentity>,
    pub(crate) has_mm: bool,
    pub(crate) ppid: Option<Arc<crate::namespace::PidIdentity>>,
    pub(crate) process_group: Arc<crate::namespace::PidIdentity>,
    pub(crate) session: Arc<crate::namespace::PidIdentity>,
    pub(crate) tty_number: u32,
    pub(crate) terminal_process_group: isize,
    pub(crate) comm: Vec<u8>,
    pub(crate) credentials: ProcCredentialSnapshot,
    pub(crate) state: u8,
    pub(crate) nice: i32,
    pub(crate) priority: i32,
    pub(crate) threads: Vec<ProcThreadSnapshot>,
    pub(crate) runtime_us: u64,
    pub(crate) start_time_us: u64,
    pub(crate) virtual_pages: usize,
    pub(crate) resident_pages: usize,
    pub(crate) swapped_pages: usize,
    pub(crate) shared_pages: usize,
    pub(crate) text_pages: usize,
    pub(crate) data_pages: usize,
    pub(crate) fd_size: usize,
    pub(crate) last_cpu: usize,
    pub(crate) io: ProcIoSnapshot,
}

/// descriptor enumeration 不取得 namespace gate；magic link 直接消费 opened identity。
pub(crate) struct ProcFileDescriptorSnapshot {
    pub(crate) fd: usize,
    pub(crate) target: Arc<super::super::OpenedFile>,
}

#[derive(Clone, Copy)]
pub(crate) struct ProcCpuSnapshot {
    pub(crate) cpu: usize,
    pub(crate) busy_us: u64,
}

pub(crate) struct ProcSnapshot {
    pub(crate) uptime_us: u64,
    pub(crate) boot_epoch_seconds: u64,
    pub(crate) total_pages: usize,
    pub(crate) free_pages: usize,
    pub(crate) buddy_free_blocks: [usize; usize::BITS as usize],
    pub(crate) direct_reclaim_attempts: u64,
    pub(crate) direct_reclaim_scanned_pages: u64,
    pub(crate) direct_reclaim_reclaimed_pages: u64,
    pub(crate) background_reclaim_scanned_pages: u64,
    pub(crate) background_reclaim_reclaimed_pages: u64,
    pub(crate) cached_pages: usize,
    pub(crate) dirty_pages: usize,
    pub(crate) reclaimable_cached_pages: usize,
    pub(crate) heap_pages: usize,
    pub(crate) swap_total_pages: usize,
    pub(crate) swap_free_pages: usize,
    pub(crate) swap_cached_pages: usize,
    pub(crate) runnable_tasks: usize,
    pub(crate) total_tasks: usize,
    pub(crate) processes_created: u64,
    pub(crate) last_pid: usize,
    pub(crate) load_milli: [u64; 3],
    pub(crate) cpus: Vec<ProcCpuSnapshot>,
    pub(crate) processes: Vec<ProcProcessSnapshot>,
}
