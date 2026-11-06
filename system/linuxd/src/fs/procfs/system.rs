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

pub(super) fn format_meminfo(
    snapshot: &ProcSnapshot,
    mapped: &crate::memory::MappedMemory,
) -> Result<Vec<u8>, FileSystemError> {
    let kernel = native_runtime::system::memory_stats();
    // 1. 没有交换设备，交换项为 0；内核在内存紧张时淘汰干净的文件页，MemAvailable 计入可淘汰的干净缓存。
    // 2. AnonPages/Mapped 来自 linuxd 的 VMA 后备对象（跨进程去重），Shmem 另加 tmpfs/memfd 的已计费页；
    //    Slab/KernelStack/PageTables 来自内核的内存统计（统计范围记录在 docs/syscall-support/memory.md）。
    let clean_cache = snapshot.cached_pages.saturating_sub(snapshot.dirty_pages);
    let shmem = mapped.shmem_pages + crate::fs::memory_file_pages();
    proc_text(format_args!(
        "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\nBuffers:        0 kB\nCached:         {} kB\nSwapCached:     0 kB\nActive:         0 kB\nInactive:       0 kB\nSwapTotal:      0 kB\nSwapFree:       0 kB\nDirty:          {} kB\nWriteback:      0 kB\nAnonPages:      {} kB\nMapped:         {} kB\nShmem:          {} kB\nSlab:           {} kB\nKernelStack:    {} kB\nPageTables:     {} kB\n",
        snapshot.total_pages * 4,
        snapshot.free_pages * 4,
        (snapshot.free_pages + clean_cache).min(snapshot.total_pages) * 4,
        snapshot.cached_pages * 4,
        snapshot.dirty_pages * 4,
        mapped.anonymous_pages * 4,
        mapped.mapped_pages * 4,
        shmem * 4,
        kernel.heap_pages * 4,
        kernel.kernel_stack_pages * 4,
        kernel.page_table_pages * 4,
    ))
}

pub(super) fn format_buddyinfo(snapshot: &ProcSnapshot) -> Result<Vec<u8>, FileSystemError> {
    let mut output = ProcText::new();
    output
        .write_str("Node 0, zone   Normal ")
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let order_count = (usize::BITS - snapshot.total_pages.max(1).leading_zeros()) as usize;
    for order in 0..order_count {
        let blocks = snapshot.buddy_free_blocks.get(order).copied().unwrap_or(0);
        write!(output, " {blocks:>6}").map_err(|_| FileSystemError::OutOfMemory)?;
    }
    output
        .write_char('\n')
        .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(output.finish())
}

pub(super) fn format_vmstat(snapshot: &ProcSnapshot) -> Result<Vec<u8>, FileSystemError> {
    let kernel = native_runtime::system::memory_stats();
    // 淘汰只在缺页入口同步进行（direct reclaim），没有后台 kswapd；不单独计扫描数，scan 与 steal 相同。
    proc_text(format_args!(
        "nr_free_pages {}\nnr_dirty {}\nallocstall 0\npgscan_direct {}\npgsteal_direct {}\npgscan_kswapd 0\npgsteal_kswapd 0\n",
        snapshot.free_pages, snapshot.dirty_pages, kernel.evicted_pages, kernel.evicted_pages,
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
