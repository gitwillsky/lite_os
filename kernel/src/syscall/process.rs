use alloc::vec::Vec;

use crate::{
    fs::FileSystemError,
    memory::{ElfLoadError, UserAccessError},
    syscall::errno,
    task::{
        ProgramLoadError, TaskControlBlock, current_task, exit_current_and_run_next,
        load_program_from_fs, suspend_current_and_run_next,
    },
};

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
/// @return 当前唯一的 init process 由 kernel 创建，因此父 PID 为零。
pub(crate) fn sys_get_ppid() -> isize {
    // 当前没有 clone/fork 入口，不存在第二个 Process；缺少这个边界会伪造尚未存在的 parent graph。
    0
}

/// @description 返回当前线程标识；单线程模型中与 PID 相同。
///
/// @return 当前任务的 TID。
pub(crate) fn sys_get_tid() -> isize {
    current_task()
        .expect("gettid requires a current task")
        .tid() as isize
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
    let path = match copy_user_c_string(&task, path, MAX_PATH_BYTES, errno::ENAMETOOLONG) {
        Ok(path) if !path.is_empty() => path,
        Ok(_) => return -errno::ENOENT,
        Err(error) => return error,
    };
    let path = match resolve_exec_path(&task, path) {
        Ok(path) => path,
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

    let elf = match load_program_from_fs(&path) {
        Ok(elf) => elf,
        Err(error) => return program_load_errno(error),
    };
    match task.execve_replace(&elf, &argv, &envp) {
        Ok(()) => 0,
        Err(ElfLoadError::OutOfMemory) => -errno::ENOMEM,
        Err(ElfLoadError::InvalidElf) => -errno::ENOEXEC,
    }
}

fn resolve_exec_path(task: &TaskControlBlock, path: Vec<u8>) -> Result<Vec<u8>, isize> {
    if path.first() == Some(&b'/') {
        return Ok(path);
    }

    let cwd = task.cwd();
    let mut resolved = Vec::new();
    resolved
        .try_reserve_exact(cwd.len() + 1 + path.len())
        .map_err(|_| -errno::ENOMEM)?;
    resolved.extend_from_slice(cwd.as_bytes());
    if resolved.last() != Some(&b'/') {
        resolved.push(b'/');
    }
    resolved.extend_from_slice(&path);
    if resolved.len() >= MAX_PATH_BYTES {
        return Err(-errno::ENAMETOOLONG);
    }
    Ok(resolved)
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
