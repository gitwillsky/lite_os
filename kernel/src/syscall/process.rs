use alloc::vec::Vec;

use crate::{
    fs::FileSystemError,
    memory::{ElfLoadError, UserAccessError},
    syscall::errno,
    task::{
        EXEC_ARGUMENT_BYTES_LIMIT, ProcessCloneError, ProcessCloneResources, ProcessGroupError,
        ProgramLoadError, SetProcessGroupError, TaskControlBlock, ThreadCloneError, WaitChildError,
        child_status_pid, clone_current_process, clone_current_thread, consume_child_status,
        create_session, current_task, exit_current_group, exit_current_thread, load_executable,
        parent_pid, process_group, session_id, set_process_group, wait_child,
    },
};

use super::INTERNAL_RESTART_SYS;
use super::clone_errno::{
    clone_resource_errno, process_clone_memory_errno, thread_clone_memory_errno,
};

const MAX_PATH_BYTES: usize = 4096;
const MAX_ARG_STRING_BYTES: usize = 32 * 4096;

/// 按 Linux `exit` 语义终止 calling Thread。
///
/// # Parameters
///
/// - `exit_code`: 用户态退出状态。
///
/// # Returns
///
/// 此函数不返回。
pub(crate) fn sys_exit(exit_code: i32) -> ! {
    exit_current_thread(exit_code)
}

/// 按 Linux `exit_group` 语义终止 calling Thread 所属 Process。
///
/// # Parameters
///
/// - `exit_code`: 用户态退出状态，kernel 只保留低 8 bit。
///
/// # Returns
///
/// 此函数不返回。
pub(crate) fn sys_exit_group(exit_code: i32) -> ! {
    exit_current_group(exit_code)
}

/// 返回当前进程标识。
///
/// # Returns
///
/// 当前任务的 PID。
pub(crate) fn sys_get_pid() -> isize {
    current_task()
        .expect("getpid requires a current task")
        .visible_tgid() as isize
}

/// 返回当前进程的父进程标识。
///
/// # Returns
///
/// process graph 中的 parent TGID；init 返回零。
pub(crate) fn sys_get_ppid() -> isize {
    let task = current_task().expect("getppid requires a current task");
    parent_pid(task.tgid()) as isize
}

/// 返回当前线程标识；单线程模型中与 PID 相同。
///
/// # Returns
///
/// 当前任务的 TID。
pub(crate) fn sys_get_tid() -> isize {
    current_task()
        .expect("gettid requires a current task")
        .visible_tid() as isize
}

pub(super) fn process_group_error(error: ProcessGroupError) -> isize {
    match error {
        ProcessGroupError::OutOfMemory => -errno::ENOMEM,
        ProcessGroupError::NotFound => -errno::ESRCH,
        ProcessGroupError::Permission => -errno::EPERM,
        ProcessGroupError::NotTerminal => -errno::ENOTTY,
    }
}

fn set_process_group_error(error: SetProcessGroupError) -> isize {
    match error {
        SetProcessGroupError::OutOfMemory => -errno::ENOMEM,
        SetProcessGroupError::NotFound => -errno::ESRCH,
        SetProcessGroupError::Permission => -errno::EPERM,
        SetProcessGroupError::Executed => -errno::EACCES,
    }
}

/// 修改 caller 或其直接 child 的 process group membership。
///
/// # Parameters
///
/// - `pid`: 零表示 caller，否则为 direct child TGID。
/// - `pgid`: 零表示目标 TGID，否则为同 session process group。
///
/// # Returns
///
/// 成功返回零；目标/权限错误返回负 errno。
pub(crate) fn sys_setpgid(pid: i32, pgid: i32) -> isize {
    if pid < 0 || pgid < 0 {
        return -errno::EINVAL;
    }
    set_process_group(pid as usize, pgid as usize).map_or_else(set_process_group_error, |()| 0)
}

/// 查询 live/zombie Process 的 process group ID。
///
/// # Parameters
///
/// - `pid`: 零表示 caller。
///
/// # Returns
///
/// PGID 或负 errno。
pub(crate) fn sys_getpgid(pid: i32) -> isize {
    if pid < 0 {
        return -errno::ESRCH;
    }
    process_group(pid as usize).map_or_else(process_group_error, |value| value as isize)
}

