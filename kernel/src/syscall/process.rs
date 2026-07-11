use alloc::vec::Vec;

use crate::{
    fs::{FileSystemError, vfs},
    memory::{ElfLoadError, UserAccessError},
    syscall::errno,
    task::{
        ProgramLoadError, TaskControlBlock, ThreadCloneError, WaitChildError, clone_current_thread,
        current_task, exit_current_and_run_next, fork_current_process, load_program_from_inode,
        parent_pid, reap_child, suspend_current_and_run_next, thread_count, wait_child,
    },
};

use super::INTERNAL_RESTART_SYS;

const MAX_PATH_BYTES: usize = 4096;
const MAX_ARG_STRING_BYTES: usize = 32 * 4096;
const MAX_ARG_BYTES: usize = 128 * 1024;

/// @description 终止当前任务并切换到调度器。
///
/// @param exit_code 用户态退出状态。
/// @return 此函数不返回。
pub(crate) fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code)
}

/// @description 主动让出处理器。
///
/// @return 成功返回零。
pub(crate) fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}

/// @description 返回当前进程标识。
///
/// @return 当前任务的 PID。
pub(crate) fn sys_get_pid() -> isize {
    current_task()
        .expect("getpid requires a current task")
        .tgid() as isize
}

/// @description 返回当前进程的父进程标识。
///
/// @return process graph 中的 parent TGID；init 返回零。
pub(crate) fn sys_get_ppid() -> isize {
    let task = current_task().expect("getppid requires a current task");
    parent_pid(task.tgid()) as isize
}

/// @description 返回当前线程标识；单线程模型中与 PID 相同。
///
/// @return 当前任务的 TID。
pub(crate) fn sys_get_tid() -> isize {
    current_task()
        .expect("gettid requires a current task")
        .tid() as isize
}

/// @description 实现 fork-shaped Linux/riscv64 clone；不伪造 thread/TLS/tid-pointer 语义。
///
/// @param flags 当前必须精确为 `SIGCHLD`。
/// @param stack fork child 继承栈，必须为零。
/// @param parent_tid fork flags 未启用对应语义，按 Linux 规则忽略。
/// @param tls fork flags 未启用对应语义，按 Linux 规则忽略。
/// @param child_tid fork flags 未启用对应语义，按 Linux 规则忽略。
/// @return parent 获得 child PID，child 获得零；失败返回负 errno。
pub(crate) fn sys_clone(
    flags: usize,
    stack: usize,
    parent_tid: usize,
    tls: usize,
    child_tid: usize,
) -> isize {
    const SIGCHLD: usize = 17;
    if flags == SIGCHLD {
        if stack != 0 {
            return -errno::EINVAL;
        }
        let current = current_task().expect("clone requires current task");
        if thread_count(current.tgid()) != 1 {
            return -errno::EAGAIN;
        }
        return match fork_current_process() {
            Ok(pid) => pid as isize,
            Err(error) if error.is_out_of_memory() => -errno::ENOMEM,
            Err(_) => -errno::EAGAIN,
        };
    }
    const CLONE_VM: usize = 0x100;
    const CLONE_FS: usize = 0x200;
    const CLONE_FILES: usize = 0x400;
    const CLONE_SIGHAND: usize = 0x800;
    const CLONE_THREAD: usize = 0x1_0000;
    const CLONE_SYSVSEM: usize = 0x4_0000;
    const CLONE_SETTLS: usize = 0x8_0000;
    const CLONE_PARENT_SETTID: usize = 0x10_0000;
    const CLONE_CHILD_CLEARTID: usize = 0x20_0000;
    const CLONE_DETACHED: usize = 0x40_0000;
    const CLONE_CHILD_SETTID: usize = 0x100_0000;
    const REQUIRED: usize = CLONE_VM
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_THREAD
        | CLONE_SYSVSEM
        | CLONE_SETTLS;
    // Linux 保留并忽略历史 CLONE_DETACHED；musl pthread_create 始终携带该 bit。
    // 若把它当未知 flag 拒绝，标准 pthread clone 会在任何 Thread 发布前错误返回 EINVAL。
    const OPTIONAL: usize =
        CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID | CLONE_CHILD_SETTID | CLONE_DETACHED;
    if flags & REQUIRED != REQUIRED
        || flags & !(REQUIRED | OPTIONAL) != 0
        || stack == 0
        || flags & CLONE_PARENT_SETTID != 0 && parent_tid == 0
        || flags & (CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID) != 0 && child_tid == 0
    {
        return -errno::EINVAL;
    }
    match clone_current_thread(
        stack,
        tls,
        (flags & CLONE_PARENT_SETTID != 0).then_some(parent_tid),
        (flags & CLONE_CHILD_SETTID != 0).then_some(child_tid),
        (flags & CLONE_CHILD_CLEARTID != 0).then_some(child_tid),
    ) {
        Ok(tid) => tid as isize,
        Err(ThreadCloneError::Fault) => -errno::EFAULT,
        Err(ThreadCloneError::Memory(error)) if error.is_out_of_memory() => -errno::ENOMEM,
        Err(ThreadCloneError::Memory(_)) => -errno::EINVAL,
    }
}

/// @description 设置 calling Thread 的 clear-child-tid 地址。
///
/// @param address 零表示清除，否则 thread exit 时写零并 futex wake。
/// @return calling TID。
pub(crate) fn sys_set_tid_address(address: usize) -> isize {
    current_task()
        .expect("set_tid_address requires current task")
        .set_clear_child_tid(address) as isize
}

