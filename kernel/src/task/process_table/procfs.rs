use core::sync::atomic::Ordering;

use crate::{
    cpu,
    fs::{
        ProcCpuSnapshot, ProcCredentialSnapshot, ProcFileDescriptorSnapshot, ProcIoSnapshot,
        ProcNetworkSnapshot, ProcProcessSnapshot, ProcRouteSnapshot, ProcSnapshot,
        ProcSnapshotScope, ProcSource, ProcThreadSnapshot, page_cache_statistics,
    },
    memory::{frame_statistics, reclaim_statistics},
    task::{
        RunState, current_task,
        processor::{cpu_runtime_snapshot, runnable_task_count},
    },
    timer::{boot_epoch_seconds, get_time_us},
};

use super::{PROCESS_TABLE, ProcessState};

struct ProcessSnapshotRow {
    pid: alloc::sync::Arc<crate::namespace::PidIdentity>,
    ppid: Option<alloc::sync::Arc<crate::namespace::PidIdentity>>,
    process_group: alloc::sync::Arc<crate::namespace::PidIdentity>,
    session: alloc::sync::Arc<crate::namespace::PidIdentity>,
    process: alloc::sync::Arc<crate::task::model::Process>,
    threads: core::ops::Range<usize>,
}

struct GraphSnapshotRows {
    process: Option<ProcessSnapshotRow>,
    kernel: Option<alloc::sync::Arc<crate::task::TaskControlBlock>>,
    tasks: alloc::vec::Vec<alloc::sync::Arc<crate::task::TaskControlBlock>>,
    total_tasks: usize,
    last_pid: usize,
    processes_created: u64,
}

fn graph_snapshot_rows(
    scope: ProcSnapshotScope,
    view: &crate::namespace::PidNamespace,
) -> Result<GraphSnapshotRows, crate::fs::FileSystemError> {
    let mut thread_capacity = 0;
    loop {
        // 1. 只为请求的线程组准备 storage；System 不分配、不 pin 任意 TCB。
        let mut tasks = alloc::vec::Vec::new();
        tasks
            .try_reserve_exact(thread_capacity)
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        let graph = PROCESS_TABLE.graph.lock();
        let node = match scope {
            ProcSnapshotScope::Process(key) => graph.nodes.get(&key),
            ProcSnapshotScope::System => None,
        };
        let kernel = match scope {
            ProcSnapshotScope::Process(key) if node.is_none() => {
                graph.kernel_threads.get(&key).cloned()
            }
            _ => None,
        };
        let required_threads = node.map_or(0, |node| match &node.state {
            ProcessState::Live(threads) => threads.len(),
            ProcessState::Exited(..) => 0,
        });
        if required_threads > tasks.capacity() {
            thread_capacity = required_threads;
            drop(graph);
            continue;
        }
        // 2. 容量验证、目标关系与实际 TID index 大小在同一 graph linearization 读取。
        // 所有 push 都已预留；没有全局 Process/Thread 遍历或无关资源保活。
        let process = node.map(|node| {
            if let ProcessState::Live(threads) = &node.state {
                tasks.extend(threads.values().cloned());
            }
            let group = graph
                .groups
                .get(&node.process_group)
                .expect("live process has actual group/session registration");
            ProcessSnapshotRow {
                pid: node.pid.identity().clone(),
                ppid: node
                    .parent
                    .and_then(|key| graph.nodes.get(&key))
                    .map(|parent| parent.pid.identity().clone()),
                process_group: group.pgid.identity().clone(),
                session: group.sid.identity().clone(),
                process: node.process.clone(),
                threads: 0..tasks.len(),
            }
        });
        return Ok(GraphSnapshotRows {
            process,
            kernel,
            tasks,
            total_tasks: graph.live_task_count(),
            last_pid: view.last_allocated() as usize,
            processes_created: graph.processes_created,
        });
    }
}

