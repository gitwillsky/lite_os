use alloc::vec::Vec;
use core::fmt::Write;

use super::{FileSystemError, ProcSnapshot, ProcText, proc_text};

const CLOCK_TICKS_PER_SECOND: u64 = 100;

pub(super) fn ticks(microseconds: u64) -> u64 {
    microseconds / (1_000_000 / CLOCK_TICKS_PER_SECOND)
}

pub(super) fn format_cpu_stat(snapshot: &ProcSnapshot) -> Result<Vec<u8>, FileSystemError> {
    let mut output = ProcText::new();
    let total_busy: u64 = snapshot
        .cpus
        .iter()
        .map(|cpu| cpu.busy_us.min(snapshot.uptime_us))
        .sum();
    let total_idle = snapshot
        .uptime_us
        .saturating_mul(snapshot.cpus.len() as u64)
        .saturating_sub(total_busy);
    writeln!(
        output,
        "cpu  {} 0 0 {} 0 0 0 0",
        ticks(total_busy),
        ticks(total_idle)
    )
    .map_err(|_| FileSystemError::OutOfMemory)?;
    for cpu in &snapshot.cpus {
        let busy = cpu.busy_us.min(snapshot.uptime_us);
        writeln!(
            output,
            "cpu{} {} 0 0 {} 0 0 0 0",
            cpu.cpu,
            ticks(busy),
            ticks(snapshot.uptime_us - busy)
        )
        .map_err(|_| FileSystemError::OutOfMemory)?;
    }
    writeln!(output, "btime {}", snapshot.boot_epoch_seconds)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    writeln!(
        output,
        "processes {}\nprocs_running {}\nprocs_blocked 0",
        snapshot.processes_created, snapshot.runnable_tasks
    )
    .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(output.finish())
}

pub(super) fn format_meminfo(snapshot: &ProcSnapshot) -> Result<Vec<u8>, FileSystemError> {
    let available_pages = snapshot
        .free_pages
        .saturating_add(snapshot.reclaimable_cached_pages)
        .min(snapshot.total_pages);
    // Linux Cached 来自 NR_FILE_PAGES，含 shmem（tmpfs/memfd/SysV/GEM shmem）页；磁盘页缓存另计。
    let cached_pages = snapshot.cached_pages + snapshot.shmem_pages;
    proc_text(format_args!(
        "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\nBuffers:        0 kB\nCached:         {} kB\nSwapCached:     {} kB\nActive:         0 kB\nInactive:       0 kB\nSwapTotal:      {} kB\nSwapFree:       {} kB\nDirty:          {} kB\nWriteback:      0 kB\nAnonPages:      {} kB\nMapped:         {} kB\nShmem:          {} kB\nSlab:           {} kB\nKernelStack:    {} kB\nPageTables:     {} kB\n",
        snapshot.total_pages * 4,
        snapshot.free_pages * 4,
        available_pages * 4,
        cached_pages * 4,
        snapshot.swap_cached_pages * 4,
        snapshot.swap_total_pages * 4,
        snapshot.swap_free_pages * 4,
        snapshot.dirty_pages * 4,
        snapshot.anon_mapped_pages * 4,
        snapshot.file_mapped_pages * 4,
        snapshot.shmem_pages * 4,
        snapshot.heap_pages * 4,
        snapshot.kernel_stack_bytes / 1024,
        snapshot.page_table_pages * 4,
    ))
}

pub(super) fn format_buddyinfo(snapshot: &ProcSnapshot) -> Result<Vec<u8>, FileSystemError> {
    let mut output = ProcText::new();
    output
        .write_str("Node 0, zone   Normal ")
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let order_count = (usize::BITS - snapshot.total_pages.max(1).leading_zeros()) as usize;
    for blocks in snapshot.buddy_free_blocks.iter().take(order_count) {
        write!(output, " {blocks:>6}").map_err(|_| FileSystemError::OutOfMemory)?;
    }
    output
        .write_char('\n')
        .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(output.finish())
}

pub(super) fn format_vmstat(snapshot: &ProcSnapshot) -> Result<Vec<u8>, FileSystemError> {
    proc_text(format_args!(
        "allocstall {}\npgscan_direct {}\npgsteal_direct {}\npgscan_kswapd {}\npgsteal_kswapd {}\n",
        snapshot.direct_reclaim_attempts,
        snapshot.direct_reclaim_scanned_pages,
        snapshot.direct_reclaim_reclaimed_pages,
        snapshot.background_reclaim_scanned_pages,
        snapshot.background_reclaim_reclaimed_pages,
    ))
}

pub(super) fn format_loadavg(snapshot: &ProcSnapshot) -> Result<Vec<u8>, FileSystemError> {
    proc_text(format_args!(
        "{}.{:02} {}.{:02} {}.{:02} {}/{} {}\n",
        snapshot.load_milli[0] / 1000,
        snapshot.load_milli[0] / 10 % 100,
        snapshot.load_milli[1] / 1000,
        snapshot.load_milli[1] / 10 % 100,
        snapshot.load_milli[2] / 1000,
        snapshot.load_milli[2] / 10 % 100,
        snapshot.runnable_tasks,
        snapshot.total_tasks,
        snapshot.last_pid
    ))
}

pub(super) fn format_uptime(
    snapshot: &ProcSnapshot,
    offsets: crate::namespace::TimeOffsets,
) -> Result<Vec<u8>, FileSystemError> {
    let idle_us: u64 = snapshot
        .cpus
        .iter()
        .map(|cpu| {
            snapshot
                .uptime_us
                .saturating_sub(cpu.busy_us.min(snapshot.uptime_us))
        })
        .sum();
    let uptime_ns = offsets.project(
        crate::namespace::TimeClock::Boottime,
        snapshot.uptime_us.saturating_mul(1_000),
    );
    proc_text(format_args!(
        "{}.{:02} {}.{:02}\n",
        uptime_ns / 1_000_000_000,
        uptime_ns / 10_000_000 % 100,
        idle_us / 1_000_000,
        idle_us / 10_000 % 100
    ))
}