/// @description 注册 calling Thread 的 Linux robust-list head。
///
/// @param head 用户 robust_list_head 地址。
/// @param length RV64 必须为 24 bytes。
/// @return 成功返回零，形状错误返回 `EINVAL`。
pub(crate) fn sys_set_robust_list(head: usize, length: usize) -> isize {
    current_task()
        .expect("set_robust_list requires current task")
        .set_robust_list(head, length)
        .map_or(-errno::EINVAL, |()| 0)
}

/// @description 等待并消费直接 child 的最小 exit record。
///
/// @param pid `-1` 表示任一 child，正数表示指定 child。
/// @param status 可为空；非空时写入 Linux wait status word。
/// @param options 当前只接受零或 `WNOHANG`。
/// @param rusage 当前必须为空，避免返回未实现的资源统计。
/// @return child PID、WNOHANG 的零，或负 Linux errno。
pub(crate) fn sys_wait4(pid: isize, status: *mut i32, options: usize, rusage: *mut u8) -> isize {
    const WNOHANG: usize = 1;
    if options & !WNOHANG != 0 || !rusage.is_null() {
        return -errno::EINVAL;
    }
    let current = current_task().expect("wait4 requires current task");
    if thread_count(current.tgid()) != 1 {
        return -errno::EAGAIN;
    }
    let record = match wait_child(pid, options & WNOHANG != 0) {
        Ok(Some(record)) => record,
        Ok(None) => return 0,
        Err(WaitChildError::NoChild) => return -errno::ECHILD,
        Err(WaitChildError::InvalidSelector) => return -errno::EINVAL,
        Err(WaitChildError::Interrupted) => return INTERNAL_RESTART_SYS,
    };
    if !status.is_null() {
        let task = current_task().expect("wait4 copyout requires current task");
        if task
            .copy_to_user(status as usize, &record.status.to_ne_bytes())
            .is_err()
        {
            return -errno::EFAULT;
        }
    }
    reap_child(record.pid);
    record.pid as isize
}

/// @description 用新的静态 RV64 ELF 映像、参数和环境替换当前进程。
///
/// @param path NUL 结尾的可执行文件路径字节。
/// @param argv NUL 结尾的参数指针数组。
/// @param envp NUL 结尾的环境指针数组。
/// @return 新上下文准备完成时返回零，失败返回负 errno。
pub(crate) fn sys_execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    if thread_count(task.tgid()) != 1 {
        // exec 必须先终止 sibling；在该事务实现前明确拒绝，避免共享地址空间替换后出现 stale context。
        return -errno::EAGAIN;
    }
    let path = match copy_user_c_string(&task, path, MAX_PATH_BYTES, errno::ENAMETOOLONG) {
        Ok(path) if !path.is_empty() => path,
        Ok(_) => return -errno::ENOENT,
        Err(error) => return error,
    };
    let mut argument_bytes = 0;
    let mut argv = match copy_user_string_array(&task, argv, &mut argument_bytes) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    // Linux 将 NULL/空 argv 规范化为一个空 argv[0]；缺少此分支会向新映像暴露 argc=0 的异常启动契约。
    if argv.is_empty() {
        let Some(bytes) = argument_bytes
            .checked_add(core::mem::size_of::<usize>() + 1)
            .filter(|bytes| *bytes <= MAX_ARG_BYTES)
        else {
            return -errno::E2BIG;
        };
        argument_bytes = bytes;
        if argv.try_reserve(1).is_err() {
            return -errno::ENOMEM;
        }
        argv.push(Vec::new());
    }
    let envp = match copy_user_string_array(&task, envp, &mut argument_bytes) {
        Ok(envp) => envp,
        Err(error) => return error,
    };

    let start = (path.first() != Some(&b'/')).then(|| task.working_directory());
    let inode = match vfs().open_at(start, &path) {
        Ok(inode) => inode,
        Err(error) => return program_load_errno(ProgramLoadError::FileSystem(error)),
    };
    let elf = match load_program_from_inode(inode) {
        Ok(elf) => elf,
        Err(error) => return program_load_errno(error),
    };
    match task.execve_replace(&elf, &argv, &envp) {
        Ok(()) => 0,
        Err(ElfLoadError::OutOfMemory) => -errno::ENOMEM,
        Err(ElfLoadError::InvalidElf) => -errno::ENOEXEC,
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
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    for index in 0usize.. {
        *total_bytes = total_bytes
            .checked_add(core::mem::size_of::<usize>())
            .ok_or(-errno::E2BIG)?;
        if *total_bytes > MAX_ARG_BYTES {
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
        if *total_bytes > MAX_ARG_BYTES {
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
        ProgramLoadError::NotRegularFile | ProgramLoadError::NotExecutable => errno::EACCES,
        ProgramLoadError::FileSystem(FileSystemError::NotFound) => errno::ENOENT,
        ProgramLoadError::FileSystem(FileSystemError::NotDirectory) => errno::ENOTDIR,
        ProgramLoadError::FileSystem(FileSystemError::SymbolicLink) => errno::ELOOP,
        ProgramLoadError::FileSystem(FileSystemError::OutOfMemory) => errno::ENOMEM,
        ProgramLoadError::FileSystem(
            FileSystemError::AlreadyExists
            | FileSystemError::IsDirectory
            | FileSystemError::DirectoryNotEmpty
            | FileSystemError::InvalidPath
            | FileSystemError::InvalidOperation
            | FileSystemError::IoError
            | FileSystemError::InvalidFileSystem,
        ) => errno::EIO,
        ProgramLoadError::FileSystem(FileSystemError::NoSpace) => errno::ENOMEM,
    };
    -errno
}