/// 查询 live/zombie Process 的 session ID。
///
/// # Parameters
///
/// - `pid`: 零表示 caller。
///
/// # Returns
///
/// SID 或负 errno。
pub(crate) fn sys_getsid(pid: i32) -> isize {
    if pid < 0 {
        return -errno::ESRCH;
    }
    session_id(pid as usize).map_or_else(process_group_error, |value| value as isize)
}

/// 创建以 caller TGID 命名的新 session 与 process group。
///
/// # Returns
///
/// 新 SID；caller 已是 process-group leader 时返回 `EPERM`。
pub(crate) fn sys_setsid() -> isize {
    create_session().map_or_else(process_group_error, |value| value as isize)
}

/// Linux legacy clone 的 register 参数只在此解码；与 clone3 共用同一资源准备和 graph publication。
pub(crate) fn sys_clone(
    flags: usize,
    stack: usize,
    parent_tid: usize,
    tls: usize,
    child_tid: usize,
) -> isize {
    use syscall_abi::clone::*;
    if flags as u64 & (PIDFD | DETACHED) == (PIDFD | DETACHED) {
        return -errno::EINVAL;
    }
    create_clone(
        (flags as u64 & !0xff) & !DETACHED,
        (flags & 0xff) as u64,
        crate::task::CloneParameters {
            stack,
            tls: (flags as u64 & SETTLS != 0).then_some(tls),
            parent_set_tid: (flags as u64 & PARENT_SETTID != 0).then_some(parent_tid),
            pidfd: (flags as u64 & PIDFD != 0).then_some(parent_tid),
            child_set_tid: (flags as u64 & CHILD_SETTID != 0).then_some(child_tid),
            clear_child_tid: (flags as u64 & CHILD_CLEARTID != 0).then_some(child_tid),
            requested_pids: Vec::new(),
        },
    )
}

/// Linux clone3 的尺寸兼容、未知尾部、stack range 与 set_tid user-copy，均先于 domain publication。
///
/// # Errors
///
/// 短于 VER0 为 EINVAL、超过一页或非零未知尾部为 E2BIG、无效用户地址为 EFAULT；
/// resource/namespace/PID 失败沿唯一 clone transaction 返回其实际 errno。
pub(crate) fn sys_clone3(address: usize, size: usize) -> isize {
    use syscall_abi::clone::*;
    if size > 4096 {
        return -errno::E2BIG;
    }
    if size < SIZE_VER0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("clone3 requires current Thread");
    let mut bytes = [0; SIZE_VER2];
    let prefix = size.min(SIZE_VER2);
    if task.copy_from_user(address, &mut bytes[..prefix]).is_err() {
        return -errno::EFAULT;
    }
    // 固定小块验证 future struct 尾部；无需按不可信 size 分配，也不忽略任何未知非零字段。
    let mut tail = [0; 64];
    let mut position = prefix;
    while position < size {
        let length = (size - position).min(tail.len());
        let Some(pointer) = address.checked_add(position) else {
            return -errno::EFAULT;
        };
        if task.copy_from_user(pointer, &mut tail[..length]).is_err() {
            return -errno::EFAULT;
        }
        if tail[..length].iter().any(|&byte| byte != 0) {
            return -errno::E2BIG;
        }
        position += length;
    }
    let arguments = CloneArgs::decode(&bytes).expect("fixed clone3 prefix");
    if arguments.exit_signal > 0xff
        || arguments.flags & (THREAD | PARENT) != 0 && arguments.exit_signal != 0
        || arguments.flags & (DETACHED | (0xff & !NEWTIME)) != 0
        || arguments.set_tid_size > MAX_SET_TID as u64
        || (arguments.set_tid == 0) != (arguments.set_tid_size == 0)
        || arguments.flags & INTO_CGROUP != 0
    {
        return -errno::EINVAL;
    }
    let stack = if arguments.stack == 0 {
        if arguments.stack_size != 0 {
            return -errno::EINVAL;
        }
        0
    } else {
        let Some(top) = arguments.stack.checked_add(arguments.stack_size) else {
            return -errno::EINVAL;
        };
        if arguments.stack_size == 0 || top > crate::memory::USER_ADDRESS_END as u64 {
            return -errno::EINVAL;
        }
        top as usize
    };
    let mut requested_pids = Vec::new();
    if arguments.set_tid_size != 0 {
        let count = arguments.set_tid_size as usize;
        if requested_pids.try_reserve_exact(count).is_err() {
            return -errno::ENOMEM;
        }
        let mut numbers = [0; MAX_SET_TID * 4];
        if task
            .copy_from_user(arguments.set_tid as usize, &mut numbers[..count * 4])
            .is_err()
        {
            return -errno::EFAULT;
        }
        requested_pids.extend(
            numbers[..count * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| i32::from_ne_bytes(*word)),
        );
    }
    create_clone(
        arguments.flags,
        arguments.exit_signal,
        crate::task::CloneParameters {
            stack,
            tls: (arguments.flags & SETTLS != 0).then_some(arguments.tls as usize),
            parent_set_tid: (arguments.flags & PARENT_SETTID != 0)
                .then_some(arguments.parent_tid as usize),
            pidfd: (arguments.flags & PIDFD != 0).then_some(arguments.pidfd as usize),
            child_set_tid: (arguments.flags & CHILD_SETTID != 0)
                .then_some(arguments.child_tid as usize),
            clear_child_tid: (arguments.flags & CHILD_CLEARTID != 0)
                .then_some(arguments.child_tid as usize),
            requested_pids,
        },
    )
}

