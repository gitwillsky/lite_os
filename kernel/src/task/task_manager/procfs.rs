use core::sync::atomic::Ordering;

use crate::{
    arch::hart,
    fs::{
        ProcCpuSnapshot, ProcFileDescriptorSnapshot, ProcIoSnapshot, ProcNetworkSnapshot,
        ProcProcessSnapshot, ProcSnapshot, ProcSource, ProcThreadSnapshot, page_cache_statistics,
    },
    memory::{frame_statistics, reclaim_statistics},
    task::{RunState, current_task, processor::cpu_runtime_snapshot},
    timer::{boot_epoch_seconds, get_time_us},
};

use super::{ProcessState, TASK_MANAGER};

/// @description task façade 对外提供的系统运行状态快照，不拥有任何统计状态。
pub(crate) struct SystemInfoSnapshot {
    /// 自启动起的 monotonic 微秒数。
    pub(crate) uptime_us: u64,
    /// allocator 当前管理的物理内存字节数。
    pub(crate) total_memory_bytes: u64,
    /// allocator 当前空闲的物理内存字节数。
    pub(crate) free_memory_bytes: u64,
    /// process graph 当前 live thread 数量。
    pub(crate) task_count: usize,
    /// TaskManager 唯一 EWMA owner 投影出的 1/5/15 分钟千分制负载。
    pub(crate) load_milli: [u64; 3],
}

/// @description 将 task/memory/processor 的权威状态投影为 procfs 只读快照。
pub(crate) struct KernelProcSource;

impl ProcSource for KernelProcSource {
    fn snapshot(&self) -> Result<ProcSnapshot, crate::fs::FileSystemError> {
        process_snapshot()
    }

    fn current_pid(&self) -> Option<usize> {
        crate::task::current_task().map(|task| task.tgid())
    }

    fn process_arguments(
        &self,
        pid: usize,
    ) -> Result<Option<alloc::vec::Vec<u8>>, crate::fs::FileSystemError> {
        let representative = {
            let graph = TASK_MANAGER.graph.lock();
            let Some(node) = graph.nodes.get(&pid) else {
                return Ok(None);
            };
            let ProcessState::Live(threads) = &node.state else {
                return Ok(None);
            };
            let Some(representative) = threads.values().next() else {
                return Ok(None);
            };
            representative.clone()
        };
        match representative.process_arguments() {
            Ok(arguments) => Ok(Some(arguments)),
            Err(crate::memory::UserAccessError::OutOfMemory) => {
                Err(crate::fs::FileSystemError::OutOfMemory)
            }
            Err(_) => Ok(None),
        }
    }

    fn process_file_descriptors(
        &self,
        pid: usize,
    ) -> Result<Option<alloc::vec::Vec<ProcFileDescriptorSnapshot>>, crate::fs::FileSystemError>
    {
        let representative = {
            let graph = TASK_MANAGER.graph.lock();
            let Some(node) = graph.nodes.get(&pid) else {
                return Ok(None);
            };
            let ProcessState::Live(threads) = &node.state else {
                return Ok(None);
            };
            let Some(representative) = threads.values().next() else {
                return Ok(None);
            };
            representative.clone()
        };
        let Some(caller) = crate::task::current_task() else {
            return Err(crate::fs::FileSystemError::AccessDenied);
        };
        let caller_euid = caller.credential_res_ids(true)[1];
        let target_uids = representative.credential_res_ids(true);
        if caller.tgid() != pid
            && caller_euid != 0
            && target_uids.iter().any(|uid| *uid != caller_euid)
        {
            return Err(crate::fs::FileSystemError::AccessDenied);
        }
        Ok(representative.process_file_descriptors())
    }
}

