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
    // 1. 内核不回收页缓存，也没有交换设备：MemAvailable 等于 MemFree，交换项为 0。
    // 2. 匿名/映射/shmem/slab/内核栈/页表页属于内核与各服务私有的分配，没有跨进程汇总的 owner，报告 0
    //    （范围缩减记录在 docs/plans/microkernel.md）。
    proc_text(format_args!(
        "MemTotal:       {} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\nBuffers:        0 kB\nCached:         {} kB\nSwapCached:     0 kB\nActive:         0 kB\nInactive:       0 kB\nSwapTotal:      0 kB\nSwapFree:       0 kB\nDirty:          {} kB\nWriteback:      0 kB\nAnonPages:      0 kB\nMapped:         0 kB\nShmem:          0 kB\nSlab:           0 kB\nKernelStack:    0 kB\nPageTables:     0 kB\n",
        snapshot.total_pages * 4,
        snapshot.free_pages * 4,
        snapshot.free_pages.min(snapshot.total_pages) * 4,
        snapshot.cached_pages * 4,
        snapshot.dirty_pages * 4,
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
    // 内核不做页回收：回收计数恒为 0，空闲页来自分配器。
    proc_text(format_args!(
        "nr_free_pages {}\nnr_dirty {}\nallocstall 0\npgscan_direct 0\npgsteal_direct 0\npgscan_kswapd 0\npgsteal_kswapd 0\n",
        snapshot.free_pages, snapshot.dirty_pages,
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
