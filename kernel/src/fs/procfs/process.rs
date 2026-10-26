use alloc::vec::Vec;
use core::fmt::{self, Write};

use super::{
    FileSystemError, ProcCredentialSnapshot, ProcIoSnapshot, ProcProcessSnapshot, ProcText,
    ProcThreadSnapshot, proc_text, system::ticks,
};

struct Sanitized<'a> {
    bytes: &'a [u8],
    forbidden: &'a [u8],
}

impl fmt::Display for Sanitized<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &byte in self.bytes {
            formatter.write_char(if self.forbidden.contains(&byte) {
                '?'
            } else {
                char::from(byte)
            })?;
        }
        Ok(())
    }
}

/// 将 I/O owner 快照编码为 Linux `/proc/<task>/io` 七字段格式。
///
/// # Parameters
///
/// - `io`: Process 聚合或 Thread 私有 I/O counter 快照。
///
/// # Returns
///
/// 包含尾随换行的 io 文本；当前同步写路径不会产生 cancelled writes。
pub(super) fn format_io(io: &ProcIoSnapshot) -> Result<Vec<u8>, FileSystemError> {
    proc_text(format_args!(
        "rchar: {}\nwchar: {}\nsyscr: {}\nsyscw: {}\nread_bytes: {}\nwrite_bytes: {}\ncancelled_write_bytes: 0\n",
        io.read_characters,
        io.written_characters,
        io.read_syscalls,
        io.write_syscalls,
        io.read_bytes,
        io.write_bytes,
    ))
}

/// 将 Process snapshot 编码为 Linux `/proc/<pid>/stat` 单行格式。
///
/// # Parameters
///
/// - `process`: 目标 live Process 的只读快照。
///
/// # Returns
///
/// 包含尾随换行的 stat 文本。
pub(super) fn format_process_stat(
    process: &ProcProcessSnapshot,
    offsets: crate::namespace::TimeOffsets,
    pid_view: &crate::namespace::PidNamespace,
) -> Result<Vec<u8>, FileSystemError> {
    format_task_stat(
        process,
        TaskStatFields {
            comm: &process.comm,
            pid: process.pid.number_in(pid_view) as usize,
            has_mm: process.has_mm,
            state: process.state,
            nice: process.nice,
            priority: process.priority,
            runtime_us: process.runtime_us,
            start_time_us: process.start_time_us,
            last_cpu: process.last_cpu,
        },
        offsets,
        pid_view,
    )
}

/// 将 Thread snapshot 编码为 Linux `/proc/<tgid>/task/<tid>/stat` 单行格式。
///
/// # Parameters
///
/// - `process`: Thread 所属 Process 的共享快照。
/// - `thread`: 目标 live Thread 的只读快照。
///
/// # Returns
///
/// 包含尾随换行的 stat 文本。
pub(super) fn format_thread_stat(
    process: &ProcProcessSnapshot,
    thread: &ProcThreadSnapshot,
    offsets: crate::namespace::TimeOffsets,
    pid_view: &crate::namespace::PidNamespace,
) -> Result<Vec<u8>, FileSystemError> {
    format_task_stat(
        process,
        TaskStatFields {
            comm: thread.comm(),
            pid: thread.tid.number_in(pid_view) as usize,
            has_mm: thread.has_mm,
            state: thread.state,
            nice: thread.nice,
            priority: thread.priority,
            runtime_us: thread.runtime_us,
            start_time_us: thread.start_time_us,
            last_cpu: thread.last_cpu,
        },
        offsets,
        pid_view,
    )
}

struct TaskStatFields<'a> {
    comm: &'a [u8],
    pid: usize,
    has_mm: bool,
    state: u8,
    nice: i32,
    priority: i32,
    runtime_us: u64,
    start_time_us: u64,
    last_cpu: usize,
}