fn process_snapshot() -> Result<ProcSnapshot, crate::fs::FileSystemError> {
    let uptime_us = get_time_us();
    let current = current_task();
    // 1. graph lock 内只复制关系元数据与 Arc；后续不得带 graph lock 获取 task 内部锁。
    let (rows, last_pid, processes_created) = {
        let graph = TASK_MANAGER.graph.lock();
        let mut rows = alloc::vec::Vec::new();
        rows.try_reserve_exact(graph.nodes.len())
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        for (&pid, node) in &graph.nodes {
            let ProcessState::Live(threads) = &node.state else {
                continue;
            };
            let Some(representative) = threads.values().next() else {
                continue;
            };
            let mut thread_rows = alloc::vec::Vec::new();
            thread_rows
                .try_reserve_exact(threads.len())
                .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
            thread_rows.extend(threads.values().cloned());
            rows.push((
                pid,
                node.parent.unwrap_or(0),
                node.process_group,
                node.session,
                representative.clone(),
                thread_rows,
            ));
        }
        (
            rows,
            graph.next_pid.saturating_sub(1),
            graph.processes_created,
        )
    };

    // 2. 聚合每个 live thread 的 scheduler 状态；Process 级内存只从 representative 读取一次。
    let mut runnable_tasks = 0;
    let mut total_tasks = 0;
    let mut processes = alloc::vec::Vec::new();
    processes
        .try_reserve_exact(rows.len())
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    for (pid, ppid, process_group, session, representative, threads) in rows {
        let mut thread_snapshots = alloc::vec::Vec::new();
        thread_snapshots
            .try_reserve_exact(threads.len())
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        for thread in &threads {
            total_tasks += 1;
            let run_state = thread.scheduling.state.lock().run_state();
            let runnable = matches!(
                run_state,
                RunState::New
                    | RunState::Ready { .. }
                    | RunState::Running { .. }
                    | RunState::Preempting { .. }
                    | RunState::WakePending { .. }
                    | RunState::StopPending { .. }
            );
            if runnable {
                runnable_tasks += 1;
            }
            let active_now_us = current
                .as_ref()
                .is_some_and(|current| current.tid() == thread.tid())
                .then_some(uptime_us);
            let (start_time_us, nice, priority, runtime_us) =
                thread.thread_statistics(active_now_us);
            thread_snapshots.push(ProcThreadSnapshot {
                tid: thread.tid(),
                state: if runnable {
                    b'R'
                } else if matches!(run_state, RunState::Stopped { .. }) {
                    b'T'
                } else {
                    b'S'
                },
                nice,
                priority,
                runtime_us,
                start_time_us,
                last_cpu: hart::hart_index(thread.scheduling.last_cpu.load(Ordering::Relaxed))
                    .expect("task last_cpu disappeared from topology"),
                io: io_snapshot(thread.thread_io_statistics()),
            });
        }
        let leader = thread_snapshots
            .iter()
            .find(|thread| thread.tid == pid)
            .unwrap_or_else(|| {
                thread_snapshots
                    .first()
                    .expect("live process has no threads")
            });
        let (state, nice, priority, last_cpu) =
            (leader.state, leader.nice, leader.priority, leader.last_cpu);
        // 1. Linux 只刷新 same-thread-group 的 current task；其他 running sibling 由下一 tick 提交。
        // 2. Process counter 保留 exited Thread；改回累加 live Thread 会让 /proc runtime 倒退。
        let runtime_us = match current.as_ref() {
            Some(task) if task.tgid() == pid => task.cpu_runtime_snapshot(uptime_us).0,
            _ => representative.process_cpu_runtime_us(),
        };
        let statistics = representative
            .process_statistics()
            .map_err(|()| crate::fs::FileSystemError::OutOfMemory)?;
        let uids = representative.credential_res_ids(true);
        let gids = representative.credential_res_ids(false);
        let groups = representative
            .supplementary_groups()
            .map_err(|()| crate::fs::FileSystemError::OutOfMemory)?;
        let (tty_number, terminal_process_group) = representative.terminal_proc_identity(session);
        processes.push(ProcProcessSnapshot {
            pid,
            ppid,
            process_group,
            session,
            tty_number,
            terminal_process_group,
            comm: statistics.comm,
            uids,
            gids,
            groups,
            state,
            nice,
            priority,
            threads: thread_snapshots,
            runtime_us,
            start_time_us: statistics.start_time_us,
            virtual_pages: statistics.virtual_pages,
            resident_pages: statistics.resident_pages,
            shared_pages: statistics.shared_pages,
            text_pages: statistics.text_pages,
            data_pages: statistics.data_pages,
            fd_size: statistics.fd_size,
            last_cpu,
            io: io_snapshot(representative.process_io_statistics()),
        });
    }

    // 3. allocator 与 per-hart processor 分别提供其唯一 owner 下的统计。
    let frame = frame_statistics();
    let heap = crate::memory::heap_statistics();
    let reclaim = reclaim_statistics();
    let cache = page_cache_statistics();
    let load_milli = TASK_MANAGER.load_average.values();
    let cpu_runtime =
        cpu_runtime_snapshot().map_err(|()| crate::fs::FileSystemError::OutOfMemory)?;
    let mut cpus = alloc::vec::Vec::new();
    cpus.try_reserve_exact(cpu_runtime.len())
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    cpus.extend(
        cpu_runtime
            .into_iter()
            .map(|(hart_id, busy_us)| ProcCpuSnapshot {
                cpu: hart::hart_index(hart_id).expect("processor hart disappeared from topology"),
                busy_us,
            }),
    );
    let network = crate::socket::network_snapshot().map(|snapshot| ProcNetworkSnapshot {
        address: snapshot.address.map(|address| address.octets()),
        prefix_length: snapshot.prefix_length,
        gateway: snapshot.gateway.map(|address| address.octets()),
        up: snapshot.up,
        received_bytes: snapshot.statistics.received_bytes,
        received_packets: snapshot.statistics.received_packets,
        transmitted_bytes: snapshot.statistics.transmitted_bytes,
        transmitted_packets: snapshot.statistics.transmitted_packets,
    });
    Ok(ProcSnapshot {
        uptime_us,
        boot_epoch_seconds: boot_epoch_seconds(),
        total_pages: frame.capacity_pages,
        free_pages: frame.free_pages,
        buddy_free_blocks: frame.free_blocks,
        direct_reclaim_attempts: reclaim.attempts,
        direct_reclaim_scanned_pages: reclaim.scanned_pages,
        direct_reclaim_reclaimed_pages: reclaim.reclaimed_pages,
        cached_pages: cache.resident_pages,
        dirty_pages: cache.dirty_pages,
        reclaimable_cached_pages: cache.reclaimable_pages,
        heap_pages: heap.resident_pages,
        runnable_tasks,
        total_tasks,
        processes_created,
        last_pid,
        load_milli,
        cpus,
        processes,
        network,
    })
}

