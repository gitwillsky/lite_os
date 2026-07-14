use core::sync::atomic::Ordering;

use crate::{
    arch::hart,
    fs::{
        ProcCpuSnapshot, ProcFileDescriptorSnapshot, ProcNetworkSnapshot, ProcProcessSnapshot,
        ProcSnapshot, ProcSource,
    },
    memory::frame_statistics,
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
    fn snapshot(&self) -> ProcSnapshot {
        process_snapshot()
    }

    fn current_pid(&self) -> Option<usize> {
        crate::task::current_task().map(|task| task.tgid())
    }

    fn process_arguments(&self, pid: usize) -> Option<alloc::vec::Vec<u8>> {
        let representative = {
            let graph = TASK_MANAGER.graph.lock();
            let node = graph.nodes.get(&pid)?;
            let ProcessState::Live(threads) = &node.state else {
                return None;
            };
            threads.values().next()?.clone()
        };
        representative.process_arguments()
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

fn process_snapshot() -> ProcSnapshot {
    let uptime_us = get_time_us();
    let current = current_task();
    // 1. graph lock 内只复制关系元数据与 Arc；后续不得带 graph lock 获取 task 内部锁。
    let (rows, last_pid, processes_created) = {
        let graph = TASK_MANAGER.graph.lock();
        let mut rows = alloc::vec::Vec::new();
        for (&pid, node) in &graph.nodes {
            let ProcessState::Live(threads) = &node.state else {
                continue;
            };
            let Some(representative) = threads.values().next() else {
                continue;
            };
            rows.push((
                pid,
                node.parent.unwrap_or(0),
                node.process_group,
                node.session,
                representative.clone(),
                threads.values().cloned().collect::<alloc::vec::Vec<_>>(),
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
    let mut processes = alloc::vec::Vec::with_capacity(rows.len());
    for (pid, ppid, process_group, session, representative, threads) in rows {
        let mut state = b'S';
        for thread in &threads {
            total_tasks += 1;
            let run_state = thread.scheduling.state.lock().run_state;
            if matches!(
                run_state,
                RunState::New
                    | RunState::Ready { .. }
                    | RunState::Running { .. }
                    | RunState::Preempting { .. }
                    | RunState::WakePending { .. }
                    | RunState::StopPending { .. }
            ) {
                runnable_tasks += 1;
                state = b'R';
            } else if matches!(run_state, RunState::Stopped { .. }) {
                state = b'T';
            }
        }
        // 1. Linux 只刷新 same-thread-group 的 current task；其他 running sibling 由下一 tick 提交。
        // 2. Process counter 保留 exited Thread；改回累加 live Thread 会让 /proc runtime 倒退。
        let runtime_us = match current.as_ref() {
            Some(task) if task.tgid() == pid => task.cpu_runtime_snapshot(uptime_us).0,
            _ => representative.process_cpu_runtime_us(),
        };
        let policy = representative.scheduling.policy.lock();
        let nice = policy.nice;
        let priority = policy.get_dynamic_priority();
        drop(policy);
        let (comm, start_time_us, virtual_pages, resident_pages, fd_size) =
            representative.process_statistics();
        let uids = representative.credential_res_ids(true);
        let gids = representative.credential_res_ids(false);
        let groups = representative.supplementary_groups();
        processes.push(ProcProcessSnapshot {
            pid,
            ppid,
            process_group,
            session,
            comm,
            uids,
            gids,
            groups,
            state,
            nice,
            priority,
            threads: threads.len(),
            runtime_us,
            start_time_us,
            virtual_pages,
            resident_pages,
            fd_size,
            last_cpu: hart::hart_index(representative.scheduling.last_cpu.load(Ordering::Relaxed))
                .expect("task last_cpu disappeared from topology"),
        });
    }

    // 3. allocator 与 per-hart processor 分别提供其唯一 owner 下的统计。
    let (total_pages, free_pages) = frame_statistics();
    let load_milli = TASK_MANAGER.load_average.values();
    let cpus = cpu_runtime_snapshot()
        .into_iter()
        .map(|(hart_id, busy_us)| ProcCpuSnapshot {
            cpu: hart::hart_index(hart_id).expect("processor hart disappeared from topology"),
            busy_us,
        })
        .collect();
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
    ProcSnapshot {
        uptime_us,
        boot_epoch_seconds: boot_epoch_seconds(),
        total_pages,
        free_pages,
        runnable_tasks,
        total_tasks,
        processes_created,
        last_pid,
        load_milli,
        cpus,
        processes,
        network,
    }
}

/// @description 从 procfs 共用的采集边界投影系统级运行状态，避免 syscall 复制统计路径。
///
/// @return 当前 uptime、内存、任务数与 1/5/15 分钟负载的不可变快照。
pub(crate) fn system_info_snapshot() -> SystemInfoSnapshot {
    let snapshot = process_snapshot();
    let page_size = crate::memory::PAGE_SIZE as u64;
    SystemInfoSnapshot {
        uptime_us: snapshot.uptime_us,
        total_memory_bytes: (snapshot.total_pages as u64).saturating_mul(page_size),
        free_memory_bytes: (snapshot.free_pages as u64).saturating_mul(page_size),
        task_count: snapshot.total_tasks,
        load_milli: snapshot.load_milli,
    }
}