fn format_task_stat(
    process: &ProcProcessSnapshot,
    task: TaskStatFields<'_>,
    offsets: crate::namespace::TimeOffsets,
    pid_view: &crate::namespace::PidNamespace,
) -> Result<Vec<u8>, FileSystemError> {
    let comm = Sanitized {
        bytes: task.comm,
        forbidden: b"()\n",
    };
    let virtual_size = if task.has_mm {
        process.virtual_pages.saturating_mul(4096)
    } else {
        0
    };
    let resident_pages = if task.has_mm {
        process.resident_pages
    } else {
        0
    };
    proc_text(format_args!(
        "{} ({}) {} {} {} {} {} {} {} 0 0 0 0 {} 0 0 0 {} {} {} 0 {} {} {} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 {}\n",
        task.pid,
        comm,
        task.state as char,
        process
            .ppid
            .as_ref()
            .map_or(0, |parent| parent.number_in(pid_view)),
        process.process_group.number_in(pid_view),
        process.session.number_in(pid_view),
        process.tty_number,
        process.terminal_process_group,
        if process.kernel_thread {
            0x0020_0000u32
        } else {
            0
        },
        ticks(task.runtime_us),
        task.priority,
        task.nice,
        process.threads.len(),
        ticks(
            offsets.project(
                crate::namespace::TimeClock::Boottime,
                task.start_time_us.saturating_mul(1_000)
            ) / 1_000
        ),
        virtual_size,
        resident_pages,
        task.last_cpu
    ))
}

/// 将 Process AddressSpace 快照编码为 Linux `/proc/<pid>/statm` 七字段格式。
///
/// # Parameters
///
/// - `process`: 目标 live Process 的只读快照。
///
/// # Returns
///
/// `size resident shared text lib data dt` 页数与尾随换行。
pub(super) fn format_process_statm(
    process: &ProcProcessSnapshot,
) -> Result<Vec<u8>, FileSystemError> {
    format_task_statm(process, process.has_mm)
}

/// Thread statm 使用自身 mm presence；页数由同一 group common snapshot 投影。
pub(super) fn format_thread_statm(
    process: &ProcProcessSnapshot,
    thread: &ProcThreadSnapshot,
) -> Result<Vec<u8>, FileSystemError> {
    format_task_statm(process, thread.has_mm)
}

fn format_task_statm(
    process: &ProcProcessSnapshot,
    has_mm: bool,
) -> Result<Vec<u8>, FileSystemError> {
    if !has_mm {
        return proc_text(format_args!("0 0 0 0 0 0 0\n"));
    }
    proc_text(format_args!(
        "{} {} {} {} 0 {} 0\n",
        process.virtual_pages,
        process.resident_pages,
        process.shared_pages,
        process.text_pages,
        process.data_pages,
    ))
}

/// 将 Process comm 编码为 Linux `/proc/<pid>/comm` 格式。
///
/// # Parameters
///
/// - `process`: 目标 live Process 的只读快照。
///
/// # Returns
///
/// 清理内嵌换行并带尾随换行的 comm 文本。
pub(super) fn format_process_comm(
    process: &ProcProcessSnapshot,
) -> Result<Vec<u8>, FileSystemError> {
    proc_text(format_args!(
        "{}\n",
        Sanitized {
            bytes: &process.comm,
            forbidden: b"\n",
        }
    ))
}

/// 将目标 Thread 的独立 comm 编码为带尾随换行的文本；分配错误原样返回。
pub(super) fn format_thread_comm(thread: &ProcThreadSnapshot) -> Result<Vec<u8>, FileSystemError> {
    proc_text(format_args!(
        "{}\n",
        Sanitized {
            bytes: thread.comm(),
            forbidden: b"\n"
        }
    ))
}

/// 将 Process owner 状态编码为已声明的 Linux status 字段。
///
/// # Parameters
///
/// - `process`: 目标 live Process 的只读快照。
///
/// # Returns
///
/// 包含 identity、graph、fd 与 memory 字段的 status 文本。
pub(super) fn format_process_status(
    process: &ProcProcessSnapshot,
    viewer: &crate::namespace::UserNamespace,
    pid_view: &crate::namespace::PidNamespace,
) -> Result<Vec<u8>, FileSystemError> {
    format_task_status(
        process,
        TaskStatusFields {
            comm: &process.comm,
            pid: &process.pid,
            state: process.state,
            credentials: &process.credentials,
            has_mm: process.has_mm,
            fd_size: process.fd_size,
        },
        viewer,
        pid_view,
    )
}