/// task façade 对外提供的系统运行状态快照，不拥有任何统计状态。
pub(crate) struct SystemInfoSnapshot {
    /// 自启动起的 monotonic 微秒数。
    pub(crate) uptime_us: u64,
    /// allocator 当前管理的物理内存字节数。
    pub(crate) total_memory_bytes: u64,
    /// allocator 当前空闲的物理内存字节数。
    pub(crate) free_memory_bytes: u64,
    pub(crate) total_swap_bytes: u64,
    pub(crate) free_swap_bytes: u64,
    /// 实际 user/kernel execution index 的 live task 总数；不计 idle。
    pub(crate) task_count: usize,
    /// ProcessTable 唯一 EWMA owner 投影出的 1/5/15 分钟千分制负载。
    pub(crate) load_milli: [u64; 3],
}

/// 将 task/memory/processor 的权威状态投影为 procfs 只读快照。
pub(crate) struct KernelProcSource;

impl ProcSource for KernelProcSource {
    fn process_identity(
        &self,
        key: usize,
    ) -> Option<alloc::sync::Arc<crate::namespace::PidIdentity>> {
        let graph = PROCESS_TABLE.graph.lock();
        graph
            .nodes
            .get(&key)
            .map(|node| node.pid.identity().clone())
            .or_else(|| {
                graph
                    .kernel_threads
                    .get(&key)
                    .map(|thread| thread.tgid_identity().clone())
            })
    }
    fn process_identities(
        &self,
        view: &crate::namespace::PidNamespace,
    ) -> Result<
        alloc::vec::Vec<alloc::sync::Arc<crate::namespace::PidIdentity>>,
        crate::fs::FileSystemError,
    > {
        let capacity = {
            let graph = PROCESS_TABLE.graph.lock();
            graph.nodes.len() + graph.kernel_threads.len()
        };
        let mut identities = alloc::vec::Vec::new();
        identities
            .try_reserve_exact(capacity)
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        let mut cursor = 0;
        while let Some((number, key)) = view.next_member(cursor) {
            cursor = number;
            let identity = {
                let graph = PROCESS_TABLE.graph.lock();
                graph
                    .nodes
                    .get(&key.identity())
                    .map(|node| node.pid.identity().clone())
                    .or_else(|| {
                        graph
                            .kernel_threads
                            .get(&key.identity())
                            .map(|task| task.tgid_identity().clone())
                    })
            };
            if let Some(identity) = identity {
                // namespace number 的升序就是 getdents 顺序；race 中的新 TGID 只在 graph guard 外扩容。
                identities
                    .try_reserve(1)
                    .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
                identities.push(identity);
            }
        }
        Ok(identities)
    }

    fn task_identity(
        &self,
        view: &crate::namespace::PidNamespace,
        number: u32,
    ) -> Option<crate::fs::ProcTaskIdentity> {
        let key = view.lookup(number)?.identity();
        let graph = PROCESS_TABLE.graph.lock();
        if let Some(node) = graph.nodes.get(&key) {
            return Some(crate::fs::ProcTaskIdentity {
                process: node.pid.identity().clone(),
                thread: node.pid.identity().clone(),
            });
        }
        if let Some(kernel) = graph.kernel_threads.get(&key) {
            return Some(crate::fs::ProcTaskIdentity {
                process: kernel.tgid_identity().clone(),
                thread: kernel.tid_identity(),
            });
        }
        let thread = graph.threads.get(&key)?;
        let process = graph.nodes.get(&thread.tgid)?;
        Some(crate::fs::ProcTaskIdentity {
            process: process.pid.identity().clone(),
            thread: thread.pid.identity().clone(),
        })
    }

    fn current_namespace_authority(
        &self,
    ) -> Result<crate::namespace::NamespaceAuthority, crate::fs::FileSystemError> {
        crate::task::current_task()
            .map(|task| task.namespace_authority())
            .ok_or(crate::fs::FileSystemError::AccessDenied)
    }
    fn kernel_command_line(&self) -> &[u8] {
        crate::platform::kernel_command_line()
    }