fn create_clone(flags: u64, exit_signal: u64, parameters: crate::task::CloneParameters) -> isize {
    use syscall_abi::clone::*;
    const SUPPORTED: u64 = VM
        | FS
        | FILES
        | SIGHAND
        | VFORK
        | PIDFD
        | PARENT
        | THREAD
        | SYSVSEM
        | SETTLS
        | PARENT_SETTID
        | CHILD_SETTID
        | CHILD_CLEARTID
        | NEWNS
        | NEWUTS
        | NEWUSER
        | NEWPID
        | NEWIPC
        | NEWNET
        | NEWTIME
        | CLEAR_SIGHAND;
    if flags & !SUPPORTED != 0
        || parameters.stack & 15 != 0
        || flags & SIGHAND != 0 && flags & VM == 0
        || flags & SIGHAND != 0 && flags & CLEAR_SIGHAND != 0
        || flags & THREAD != 0 && flags & (VM | SIGHAND) != (VM | SIGHAND)
        || flags & NEWIPC != 0 && flags & SYSVSEM != 0
        || flags & (NEWUSER | NEWNS) != 0 && flags & FS != 0
        || flags & THREAD != 0 && flags & (NEWUSER | NEWPID | CLEAR_SIGHAND) != 0
        || exit_signal > 64
        || parameters.pidfd.is_some() && parameters.pidfd == parameters.parent_set_tid
    {
        return -errno::EINVAL;
    }
    let task = current_task().expect("clone requires current Thread");
    if flags & PARENT != 0 && task.visible_tgid() == 1 {
        return -errno::EINVAL;
    }
    let namespaces = match task.prepare_clone_namespaces(
        flags & NEWUSER != 0,
        flags & NEWUTS != 0,
        flags & NEWNS != 0,
        flags & NEWTIME != 0,
        flags & NEWPID != 0,
        flags & NEWIPC != 0,
        flags & NEWNET != 0,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return super::process_context::namespace_error(error),
    };
    if flags & THREAD != 0 {
        return match clone_current_thread(
            parameters,
            crate::task::ThreadCloneResources {
                share_fs: flags & FS != 0,
                share_files: flags & FILES != 0,
                share_sem_undo: flags & SYSVSEM != 0,
                namespaces,
            },
        ) {
            Ok(tid) => tid as isize,
            Err(ThreadCloneError::ResourceLimit) => clone_resource_errno(),
            Err(ThreadCloneError::Pid(error)) => pid_allocation_errno(error),
            Err(ThreadCloneError::PidFd(error)) => super::pidfd::error(error),
            Err(ThreadCloneError::Memory(error)) => {
                thread_clone_memory_errno(error.is_out_of_memory())
            }
        };
    }
    match clone_current_process(
        parameters,
        ProcessCloneResources {
            inherit_parent: flags & PARENT != 0,
            exit_signal: exit_signal as u8,
            share_memory: flags & VM != 0,
            share_fs: flags & FS != 0,
            share_files: flags & FILES != 0,
            share_sem_undo: flags & SYSVSEM != 0,
            share_handlers: flags & SIGHAND != 0,
            clear_handlers: flags & CLEAR_SIGHAND != 0,
            wait_for_child: flags & VFORK != 0,
            namespaces,
        },
    ) {
        Ok(pid) => pid as isize,
        Err(ProcessCloneError::Memory(error)) => {
            process_clone_memory_errno(error.is_out_of_memory())
        }
        Err(ProcessCloneError::ResourceLimit) => clone_resource_errno(),
        Err(ProcessCloneError::Pid(error)) => pid_allocation_errno(error),
        Err(ProcessCloneError::PidFd(error)) => super::pidfd::error(error),
    }
}