fn io_snapshot(statistics: crate::task::IoStatistics) -> ProcIoSnapshot {
    ProcIoSnapshot {
        read_characters: statistics.read_characters,
        written_characters: statistics.written_characters,
        read_syscalls: statistics.read_syscalls,
        write_syscalls: statistics.write_syscalls,
        read_bytes: statistics.read_bytes,
        write_bytes: statistics.write_bytes,
    }
}

/// @description 从 procfs 共用的采集边界投影系统级运行状态，避免 syscall 复制统计路径。
///
/// @return 当前 uptime、内存、任务数与 1/5/15 分钟负载的不可变快照。
pub(crate) fn system_info_snapshot() -> SystemInfoSnapshot {
    let frame = frame_statistics();
    let task_count = TASK_MANAGER
        .graph
        .lock()
        .nodes
        .values()
        .filter_map(|node| match &node.state {
            ProcessState::Live(threads) => Some(threads.len()),
            ProcessState::Exited(_) => None,
        })
        .sum();
    let page_size = crate::memory::PAGE_SIZE as u64;
    SystemInfoSnapshot {
        uptime_us: get_time_us(),
        total_memory_bytes: (frame.capacity_pages as u64).saturating_mul(page_size),
        free_memory_bytes: (frame.free_pages as u64).saturating_mul(page_size),
        task_count,
        load_milli: TASK_MANAGER.load_average.values(),
    }
}