    fn snapshot(
        &self,
        scope: ProcSnapshotScope,
        view: &crate::namespace::PidNamespace,
    ) -> Result<ProcSnapshot, crate::fs::FileSystemError> {
        process_snapshot(scope, view)
    }
    fn current_ipc_namespace(
        &self,
    ) -> Result<alloc::sync::Arc<crate::ipc::IpcNamespace>, crate::fs::FileSystemError> {
        current_task()
            .map(|task| task.ipc_namespace())
            .ok_or(crate::fs::FileSystemError::AccessDenied)
    }
    fn current_pid_namespace(&self) -> alloc::sync::Arc<crate::namespace::PidNamespace> {
        // boot 的 proc mount 发生在 init 构造前；其唯一 PID view 是已初始化的 initial owner。
        current_task().map_or_else(crate::namespace::initial_pid, |task| task.pid_namespace())
    }
    fn current_net_namespace(
        &self,
    ) -> Result<alloc::sync::Arc<crate::socket::NetNamespace>, crate::fs::FileSystemError> {
        current_task()
            .map(|task| task.net_namespace())
            .ok_or(crate::fs::FileSystemError::AccessDenied)
    }

    fn thread_net_namespace(
        &self,
        tid: usize,
    ) -> Option<alloc::sync::Arc<crate::socket::NetNamespace>> {
        let target = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::execution_by_tid(&graph, tid)
        }?;
        target.proc_net_namespace()
    }
    fn network_snapshot(
        &self,
        target: &crate::socket::NetNamespace,
    ) -> Result<alloc::vec::Vec<ProcNetworkSnapshot>, crate::fs::FileSystemError> {
        let interfaces = crate::socket::network_snapshot(target).map_err(|error| match error {
            crate::socket::SocketError::NoMemory => crate::fs::FileSystemError::OutOfMemory,
            _ => crate::fs::FileSystemError::IoError,
        })?;
        let mut rows = alloc::vec::Vec::new();
        rows.try_reserve_exact(interfaces.len())
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        rows.extend(interfaces.into_iter().map(|snapshot| ProcNetworkSnapshot {
            name: snapshot.name,
            received_bytes: snapshot.statistics.received_bytes,
            received_packets: snapshot.statistics.received_packets,
            received_errors: snapshot.statistics.received_errors,
            received_dropped: snapshot.statistics.received_dropped,
            transmitted_bytes: snapshot.statistics.transmitted_bytes,
            transmitted_packets: snapshot.statistics.transmitted_packets,
            transmitted_errors: snapshot.statistics.transmitted_errors,
            transmitted_dropped: snapshot.statistics.transmitted_dropped,
        }));
        Ok(rows)
    }

    fn route_snapshot(
        &self,
        target: &crate::socket::NetNamespace,
    ) -> Result<alloc::vec::Vec<ProcRouteSnapshot>, crate::fs::FileSystemError> {
        use crate::socket::Ipv4RouteKind;
        let routes = crate::socket::route_snapshot(target).map_err(|error| match error {
            crate::socket::SocketError::NoMemory => crate::fs::FileSystemError::OutOfMemory,
            _ => crate::fs::FileSystemError::IoError,
        })?;
        let mut rows = alloc::vec::Vec::new();
        rows.try_reserve_exact(routes.len())
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        rows.extend(
            routes
                .into_iter()
                .filter(|snapshot| {
                    snapshot.route.table == 254 && snapshot.route.kind != Ipv4RouteKind::Broadcast
                })
                .map(|snapshot| {
                    let route = snapshot.route;
                    ProcRouteSnapshot {
                        name: snapshot.name,
                        destination: route.destination.octets(),
                        gateway: route
                            .gateway
                            .unwrap_or(core::net::Ipv4Addr::UNSPECIFIED)
                            .octets(),
                        prefix_length: route.prefix_length,
                        metric: route.metric,
                        mtu: if route.metrics.advertised_mss == 0 {
                            0
                        } else {
                            route.metrics.advertised_mss + 40
                        },
                        window: route.metrics.receive_window,
                        initial_rtt: route.metrics.round_trip_time >> 3,
                        flags: 1
                            | if route.gateway.is_some() { 2 } else { 0 }
                            | if route.prefix_length == 32 { 4 } else { 0 }
                            | if matches!(route.kind, Ipv4RouteKind::Unreachable) {
                                0x200
                            } else {
                                0
                            },
                    }
                }),
        );
        Ok(rows)
    }

    fn thread_inode_owner(
        &self,
        tid: usize,
        mode: u32,
    ) -> Result<Option<(u32, u32)>, crate::fs::FileSystemError> {
        let target = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::execution_by_tid(&graph, tid)
        };
        if let Some(task) = target {
            return Ok(Some(task.proc_inode_owner(mode)));
        }
        let process = {
            let graph = PROCESS_TABLE.graph.lock();
            graph.nodes.get(&tid).map(|node| node.process.clone())
        };
        Ok(process.map(|process| {
            let credentials = process.process_credential_snapshot();
            if mode == 0o040555 {
                (credentials.uids[1], credentials.gids[1])
            } else {
                (0, 0)
            }
        }))
    }

    fn thread_credentials(
        &self,
        tid: usize,
    ) -> Result<Option<ProcCredentialSnapshot>, crate::fs::FileSystemError> {
        let target = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::execution_by_tid(&graph, tid)
        };
        if let Some(task) = target {
            return Ok(Some(task.credential_snapshot()));
        }
        let process = {
            let graph = PROCESS_TABLE.graph.lock();
            graph.nodes.get(&tid).map(|node| node.process.clone())
        };
        Ok(process.map(|process| process.process_credential_snapshot()))
    }

    fn thread_namespace(
        &self,
        tid: usize,
        kind: crate::namespace::NamespaceSelector,
    ) -> Result<Option<crate::namespace::NamespaceReference>, crate::fs::FileSystemError> {
        let target = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::execution_by_tid(&graph, tid)
        };
        let caller = crate::task::current_task().ok_or(crate::fs::FileSystemError::AccessDenied)?;
        if let Some(target) = target {
            if !caller.may_inspect_thread(&target) {
                return Err(crate::fs::FileSystemError::AccessDenied);
            }
            return Ok(target.proc_namespace(kind));
        }
        let retained = {
            let graph = PROCESS_TABLE.graph.lock();
            graph
                .nodes
                .get(&tid)
                .map(|node| (node.process.clone(), node.pid.identity().clone()))
        };
        let Some((process, identity)) = retained else {
            return Ok(None);
        };
        if !caller.may_inspect_process(&process) {
            return Err(crate::fs::FileSystemError::AccessDenied);
        }
        // exit_task_namespaces 已退役 nsproxy；只有 cred/PID 自身仍持有真实 namespace。
        use crate::namespace::{NamespaceReference, NamespaceSelector, NamespaceType};
        Ok(match kind {
            NamespaceSelector::Namespace(NamespaceType::User) => Some(NamespaceReference::new(
                process.process_credential_snapshot().user_namespace,
            )),
            NamespaceSelector::Namespace(NamespaceType::Pid) => {
                Some(NamespaceReference::new(identity.namespace()))
            }
            _ => None,
        })
    }

    fn thread_time_for_children(
        &self,
        tid: usize,
    ) -> Option<alloc::sync::Arc<crate::namespace::TimeNamespace>> {
        let target = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::execution_by_tid(&graph, tid)
        }?;
        target.proc_time_for_children()
    }
    fn current_time_offsets(
        &self,
    ) -> Result<crate::namespace::TimeOffsets, crate::fs::FileSystemError> {
        crate::task::current_task()
            .map(|task| task.time_offsets())
            .ok_or(crate::fs::FileSystemError::AccessDenied)
    }

    fn thread_mount_snapshot(
        &self,
        tid: usize,
    ) -> Option<(
        alloc::sync::Arc<crate::fs::MountNamespace>,
        alloc::sync::Arc<crate::fs::OpenedFile>,
    )> {
        let task = {
            let graph = PROCESS_TABLE.graph.lock();
            super::thread_selector::execution_by_tid(&graph, tid)
        }?;
        task.proc_mount_snapshot()
    }
    fn current_pid(&self, view: &crate::namespace::PidNamespace) -> Option<usize> {
        let task = current_task()?;
        let number = task.tgid_identity().number_in(view);
        (number != 0).then_some(number as usize)
    }

    fn thread_arguments(
        &self,
        tid: usize,
    ) -> Result<Option<alloc::vec::Vec<u8>>, crate::fs::FileSystemError> {
        let thread = {
            let graph = PROCESS_TABLE.graph.lock();
            let thread = super::thread_selector::execution_by_tid(&graph, tid);
            if thread.is_none() {
                return Ok(graph.nodes.contains_key(&tid).then(alloc::vec::Vec::new));
            }
            thread.expect("selected live Thread")
        };
        match thread.proc_arguments() {
            Ok(arguments) => Ok(Some(arguments)),
            Err(crate::memory::UserAccessError::OutOfMemory) => {
                Err(crate::fs::FileSystemError::OutOfMemory)
            }
            // Linux access_remote_vm 无法读取 argument range 时返回空内容，实际 Task 并未消失。
            Err(_) => Ok(Some(alloc::vec::Vec::new())),
        }
    }

    fn thread_file_descriptors(
        &self,
        tid: usize,
    ) -> Result<Option<alloc::vec::Vec<ProcFileDescriptorSnapshot>>, crate::fs::FileSystemError>
    {
        let (thread, process) = {
            let graph = PROCESS_TABLE.graph.lock();
            (
                super::thread_selector::execution_by_tid(&graph, tid),
                graph.nodes.get(&tid).map(|node| node.process.clone()),
            )
        };
        let caller = crate::task::current_task().ok_or(crate::fs::FileSystemError::AccessDenied)?;
        if let Some(thread) = thread {
            if !caller.may_inspect_thread(&thread) {
                return Err(crate::fs::FileSystemError::AccessDenied);
            }
            return thread.proc_file_descriptors().map(Some);
        }
        let Some(process) = process else {
            return Ok(None);
        };
        if !caller.may_inspect_process(&process) {
            return Err(crate::fs::FileSystemError::AccessDenied);
        }
        process.proc_file_descriptors().map(Some)
    }

    fn thread_file_information(
        &self,
        tid: usize,
        fd: usize,
        view: &crate::namespace::PidNamespace,
    ) -> Result<Option<alloc::vec::Vec<u8>>, crate::fs::FileSystemError> {
        let (thread, process) = {
            let graph = PROCESS_TABLE.graph.lock();
            (
                super::thread_selector::execution_by_tid(&graph, tid),
                graph.nodes.get(&tid).map(|node| node.process.clone()),
            )
        };
        let caller = current_task().ok_or(crate::fs::FileSystemError::AccessDenied)?;
        if let Some(thread) = thread {
            if !caller.may_inspect_thread(&thread) {
                return Err(crate::fs::FileSystemError::AccessDenied);
            }
            return thread.proc_file_information(fd, view);
        }
        if let Some(process) = process
            && !caller.may_inspect_process(&process)
        {
            return Err(crate::fs::FileSystemError::AccessDenied);
        }
        // 原 leader 已 exit_files；不能读取仍运行 worker 的表来填补历史 Thread。
        Ok(None)
    }

    fn current_root(&self) -> Option<alloc::sync::Arc<crate::fs::OpenedFile>> {
        crate::task::current_task()
            .filter(|task| !task.is_kernel_thread())
            .map(|task| task.lookup_origin().root())
    }

    fn process_directory(
        &self,
        tid: usize,
        kind: crate::fs::ProcDirectory,
    ) -> Result<Option<alloc::sync::Arc<crate::fs::OpenedFile>>, crate::fs::FileSystemError> {
        let (target, process) = {
            let graph = PROCESS_TABLE.graph.lock();
            (
                super::thread_selector::execution_by_tid(&graph, tid),
                graph.nodes.get(&tid).map(|node| node.process.clone()),
            )
        };
        let Some(caller) = crate::task::current_task() else {
            return Err(crate::fs::FileSystemError::AccessDenied);
        };
        if let Some(target) = target {
            if !caller.may_inspect_thread(&target) {
                return Err(crate::fs::FileSystemError::AccessDenied);
            }
            return Ok(target.proc_directory(kind));
        }
        // 原 leader 的 proc inode 仍存在：先按历史身份授权，再报告已退出的 fs/mm 无 target。
        // 不能在授权前返回 None，泄露退出状态或跳过 Linux proc_fd_access_allowed 的顺序。
        if let Some(process) = process
            && !caller.may_inspect_process(&process)
        {
            return Err(crate::fs::FileSystemError::AccessDenied);
        }
        Ok(None)
    }

    fn thread_executable(
        &self,
        pid: usize,
    ) -> Result<Option<alloc::sync::Arc<crate::fs::OpenedFile>>, crate::fs::FileSystemError> {
        let (representative, process) = {
            let graph = PROCESS_TABLE.graph.lock();
            (
                super::thread_selector::execution_by_tid(&graph, pid),
                graph.nodes.get(&pid).map(|node| node.process.clone()),
            )
        };
        let Some(caller) = crate::task::current_task() else {
            return Err(crate::fs::FileSystemError::AccessDenied);
        };
        if let Some(representative) = representative {
            if !caller.may_inspect_thread(&representative) {
                return Err(crate::fs::FileSystemError::AccessDenied);
            }
            return representative.process_executable();
        }
        // 原 leader 的 proc inode 仍存在：先按历史身份授权，再报告已退出的 fs/mm 无 target。
        // 不能在授权前返回 None，泄露退出状态或跳过 Linux proc_fd_access_allowed 的顺序。
        if let Some(process) = process
            && !caller.may_inspect_process(&process)
        {
            return Err(crate::fs::FileSystemError::AccessDenied);
        }
        Ok(None)
    }
}