/// 设置 calling Thread 的 clear-child-tid 地址。
///
/// # Parameters
///
/// - `address`: 零表示清除，否则 thread exit 时写零并 futex wake。
///
/// # Returns
///
/// calling TID。
pub(crate) fn sys_set_tid_address(address: usize) -> isize {
    current_task()
        .expect("set_tid_address requires current task")
        .set_clear_child_tid(address) as isize
}

/// 注册 calling Thread 的 Linux robust-list head。
///
/// # Parameters
///
/// - `head`: 用户 robust_list_head 地址；零且 length 正确时注销。
/// - `length`: LP64 必须为 24 bytes。
///
/// # Returns
///
/// 成功返回零，形状错误返回 `EINVAL`。
pub(crate) fn sys_set_robust_list(head: usize, length: usize) -> isize {
    current_task()
        .expect("set_robust_list requires current task")
        .set_robust_list(head, length)
        .map_or(-errno::EINVAL, |()| 0)
}

/// 按 native REALCREDS/exec 序列查询 registration；len copyout 先于 head copyout。
pub(crate) fn sys_get_robust_list(pid: i32, head_pointer: usize, length_pointer: usize) -> isize {
    let task = current_task().expect("get_robust_list requires current task");
    let head = match crate::task::robust_list_head(pid, &task) {
        Ok(head) => head,
        Err(error) => {
            return -match error {
                crate::task::RobustListError::NotFound => errno::ESRCH,
                crate::task::RobustListError::Permission => errno::EPERM,
                crate::task::RobustListError::OutOfMemory => errno::ENOMEM,
                crate::task::RobustListError::Interrupted => errno::EINTR,
            };
        }
    };
    let length = 3 * core::mem::size_of::<usize>();
    if task
        .copy_to_user(length_pointer, &length.to_ne_bytes())
        .is_err()
        || task
            .copy_to_user(head_pointer, &head.to_ne_bytes())
            .is_err()
    {
        return -errno::EFAULT;
    }
    0
}

/// 等待并消费直接 child 的最小 exit record。
///
/// # Parameters
///
/// - `pid`: `-1` 表示任一 child，正数表示指定 child。
/// - `status`: 可为空；非空时写入 Linux wait status word。
/// - `options`: native WNOHANG/WUNTRACED/WCONTINUED 与 __WCLONE/__WALL/__WNOTHREAD。
/// - `rusage`: 可为空；非空时写入 child（含其已回收后代）的资源使用。
///
/// # Returns
///
/// child PID、WNOHANG 的零，或负 Linux errno。
pub(crate) fn sys_wait4(pid: isize, status: *mut i32, options: u32, rusage: *mut u8) -> isize {
    let options = options as usize;
    const WNOHANG: usize = 1;
    const WUNTRACED: usize = 2;
    const WCONTINUED: usize = 8;
    const WNOTHREAD: usize = 0x2000_0000;
    const WALL: usize = 0x4000_0000;
    const WCLONE: usize = 0x8000_0000;
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WNOTHREAD | WALL | WCLONE) != 0 {
        return -errno::EINVAL;
    }
    let record = match wait_child(
        pid,
        options & WNOHANG != 0,
        options & WUNTRACED != 0,
        options & WCONTINUED != 0,
        crate::task::ChildWaitFilter {
            class: if options & WALL != 0 {
                crate::task::ChildWaitClass::All
            } else if options & WCLONE != 0 {
                crate::task::ChildWaitClass::Clone
            } else {
                crate::task::ChildWaitClass::Ordinary
            },
            creator_only: options & WNOTHREAD != 0,
            ptrace_stops: true,
        },
    ) {
        Ok(Some(record)) => record,
        Ok(None) => return 0,
        Err(WaitChildError::NoChild) => return -errno::ECHILD,
        Err(WaitChildError::InvalidSelector) => return -errno::EINVAL,
        Err(WaitChildError::Interrupted) => return INTERNAL_RESTART_SYS,
        Err(WaitChildError::OutOfMemory) => return -errno::ENOMEM,
    };
    let visible_pid = child_status_pid(&record);
    // native kernel_wait4 先消费退出/作业事件，再返回 user-copy；EFAULT 不恢复已回收的 child。
    let wait_status = record.status;
    // 回收后 child 的 Process 不再可达，所以必须先读取 rusage。
    let child_usage = crate::task::child_resource_usage(&record);
    consume_child_status(record);
    let task = current_task().expect("wait4 copyout requires current task");
    if !status.is_null()
        && task
            .copy_to_user(status as usize, &wait_status.to_ne_bytes())
            .is_err()
    {
        return -errno::EFAULT;
    }
    if !rusage.is_null()
        && task
            .copy_to_user(
                rusage as usize,
                &super::resource_usage::encode_rusage(&child_usage),
            )
            .is_err()
    {
        return -errno::EFAULT;
    }
    visible_pid as isize
}

