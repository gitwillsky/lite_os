use alloc::{string::String, vec::Vec};

use crate::{
    memory::page_table::translated_byte_buffer,
    syscall::errno,
    task::{
        current_task, current_user_token, exit_current_and_run_next, loader::get_app_data_by_name,
        suspend_current_and_run_next,
    },
};

const MAX_PATH_LEN: usize = 4096;
const MAX_ARRAY_ITEMS: usize = 256;
const MAX_ARRAY_BYTES: usize = 128 * 1024;

/// @description 终止当前任务并切换到调度器。
///
/// @param exit_code 用户态退出状态。
/// @return 此函数不返回。
pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    unreachable!()
}

/// @description 主动让出处理器。
///
/// @return 成功返回零。
pub fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}

/// @description 返回当前进程标识。
///
/// @return 当前任务的 PID。
pub fn sys_get_pid() -> isize {
    current_task().expect("getpid requires a current task").pid() as isize
}

/// @description 返回当前线程标识；单线程模型中与 PID 相同。
///
/// @return 当前任务的 TID。
pub fn sys_get_tid() -> isize {
    current_task().expect("gettid requires a current task").pid() as isize
}

/// @description 用新的 ELF 映像、参数和环境替换当前进程。
///
/// @param path NUL 结尾的可执行文件路径。
/// @param argv NUL 结尾的参数指针数组。
/// @param envp NUL 结尾的环境指针数组。
/// @return 新上下文准备完成时返回零，失败返回负 errno。
pub fn sys_execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> isize {
    let token = current_user_token();
    let path = match copy_user_string(token, path, MAX_PATH_LEN) {
        Ok(path) if !path.is_empty() => path,
        Ok(_) => return -errno::ENOENT,
        Err(error) => return error,
    };
    let argv = match copy_user_string_array(token, argv) {
        Ok(argv) => argv,
        Err(error) => return error,
    };
    let envp = match copy_user_string_array(token, envp) {
        Ok(envp) => envp,
        Err(error) => return error,
    };

    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(elf) = get_app_data_by_name(&path) else {
        return -errno::ENOENT;
    };

    match task.execve_replace(&path, &elf, &argv, &envp) {
        Ok(()) => 0,
        Err(crate::memory::mm::MemoryError::OutOfMemory) => -errno::ENOMEM,
        Err(_) => -errno::EINVAL,
    }
}

/// @description 设置当前任务的真实与有效用户 ID。
///
/// @param uid 新的用户 ID。
/// @return 成功返回零，权限不足返回负 errno。
pub fn sys_setuid(uid: u32) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    match task.set_uid(uid) {
        Ok(()) => 0,
        Err(error) => error as isize,
    }
}

fn copy_user_string(token: usize, pointer: *const u8, max_len: usize) -> Result<String, isize> {
    if pointer.is_null() {
        return Err(-errno::EFAULT);
    }

    let mut value = String::new();
    for offset in 0..max_len {
        let buffers = translated_byte_buffer(token, unsafe { pointer.add(offset) }, 1);
        let Some(byte) = buffers.first().and_then(|buffer| buffer.first()).copied() else {
            return Err(-errno::EFAULT);
        };
        if byte == 0 {
            return Ok(value);
        }
        value.push(byte as char);
    }
    Err(-errno::ENAMETOOLONG)
}

fn copy_user_string_array(token: usize, array: *const *const u8) -> Result<Vec<String>, isize> {
    if array.is_null() {
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    let mut total_bytes = 0usize;
    for index in 0..MAX_ARRAY_ITEMS {
        let pointer_address = (array as usize)
            .checked_add(index * core::mem::size_of::<usize>())
            .ok_or(-errno::EFAULT)?;
        let buffers = translated_byte_buffer(
            token,
            pointer_address as *const u8,
            core::mem::size_of::<usize>(),
        );
        if buffers.len() != 1 || buffers[0].len() != core::mem::size_of::<usize>() {
            return Err(-errno::EFAULT);
        }
        let mut pointer_bytes = [0u8; core::mem::size_of::<usize>()];
        pointer_bytes.copy_from_slice(buffers[0]);
        let pointer = usize::from_ne_bytes(pointer_bytes);
        if pointer == 0 {
            return Ok(values);
        }

        let value = copy_user_string(token, pointer as *const u8, MAX_PATH_LEN)?;
        total_bytes = total_bytes
            .checked_add(value.len() + 1)
            .ok_or(-errno::E2BIG)?;
        if total_bytes > MAX_ARRAY_BYTES {
            return Err(-errno::E2BIG);
        }
        values.push(value);
    }
    Err(-errno::E2BIG)
}