fn process_snapshot(
    scope: ProcSnapshotScope,
    view: &crate::namespace::PidNamespace,
) -> Result<ProcSnapshot, crate::fs::FileSystemError> {
    let uptime_us = get_time_us();
    let current = current_task();
    // graph lock 内只做已预留 Vec 的关系/Arc 快照；后续不得带 graph lock 获取 task 内部锁。
    let GraphSnapshotRows {
        process,
        kernel,
        tasks,
        total_tasks,
        last_pid,
        processes_created,
    } = graph_snapshot_rows(scope, view)?;

    // 2. 全局 runnable 数读取实际 scheduler membership；不扫描或保活用户执行体。
    let runnable_tasks = runnable_task_count();
    let mut processes = alloc::vec::Vec::new();
    processes
        .try_reserve_exact(usize::from(process.is_some()) + usize::from(kernel.is_some()))
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    if let Some(row) = process.filter(|row| row.pid.number_in(view) != 0) {
        let ProcessSnapshotRow {
            pid,
            ppid,
            process_group,
            session,
            process,
            threads,
        } = row;
        let threads = &tasks[threads];
        let mut thread_snapshots = alloc::vec::Vec::new();
        thread_snapshots
            .try_reserve_exact(
                threads.len()
                    + usize::from(!threads.iter().any(|thread| thread.tid() == pid.key())),
            )
            .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
        for thread in threads {
            let (run_state, uninterruptible) = {
                let scheduling = thread.scheduling.state.lock();
                (scheduling.run_state(), scheduling.is_uninterruptible())
            };
            let runnable = matches!(
                run_state,
                RunState::New
                    | RunState::Ready { .. }
                    | RunState::Running { .. }
                    | RunState::Preempting { .. }
                    | RunState::WakePending { .. }
                    | RunState::StopPending { .. }
            );
            let active_now_us = current
                .as_ref()
                .is_some_and(|current| current.tid() == thread.tid())
                .then_some(uptime_us);
            let (start_time_us, nice, priority, runtime_us) =
                thread.thread_statistics(active_now_us);
            thread_snapshots.push(ProcThreadSnapshot {
                tid: thread.tid_identity(),
                credentials: thread.credential_snapshot(),
                has_mm: thread.has_mm(),
                fd_size: thread.file_table_capacity(),
                state: if runnable {
                    b'R'
                } else if matches!(run_state, RunState::Stopped { .. }) {
                    b'T'
                } else if uninterruptible {
                    b'D'
                } else {
                    b'S'
                },
                nice,
                priority,
                runtime_us,
                start_time_us,
                last_cpu: cpu::id_at(thread.scheduling.last_cpu.load(Ordering::Relaxed))
                    .expect("task last CPU disappeared from topology")
                    .index(),
                io: io_snapshot(thread.thread_io_statistics()),
            });
        }
        let state = thread_snapshots
            .iter()
            .find(|thread| thread.tid.key() == pid.key())
            .map_or(b'Z', |leader| leader.state);
        let (nice, priority, leader_cpu, leader_runtime_us) = process.process_scheduler_snapshot();
        let last_cpu = cpu::id_at(leader_cpu)
            .expect("leader CPU disappeared from topology")
            .index();
        // 1. Linux 只刷新 same-thread-group 的 current task；其他 running sibling 由下一 tick 提交。
        // 2. Process counter 保留 exited Thread；改回累加 live Thread 会让 /proc runtime 倒退。
        let runtime_us = match current.as_ref() {
            Some(task) if task.tgid() == pid.key() => task.cpu_runtime_snapshot(uptime_us).0,
            _ => process.process_cpu_runtime_us(),
        };
        let statistics = process
            .process_statistics(threads)
            .map_err(|()| crate::fs::FileSystemError::OutOfMemory)?;
        let credentials = process.process_credential_snapshot();
        // 原 leader 已注销时，TGID registration 仍属于真实 Process；用它的同一 owner
        // 投影 zombie task，不保活执行体，也不让 live sibling 的身份/计数替代 leader。
        if !thread_snapshots
            .iter()
            .any(|thread| thread.tid.key() == pid.key())
        {
            thread_snapshots.push(ProcThreadSnapshot {
                tid: pid.clone(),
                credentials: credentials.clone(),
                has_mm: statistics.has_mm,
                fd_size: statistics.fd_size,
                state: b'Z',
                nice,
                priority,
                runtime_us: leader_runtime_us,
                start_time_us: statistics.start_time_us,
                last_cpu,
                io: io_snapshot(process.leader_io_statistics()),
            });
        }
        thread_snapshots.sort_unstable_by_key(|thread| thread.tid.number_in(view));
        let (tty_number, terminal_process_group) = process.terminal_proc_identity(view);
        processes.push(ProcProcessSnapshot {
            kernel_thread: false,
            pid,
            ppid,
            process_group,
            session,
            tty_number,
            terminal_process_group,
            comm: statistics.comm,
            has_mm: statistics.has_mm,
            credentials,
            state,
            nice,
            priority,
            threads: thread_snapshots,
            runtime_us,
            start_time_us: statistics.start_time_us,
            virtual_pages: statistics.virtual_pages,
            resident_pages: statistics.resident_pages,
            swapped_pages: statistics.swapped_pages,
            shared_pages: statistics.shared_pages,
            text_pages: statistics.text_pages,
            data_pages: statistics.data_pages,
            fd_size: statistics.fd_size,
            last_cpu,
            io: io_snapshot(process.process_io_statistics()),
        });
    }

    if let Some(kernel) = kernel
        && kernel.tgid_identity().number_in(view) != 0
    {
        processes.push(kernel_snapshot(&kernel, current.as_deref(), uptime_us)?);
    }

    processes.sort_unstable_by_key(|process| process.pid.number_in(view));

    // 3. allocator 与 per-CPU processor 分别提供其唯一 owner 下的统计。
    let frame = frame_statistics();
    let heap = crate::memory::heap_statistics();
    let reclaim = reclaim_statistics();
    let cache = page_cache_statistics();
    let load_milli = PROCESS_TABLE.load_average.values();
    let cpu_runtime =
        cpu_runtime_snapshot().map_err(|()| crate::fs::FileSystemError::OutOfMemory)?;
    let mut cpus = alloc::vec::Vec::new();
    cpus.try_reserve_exact(cpu_runtime.len())
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    cpus.extend(
        cpu_runtime
            .into_iter()
            .map(|(cpu, busy_us)| ProcCpuSnapshot { cpu, busy_us }),
    );
    let (swap_total_pages, swap_free_pages) = crate::memory::swap::statistics();
    Ok(ProcSnapshot {
        uptime_us,
        boot_epoch_seconds: boot_epoch_seconds(),
        total_pages: frame.capacity_pages,
        free_pages: frame.free_pages,
        buddy_free_blocks: frame.free_blocks,
        direct_reclaim_attempts: reclaim.direct.attempts,
        direct_reclaim_scanned_pages: reclaim.direct.scanned_pages,
        direct_reclaim_reclaimed_pages: reclaim.direct.reclaimed_pages,
        background_reclaim_scanned_pages: reclaim.background.scanned_pages,
        background_reclaim_reclaimed_pages: reclaim.background.reclaimed_pages,
        cached_pages: cache.resident_pages,
        dirty_pages: cache.dirty_pages,
        reclaimable_cached_pages: cache.reclaimable_pages,
        heap_pages: heap.resident_pages,
        swap_total_pages,
        swap_free_pages,
        swap_cached_pages: crate::memory::swap_cached_pages(),
        runnable_tasks,
        total_tasks,
        processes_created,
        last_pid,
        load_milli,
        cpus,
        processes,
    })
}