/// 将 Thread snapshot 编码为 Linux `/proc/<tgid>/task/<tid>/status`。
///
/// # Parameters
///
/// - `process`: Thread 所属 Process 的共享快照。
/// - `thread`: 目标 live Thread 的只读快照。
///
/// # Returns
///
/// 包含线程 identity、状态及共享 Process 字段的 status 文本。
pub(super) fn format_thread_status(
    process: &ProcProcessSnapshot,
    thread: &ProcThreadSnapshot,
    viewer: &crate::namespace::UserNamespace,
    pid_view: &crate::namespace::PidNamespace,
) -> Result<Vec<u8>, FileSystemError> {
    format_task_status(
        process,
        TaskStatusFields {
            comm: thread.comm(),
            pid: &thread.tid,
            state: thread.state,
            credentials: &thread.credentials,
            has_mm: thread.has_mm,
            fd_size: thread.fd_size,
        },
        viewer,
        pid_view,
    )
}

struct TaskStatusFields<'a> {
    comm: &'a [u8],
    pid: &'a crate::namespace::PidIdentity,
    state: u8,
    credentials: &'a ProcCredentialSnapshot,
    has_mm: bool,
    fd_size: usize,
}
fn format_task_status(
    process: &ProcProcessSnapshot,
    task: TaskStatusFields<'_>,
    viewer: &crate::namespace::UserNamespace,
    pid_view: &crate::namespace::PidNamespace,
) -> Result<Vec<u8>, FileSystemError> {
    let TaskStatusFields {
        comm,
        pid,
        state,
        credentials,
        has_mm,
        fd_size,
    } = task;
    let comm = Sanitized {
        bytes: comm,
        forbidden: b"\t\n",
    };
    let state_name = match state {
        b'R' => "running",
        b'T' => "stopped",
        b'Z' => "zombie",
        b'D' => "disk sleep",
        b'I' => "idle",
        _ => "sleeping",
    };
    let mut output = ProcText::new();
    write!(
        output,
        "Name:\t{comm}\nState:\t{} ({state_name})\nTgid:\t{}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nFDSize:\t{}\nGroups:\t",
        state as char,
        process.pid.number_in(pid_view),
        pid.number_in(pid_view),
        process.ppid.as_ref().map_or(0, |parent| parent.number_in(pid_view)),
        viewer.display_id(true, credentials.uids[0]),
        viewer.display_id(true, credentials.uids[1]),
        viewer.display_id(true, credentials.uids[2]),
        viewer.display_id(true, credentials.fsids[0]),
        viewer.display_id(false, credentials.gids[0]),
        viewer.display_id(false, credentials.gids[1]),
        viewer.display_id(false, credentials.gids[2]),
        viewer.display_id(false, credentials.fsids[1]),
        fd_size,
    )
    .map_err(|_| FileSystemError::OutOfMemory)?;
    for group in credentials.groups.as_deref().map_or(&[][..], Vec::as_slice) {
        let group = viewer.display_id(false, *group);
        write!(output, "{group} ").map_err(|_| FileSystemError::OutOfMemory)?;
    }
    for (label, identity) in [
        ("NStgid", process.pid.as_ref()),
        ("NSpid", pid),
        ("NSpgid", process.process_group.as_ref()),
        ("NSsid", process.session.as_ref()),
    ] {
        write!(output, "\n{label}:").map_err(|_| FileSystemError::OutOfMemory)?;
        for number in identity.numbers_in(pid_view) {
            write!(output, "\t{number}").map_err(|_| FileSystemError::OutOfMemory)?;
        }
    }
    writeln!(output, "\nThreads:\t{}", process.threads.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let capabilities = credentials.capabilities;
    write!(output, "CapInh:\t{:016x}\nCapPrm:\t{:016x}\nCapEff:\t{:016x}\nCapBnd:\t{:016x}\nCapAmb:\t{:016x}\n", capabilities.sets.inheritable, capabilities.sets.permitted, capabilities.sets.effective, capabilities.bounding, capabilities.ambient).map_err(|_| FileSystemError::OutOfMemory)?;
    writeln!(output, "Kthread:\t{}", u8::from(process.kernel_thread))
        .map_err(|_| FileSystemError::OutOfMemory)?;
    if has_mm {
        write!(
            output,
            "VmSize:\t{} kB\nVmRSS:\t{} kB\nVmSwap:\t{} kB\n",
            process.virtual_pages.saturating_mul(4),
            process.resident_pages.saturating_mul(4),
            process.swapped_pages.saturating_mul(4),
        )
        .map_err(|_| FileSystemError::OutOfMemory)?;
    }
    Ok(output.finish())
}