/// 标准 waitid 的 PID/PIDFD 选择、event mask 与 siginfo codec；所有路径复用实际 child claim。
/// WNOWAIT 释放 claim 而保留事件；正常消费先于 copyout，pidfd NONBLOCK 无事件返回 EAGAIN。
pub(crate) fn sys_waitid(
    which: i32,
    id: i32,
    information: usize,
    options: u32,
    rusage: usize,
) -> isize {
    const NOHANG: u32 = 1;
    const STOPPED: u32 = 2;
    const EXITED: u32 = 4;
    const CONTINUED: u32 = 8;
    const NOWAIT: u32 = 0x0100_0000;
    const NOTHREAD: u32 = 0x2000_0000;
    const ALL: u32 = 0x4000_0000;
    const CLONE: u32 = 0x8000_0000;
    if options & !(NOHANG | STOPPED | EXITED | CONTINUED | NOWAIT | NOTHREAD | ALL | CLONE) != 0
        || options & (STOPPED | EXITED | CONTINUED) == 0
    {
        return -errno::EINVAL;
    }
    let task = current_task().expect("waitid requires current Thread");
    use crate::task::ChildWaitSelector;
    let mut nonblocking = false;
    let selector = match which {
        0 => ChildWaitSelector::All,
        1 if id > 0 => ChildWaitSelector::Process(id as usize),
        2 if id >= 0 => ChildWaitSelector::Group(id as usize),
        3 if id >= 0 => {
            let Some(file) = task.fd_get(id as usize) else {
                return -errno::EBADF;
            };
            let identity = match &file.kind {
                crate::file::OpenFileKind::Device(backend) => backend.pid_reference().cloned(),
                _ => None,
            };
            let Some(identity) = identity else {
                return -errno::EBADF;
            };
            nonblocking = file.status_flags() & crate::fs::O_NONBLOCK != 0;
            ChildWaitSelector::Pid(identity)
        }
        _ => return -errno::EINVAL,
    };
    let record = crate::task::wait_child_events(
        selector,
        nonblocking || options & NOHANG != 0,
        options & EXITED != 0,
        options & STOPPED != 0,
        options & CONTINUED != 0,
        crate::task::ChildWaitFilter {
            class: if options & ALL != 0 {
                crate::task::ChildWaitClass::All
            } else if options & CLONE != 0 {
                crate::task::ChildWaitClass::Clone
            } else {
                crate::task::ChildWaitClass::Ordinary
            },
            creator_only: options & NOTHREAD != 0,
            ptrace_stops: options & STOPPED != 0,
        },
    );
    let mut info = [0u8; 28];
    let mut child_usage = crate::task::ResourceUsage::default();
    let result = match record {
        Ok(Some(record)) => {
            child_usage = crate::task::child_resource_usage(&record);
            let (pid, uid, code, status) = crate::task::child_status_info(&record);
            info[0..4].copy_from_slice(&(syscall_abi::signal::SIGCHLD as i32).to_ne_bytes());
            info[8..12].copy_from_slice(&code.to_ne_bytes());
            info[16..20].copy_from_slice(&pid.to_ne_bytes());
            info[20..24].copy_from_slice(&uid.to_ne_bytes());
            info[24..28].copy_from_slice(&status.to_ne_bytes());
            if options & NOWAIT != 0 {
                crate::task::release_child_status(record);
            } else {
                consume_child_status(record);
            }
            0
        }
        Ok(None) => {
            if nonblocking && options & NOHANG == 0 {
                -errno::EAGAIN
            } else {
                0
            }
        }
        Err(WaitChildError::NoChild) => -errno::ECHILD,
        Err(WaitChildError::InvalidSelector) => -errno::EINVAL,
        Err(WaitChildError::Interrupted) => INTERNAL_RESTART_SYS,
        Err(WaitChildError::OutOfMemory) => -errno::ENOMEM,
    };
    // native waitid 也在无事件/错误时清理这六个字段；不覆盖未定义 union/LP64 padding。
    if information != 0
        && (task.validate_user_write(information, 128).is_err()
            || task.copy_to_user(information, &info[..12]).is_err()
            || task.copy_to_user(information + 16, &info[16..]).is_err())
    {
        return -errno::EFAULT;
    }
    // Linux 在事件处理成功后才写 rusage；没有事件时写入全零。
    if result == 0
        && rusage != 0
        && task
            .copy_to_user(rusage, &super::resource_usage::encode_rusage(&child_usage))
            .is_err()
    {
        return -errno::EFAULT;
    }
    result
}