/// boot worker 的 Linux proc 投影；无用户 Process/mm/file/fs owner，空资源由实际缺失决定。
fn kernel_snapshot(
    task: &crate::task::TaskControlBlock,
    current: Option<&crate::task::TaskControlBlock>,
    now: u64,
) -> Result<ProcProcessSnapshot, crate::fs::FileSystemError> {
    let (run_state, uninterruptible, idle) = {
        let state = task.scheduling.state.lock();
        (
            state.run_state(),
            state.is_uninterruptible(),
            state.is_idle_wait(),
        )
    };
    let state = match run_state {
        RunState::New
        | RunState::Ready { .. }
        | RunState::Running { .. }
        | RunState::Preempting { .. }
        | RunState::WakePending { .. }
        | RunState::StopPending { .. } => b'R',
        RunState::Stopped { .. } => b'T',
        RunState::Exited => b'Z',
        _ if uninterruptible => b'D',
        _ if idle => b'I',
        _ => b'S',
    };
    let active = current
        .is_some_and(|current| current.tid() == task.tid())
        .then_some(now);
    let (start_time_us, nice, priority, runtime_us) = task.thread_statistics(active);
    let last_cpu = task.scheduling.last_cpu.load(Ordering::Relaxed);
    let last_cpu = cpu::id_at(last_cpu)
        .expect("kernel task CPU disappeared")
        .index();
    let credentials = task.credential_snapshot();
    let name = task.kernel_thread_name().as_bytes();
    let mut comm = alloc::vec::Vec::new();
    comm.try_reserve_exact(name.len())
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    comm.extend_from_slice(name);
    let mut threads = alloc::vec::Vec::new();
    threads
        .try_reserve_exact(1)
        .map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    threads.push(ProcThreadSnapshot {
        tid: task.tid_identity(),
        has_mm: false,
        fd_size: 0,
        credentials: credentials.clone(),
        state,
        nice,
        priority,
        runtime_us,
        start_time_us,
        last_cpu,
        io: ProcIoSnapshot::default(),
    });
    Ok(ProcProcessSnapshot {
        kernel_thread: true,
        pid: task.tgid_identity().clone(),
        ppid: None,
        // 此 boot worker 由 idle 创建，SID/PGID 指向实际唯一 initial PID0；无伪造 user Process。
        process_group: crate::namespace::idle_pid_identity(),
        session: crate::namespace::idle_pid_identity(),
        tty_number: 0,
        terminal_process_group: -1,
        comm,
        credentials,
        state,
        nice,
        priority,
        threads,
        runtime_us,
        start_time_us,
        has_mm: false,
        virtual_pages: 0,
        resident_pages: 0,
        swapped_pages: 0,
        shared_pages: 0,
        text_pages: 0,
        data_pages: 0,
        fd_size: 0,
        last_cpu,
        io: ProcIoSnapshot::default(),
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

/// 从 procfs 共用的采集边界投影系统级运行状态，避免 syscall 复制统计路径。
///
/// # Returns
///
/// 当前 uptime、内存、任务数与 1/5/15 分钟负载的不可变快照。
pub(crate) fn system_info_snapshot() -> SystemInfoSnapshot {
    let frame = frame_statistics();
    let task_count = PROCESS_TABLE.graph.lock().live_task_count();
    let page_size = crate::memory::PAGE_SIZE as u64;
    let (swap_total, swap_free) = crate::memory::swap::statistics();
    SystemInfoSnapshot {
        uptime_us: get_time_us(),
        total_memory_bytes: (frame.capacity_pages as u64).saturating_mul(page_size),
        free_memory_bytes: (frame.free_pages as u64).saturating_mul(page_size),
        total_swap_bytes: (swap_total as u64).saturating_mul(page_size),
        free_swap_bytes: (swap_free as u64).saturating_mul(page_size),
        task_count,
        load_milli: PROCESS_TABLE.load_average.values(),
    }
}
