use alloc::{format, string::String};
use core::fmt::Write;

use super::{ProcIoSnapshot, ProcProcessSnapshot, ProcThreadSnapshot, system::ticks};

/// @description 将 I/O owner 快照编码为 Linux `/proc/<task>/io` 七字段格式。
/// @param io Process 聚合或 Thread 私有 I/O counter 快照。
/// @return 包含尾随换行的 io 文本；当前同步写路径不会产生 cancelled writes。
pub(super) fn format_io(io: &ProcIoSnapshot) -> String {
    format!(
        "rchar: {}\nwchar: {}\nsyscr: {}\nsyscw: {}\nread_bytes: {}\nwrite_bytes: {}\ncancelled_write_bytes: 0\n",
        io.read_characters,
        io.written_characters,
        io.read_syscalls,
        io.write_syscalls,
        io.read_bytes,
        io.write_bytes,
    )
}

/// @description 将 Process snapshot 编码为 Linux `/proc/<pid>/stat` 单行格式。
/// @param process 目标 live Process 的只读快照。
/// @return 包含尾随换行的 stat 文本。
pub(super) fn format_process_stat(process: &ProcProcessSnapshot) -> String {
    format_task_stat(
        process,
        TaskStatFields {
            pid: process.pid,
            state: process.state,
            nice: process.nice,
            priority: process.priority,
            runtime_us: process.runtime_us,
            start_time_us: process.start_time_us,
            last_cpu: process.last_cpu,
        },
    )
}

/// @description 将 Thread snapshot 编码为 Linux `/proc/<tgid>/task/<tid>/stat` 单行格式。
/// @param process Thread 所属 Process 的共享快照。
/// @param thread 目标 live Thread 的只读快照。
/// @return 包含尾随换行的 stat 文本。
pub(super) fn format_thread_stat(
    process: &ProcProcessSnapshot,
    thread: &ProcThreadSnapshot,
) -> String {
    format_task_stat(
        process,
        TaskStatFields {
            pid: thread.tid,
            state: thread.state,
            nice: thread.nice,
            priority: thread.priority,
            runtime_us: thread.runtime_us,
            start_time_us: thread.start_time_us,
            last_cpu: thread.last_cpu,
        },
    )
}

struct TaskStatFields {
    pid: usize,
    state: u8,
    nice: i32,
    priority: i32,
    runtime_us: u64,
    start_time_us: u64,
    last_cpu: usize,
}

fn format_task_stat(process: &ProcProcessSnapshot, task: TaskStatFields) -> String {
    let comm = String::from_utf8_lossy(&process.comm).replace(['(', ')', '\n'], "?");
    let virtual_size = process.virtual_pages.saturating_mul(4096);
    format!(
        "{} ({}) {} {} {} {} 0 0 0 0 0 0 0 {} 0 0 0 {} {} {} 0 {} {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {}\n",
        task.pid,
        comm,
        task.state as char,
        process.ppid,
        process.process_group,
        process.session,
        ticks(task.runtime_us),
        task.priority,
        task.nice,
        process.threads.len(),
        ticks(task.start_time_us),
        virtual_size,
        process.resident_pages,
        task.last_cpu
    )
}

/// @description 将 Process AddressSpace 快照编码为 Linux `/proc/<pid>/statm` 七字段格式。
/// @param process 目标 live Process 的只读快照。
/// @return `size resident shared text lib data dt` 页数与尾随换行。
pub(super) fn format_process_statm(process: &ProcProcessSnapshot) -> String {
    format!(
        "{} {} {} {} 0 {} 0\n",
        process.virtual_pages,
        process.resident_pages,
        process.shared_pages,
        process.text_pages,
        process.data_pages,
    )
}

/// @description 将 Process comm 编码为 Linux `/proc/<pid>/comm` 格式。
/// @param process 目标 live Process 的只读快照。
/// @return 清理内嵌换行并带尾随换行的 comm 文本。
pub(super) fn format_process_comm(process: &ProcProcessSnapshot) -> String {
    let comm = String::from_utf8_lossy(&process.comm).replace('\n', "?");
    format!("{comm}\n")
}

/// @description 将 Process owner 状态编码为已声明的 Linux status 字段。
/// @param process 目标 live Process 的只读快照。
/// @return 包含 identity、graph、fd 与 memory 字段的 status 文本。
pub(super) fn format_process_status(process: &ProcProcessSnapshot) -> String {
    format_task_status(process, process.pid, process.state)
}

/// @description 将 Thread snapshot 编码为 Linux `/proc/<tgid>/task/<tid>/status`。
/// @param process Thread 所属 Process 的共享快照。
/// @param thread 目标 live Thread 的只读快照。
/// @return 包含线程 identity、状态及共享 Process 字段的 status 文本。
pub(super) fn format_thread_status(
    process: &ProcProcessSnapshot,
    thread: &ProcThreadSnapshot,
) -> String {
    format_task_status(process, thread.tid, thread.state)
}

fn format_task_status(process: &ProcProcessSnapshot, pid: usize, state: u8) -> String {
    let comm = String::from_utf8_lossy(&process.comm).replace(['\t', '\n'], "?");
    let groups = process
        .groups
        .iter()
        .fold(String::new(), |mut output, group| {
            let _ = write!(output, "{group} ");
            output
        });
    let state_name = match state {
        b'R' => "running",
        b'T' => "stopped",
        _ => "sleeping",
    };
    format!(
        "Name:\t{comm}\nState:\t{} ({state_name})\nTgid:\t{}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nGroups:\t{groups}\nNStgid:\t{}\nNSpid:\t{}\nNSpgid:\t{}\nNSsid:\t{}\nThreads:\t{}\nVmSize:\t{} kB\nVmRSS:\t{} kB\n",
        state as char,
        process.pid,
        pid,
        process.ppid,
        process.uids[0],
        process.uids[1],
        process.uids[2],
        process.uids[1],
        process.gids[0],
        process.gids[1],
        process.gids[2],
        process.gids[1],
        process.fd_size,
        process.pid,
        pid,
        process.process_group,
        process.session,
        process.threads.len(),
        process.virtual_pages.saturating_mul(4),
        process.resident_pages.saturating_mul(4),
    )
}