/// 用新的 native ET_EXEC 或动态 PIE 映像、参数和环境替换当前进程。
///
/// # Parameters
///
/// - `path`: NUL 结尾的可执行文件路径字节。
/// - `argv`: NUL 结尾的参数指针数组。
/// - `envp`: NUL 结尾的环境指针数组。
///
/// # Returns
///
/// 新上下文准备完成时返回零，失败返回负 errno。
pub(crate) fn sys_execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match copy_user_c_string(&task, path, MAX_PATH_BYTES, errno::ENAMETOOLONG) {
        Ok(path) if !path.is_empty() => path,
        Ok(_) => return -errno::ENOENT,
        Err(error) => return error,
    };
    let mut argument_bytes = 0;
    let argv = match copy_user_string_array(&task, argv, &mut argument_bytes) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    let envp = match copy_user_string_array(&task, envp, &mut argument_bytes) {
        Ok(envp) => envp,
        Err(error) => return error,
    };

    let loaded = match load_executable(
        task.lookup_origin(),
        &task.mount_namespace(),
        path,
        argv,
        argument_bytes,
        &task.access_identity(true),
    ) {
        Ok(loaded) => loaded,
        Err(error) => return program_load_errno(error),
    };
    match task.execve_replace(&loaded, &envp) {
        Ok(()) => 0,
        Err(crate::task::ExecError::Image(ElfLoadError::OutOfMemory)) => -errno::ENOMEM,
        Err(crate::task::ExecError::Image(ElfLoadError::InvalidElf)) => -errno::ENOEXEC,
        Err(crate::task::ExecError::Image(ElfLoadError::Io)) => -errno::EIO,
        Err(crate::task::ExecError::Busy) => -errno::EAGAIN,
        Err(crate::task::ExecError::Permission) => -errno::EPERM,
        Err(crate::task::ExecError::Interrupted) => INTERNAL_RESTART_SYS,
        Err(crate::task::ExecError::FatalInterrupted) => {
            drop(loaded);
            drop(envp);
            drop(task);
            crate::task::exit_current_group_by_signal(syscall_abi::signal::SIGKILL)
        }
        Err(crate::task::ExecError::FatalImage) => {
            drop(loaded);
            drop(envp);
            drop(task);
            crate::task::exit_current_after_exec_failure()
        }
    }
}

fn copy_user_c_string(
    task: &TaskControlBlock,
    pointer: *const u8,
    max_bytes: usize,
    too_long_errno: isize,
) -> Result<Vec<u8>, isize> {
    if pointer.is_null() {
        return Err(-errno::EFAULT);
    }
    match task.copy_user_c_string(pointer as usize, max_bytes) {
        Ok(value) => Ok(value),
        Err(UserAccessError::Unterminated) => Err(-too_long_errno),
        Err(UserAccessError::OutOfMemory) => Err(-errno::ENOMEM),
        Err(UserAccessError::Fault | UserAccessError::Overflow) => Err(-errno::EFAULT),
    }
}

