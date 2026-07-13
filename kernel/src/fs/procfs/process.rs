use alloc::{format, string::String};
use core::fmt::Write;

use super::{ProcProcessSnapshot, ticks};

/// @description 将 Process snapshot 编码为 Linux `/proc/<pid>/stat` 单行格式。
/// @param process 目标 live Process 的只读快照。
/// @return 包含尾随换行的 stat 文本。
pub(super) fn format_process_stat(process: &ProcProcessSnapshot) -> String {
    let comm = String::from_utf8_lossy(&process.comm).replace(['(', ')', '\n'], "?");
    let virtual_size = process.virtual_pages.saturating_mul(4096);
    format!(
        "{} ({}) {} {} {} {} 0 0 0 0 0 0 0 {} 0 0 0 {} {} {} 0 {} {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {}\n",
        process.pid,
        comm,
        process.state as char,
        process.ppid,
        process.process_group,
        process.session,
        ticks(process.runtime_us),
        process.priority,
        process.nice,
        process.threads,
        ticks(process.start_time_us),
        virtual_size,
        process.resident_pages,
        process.last_cpu
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
    let comm = String::from_utf8_lossy(&process.comm).replace(['\t', '\n'], "?");
    let groups = process
        .groups
        .iter()
        .fold(String::new(), |mut output, group| {
            let _ = write!(output, "{group} ");
            output
        });
    let state_name = match process.state {
        b'R' => "running",
        b'T' => "stopped",
        _ => "sleeping",
    };
    format!(
        "Name:\t{comm}\nState:\t{} ({state_name})\nTgid:\t{}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nGroups:\t{groups}\nNStgid:\t{}\nNSpid:\t{}\nNSpgid:\t{}\nNSsid:\t{}\nThreads:\t{}\nVmSize:\t{} kB\nVmRSS:\t{} kB\n",
        process.state as char,
        process.pid,
        process.pid,
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
        process.pid,
        process.process_group,
        process.session,
        process.threads,
        process.virtual_pages.saturating_mul(4),
        process.resident_pages.saturating_mul(4),
    )
}
