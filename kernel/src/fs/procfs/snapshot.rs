use alloc::{sync::Arc, vec::Vec};

#[derive(Clone)]
pub(crate) struct ProcThreadSnapshot {
    pub(crate) tid: usize,
    pub(crate) state: u8,
    pub(crate) nice: i32,
    pub(crate) priority: i32,
    pub(crate) runtime_us: u64,
    pub(crate) start_time_us: u64,
    pub(crate) last_cpu: usize,
    pub(crate) io: ProcIoSnapshot,
}

/// @description procfs adapter 使用的 Linux task I/O counters。
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
    pub(crate) pid: usize,
    pub(crate) ppid: usize,
    pub(crate) process_group: usize,
    pub(crate) session: usize,
    pub(crate) tty_number: u32,
    pub(crate) terminal_process_group: isize,
    pub(crate) comm: Vec<u8>,
    pub(crate) uids: [u32; 3],
    pub(crate) gids: [u32; 3],
    pub(crate) groups: Vec<u32>,
    pub(crate) state: u8,
    pub(crate) nice: i32,
    pub(crate) priority: i32,
    pub(crate) threads: Vec<ProcThreadSnapshot>,
    pub(crate) runtime_us: u64,
    pub(crate) start_time_us: u64,
    pub(crate) virtual_pages: usize,
    pub(crate) resident_pages: usize,
    pub(crate) shared_pages: usize,
    pub(crate) text_pages: usize,
    pub(crate) data_pages: usize,
    pub(crate) fd_size: usize,
    pub(crate) last_cpu: usize,
    pub(crate) io: ProcIoSnapshot,
}

/// @description 一个 live descriptor number 与其 Linux procfs symlink target 快照。
pub(crate) struct ProcFileDescriptorSnapshot {
    pub(crate) fd: usize,
    pub(crate) target: Vec<u8>,
    pub(crate) opened: Option<Arc<super::super::OpenedFile>>,
}

#[derive(Clone, Copy)]
pub(crate) struct ProcCpuSnapshot {
    pub(crate) cpu: usize,
    pub(crate) busy_us: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct ProcNetworkSnapshot {
    pub(crate) address: Option<[u8; 4]>,
    pub(crate) prefix_length: u8,
    pub(crate) gateway: Option<[u8; 4]>,
    pub(crate) up: bool,
    pub(crate) received_bytes: u64,
    pub(crate) received_packets: u64,
    pub(crate) transmitted_bytes: u64,
    pub(crate) transmitted_packets: u64,
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
    pub(crate) cached_pages: usize,
    pub(crate) dirty_pages: usize,
    pub(crate) reclaimable_cached_pages: usize,
    pub(crate) heap_pages: usize,
    pub(crate) runnable_tasks: usize,
    pub(crate) total_tasks: usize,
    pub(crate) processes_created: u64,
    pub(crate) last_pid: usize,
    pub(crate) load_milli: [u64; 3],
    pub(crate) cpus: Vec<ProcCpuSnapshot>,
    pub(crate) processes: Vec<ProcProcessSnapshot>,
    pub(crate) network: Option<ProcNetworkSnapshot>,
}