fn copy_user_string_array(
    task: &TaskControlBlock,
    array: *const *const u8,
    total_bytes: &mut usize,
) -> Result<Vec<Vec<u8>>, isize> {
    if array.is_null() {
        *total_bytes = total_bytes
            .checked_add(core::mem::size_of::<usize>())
            .filter(|bytes| *bytes <= EXEC_ARGUMENT_BYTES_LIMIT)
            .ok_or(-errno::E2BIG)?;
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    for index in 0usize.. {
        *total_bytes = total_bytes
            .checked_add(core::mem::size_of::<usize>())
            .ok_or(-errno::E2BIG)?;
        if *total_bytes > EXEC_ARGUMENT_BYTES_LIMIT {
            return Err(-errno::E2BIG);
        }
        let pointer_offset = index
            .checked_mul(core::mem::size_of::<usize>())
            .ok_or(-errno::EFAULT)?;
        let pointer_address = (array as usize)
            .checked_add(pointer_offset)
            .ok_or(-errno::EFAULT)?;
        let mut pointer_bytes = [0u8; core::mem::size_of::<usize>()];
        if task
            .copy_from_user(pointer_address, &mut pointer_bytes)
            .is_err()
        {
            return Err(-errno::EFAULT);
        }
        let pointer = usize::from_ne_bytes(pointer_bytes);
        if pointer == 0 {
            return Ok(values);
        }

        let value = copy_user_c_string(
            task,
            pointer as *const u8,
            MAX_ARG_STRING_BYTES,
            errno::E2BIG,
        )?;
        *total_bytes = total_bytes
            .checked_add(value.len())
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or(-errno::E2BIG)?;
        if *total_bytes > EXEC_ARGUMENT_BYTES_LIMIT {
            return Err(-errno::E2BIG);
        }
        values.try_reserve(1).map_err(|_| -errno::ENOMEM)?;
        values.push(value);
    }
    unreachable!("unbounded range only exits through an explicit return")
}

fn program_load_errno(error: ProgramLoadError) -> isize {
    let errno = match error {
        ProgramLoadError::OutOfMemory => errno::ENOMEM,
        ProgramLoadError::InvalidExecutable => errno::ENOEXEC,
        ProgramLoadError::InterpreterLoop => errno::ELOOP,
        ProgramLoadError::ArgumentListTooLong => errno::E2BIG,
        ProgramLoadError::NotRegularFile | ProgramLoadError::NotExecutable => errno::EACCES,
        ProgramLoadError::FileSystem(FileSystemError::NotFound) => errno::ENOENT,
        ProgramLoadError::FileSystem(FileSystemError::NotDirectory) => errno::ENOTDIR,
        ProgramLoadError::FileSystem(FileSystemError::NameTooLong) => errno::ENAMETOOLONG,
        ProgramLoadError::FileSystem(FileSystemError::Loop) => errno::ELOOP,
        ProgramLoadError::FileSystem(FileSystemError::ReadOnly) => errno::EACCES,
        ProgramLoadError::FileSystem(FileSystemError::TextBusy) => errno::ETXTBSY,
        ProgramLoadError::FileSystem(FileSystemError::OutOfMemory) => errno::ENOMEM,
        ProgramLoadError::FileSystem(
            FileSystemError::AlreadyExists
            | FileSystemError::IsDirectory
            | FileSystemError::DirectoryNotEmpty
            | FileSystemError::InvalidPath
            | FileSystemError::InvalidOperation
            | FileSystemError::CrossDevice
            | FileSystemError::PermissionDenied
            | FileSystemError::AccessDenied
            | FileSystemError::Busy
            | FileSystemError::TooManyLinks
            | FileSystemError::IoError
            | FileSystemError::InvalidFileSystem,
        ) => errno::EIO,
        ProgramLoadError::FileSystem(FileSystemError::Retry) => errno::EAGAIN,
        ProgramLoadError::FileSystem(FileSystemError::NoSpace) => errno::ENOMEM,
        ProgramLoadError::FileSystem(FileSystemError::NoDevice) => errno::ENXIO,
        ProgramLoadError::FileSystem(FileSystemError::NotSupported) => errno::EOPNOTSUPP,
        ProgramLoadError::FileSystem(FileSystemError::Errno(value)) => value as isize,
    };
    -errno
}

fn pid_allocation_errno(error: crate::namespace::PidAllocationError) -> isize {
    match error {
        crate::namespace::PidAllocationError::OutOfMemory
        | crate::namespace::PidAllocationError::Dead => -errno::ENOMEM,
        crate::namespace::PidAllocationError::Invalid => -errno::EINVAL,
        crate::namespace::PidAllocationError::Exhausted => -errno::EAGAIN,
        crate::namespace::PidAllocationError::Permission => -errno::EPERM,
        crate::namespace::PidAllocationError::Exists => -errno::EEXIST,
    }
}
