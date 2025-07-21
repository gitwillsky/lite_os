use alloc::{string::ToString, sync::Arc, vec::Vec};

use crate::{
    memory::page_table::{translated_byte_buffer, translated_ref_mut, translated_str},
    task::{
        self, SchedulingPolicy, TaskStatus, block_current_and_run_next, current_task,
        current_user_token, exit_current_and_run_next, get_scheduling_policy,
        loader::get_app_data_by_name, set_scheduling_policy, suspend_current_and_run_next,
    },
};

pub fn sys_exit(exit_code: i32) -> ! {
    debug!("sys_exit: exit_code={}", exit_code);
    exit_current_and_run_next(exit_code);
    unreachable!()
}

pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().pid() as isize
}

pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid();

    let trap_cx = new_task.mm.trap_context();

    // child fork return 0, so ra = 0
    trap_cx.x[10] = 0;
    task::add_task(new_task);

    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    let token = current_user_token();
    let path_str = translated_str(token, path);

    if let Some(elf_data) = get_app_data_by_name(&path_str) {
        let task = current_task().unwrap();
        task.exec(&path_str, &elf_data);
        0
    } else {
        -1
    }
}

pub fn sys_wait_pid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    let task = current_task().unwrap();

    // 检查是否有指定的子进程
    let has_target_child = {
        let children = task.children.lock();
        if pid == -1 {
            !children.is_empty()
        } else {
            children.iter().any(|child| child.pid() == pid as usize)
        }
    };

    // 如果没有目标子进程，直接返回错误
    if !has_target_child {
        return -1; // ECHILD
    }

    // 查找已退出的子进程
    let zombie_child = {
        let children = task.children.lock();
        children.iter().enumerate().find_map(|(idx, child)| {
            if child.is_zombie() && (pid == -1 || child.pid() == pid as usize) {
                Some(idx)
            } else {
                None
            }
        })
    };

    // 如果找到已退出的子进程，返回其信息
    if let Some(idx) = zombie_child {
        let child = task.children.lock().remove(idx);

        let found_pid = child.pid();
        let exit_code = child.exit_code();

        // 写入退出码到用户空间
        if !exit_code_ptr.is_null() {
            let parent_token = task.mm.memory_set.lock().token();
            *translated_ref_mut(parent_token, exit_code_ptr) = exit_code;
        }

        return found_pid as isize;
    }

    -2
}

/// 设置进程的nice值
pub fn sys_setpriority(which: i32, who: i32, prio: i32) -> isize {
    // 简化实现：只支持设置当前进程的nice值
    if which != 0 || who != 0 {
        return -1; // EPERM
    }

    // nice值范围：-20到19
    if prio < -20 || prio > 19 {
        return -1; // EINVAL
    }

    if let Some(task) = current_task() {
        task.sched.lock().set_nice(prio);
        0
    } else {
        -1
    }
}

/// 获取进程的nice值
pub fn sys_getpriority(which: i32, who: i32) -> isize {
    // 简化实现：只支持获取当前进程的nice值
    if which != 0 || who != 0 {
        return -1; // EPERM
    }

    if let Some(task) = current_task() {
        task.sched.lock().nice as isize
    } else {
        -1
    }
}

/// 设置调度策略
pub fn sys_sched_setscheduler(pid: i32, policy: i32, _param: *const u8) -> isize {
    // 简化实现：只支持设置全局调度策略，忽略进程参数
    if pid != 0 {
        return -1; // 只支持设置当前进程
    }

    let scheduling_policy = match policy {
        0 => SchedulingPolicy::FIFO,
        1 => SchedulingPolicy::RoundRobin,
        2 => SchedulingPolicy::Priority,
        3 => SchedulingPolicy::CFS,
        _ => return -1, // EINVAL
    };

    set_scheduling_policy(scheduling_policy);
    0
}

/// 获取调度策略
pub fn sys_sched_getscheduler(pid: i32) -> isize {
    if pid != 0 {
        return -1; // 只支持获取当前进程
    }

    match get_scheduling_policy() {
        SchedulingPolicy::FIFO => 0,
        SchedulingPolicy::RoundRobin => 1,
        SchedulingPolicy::Priority => 2,
        SchedulingPolicy::CFS => 3,
    }
}

/// Execute a program with arguments and environment variables
pub fn sys_execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> isize {
    // 验证输入参数
    if path.is_null() {
        return -14; // EFAULT
    }

    let token = current_user_token();
    let path_str = translated_str(token, path);

    // 验证路径长度和内容
    if path_str.is_empty() || path_str.len() > 4096 {
        return -36; // ENAMETOOLONG
    }

    // 检查路径是否包含非法字符
    if path_str.contains('\0') {
        return -22; // EINVAL
    }

    // Parse argv with strict limits
    let mut args = Vec::new();
    const MAX_ARGS: usize = 256; // 限制最大参数数量
    const MAX_ARG_LEN: usize = 4096; // 限制单个参数最大长度
    const MAX_TOTAL_ARG_SIZE: usize = 128 * 1024; // 限制所有参数总大小
    let mut total_arg_size = 0;

    if !argv.is_null() {
        let mut i = 0;
        loop {
            if i >= MAX_ARGS {
                error!("sys_execve: too many arguments (max {})", MAX_ARGS);
                return -7; // E2BIG
            }

            let arg_ptr_addr = argv as usize + i * core::mem::size_of::<*const u8>();

            // 验证指针地址是否有效
            let buffers = translated_byte_buffer(
                token,
                arg_ptr_addr as *const u8,
                core::mem::size_of::<*const u8>(),
            );
            if buffers.is_empty() || buffers[0].len() < core::mem::size_of::<*const u8>() {
                break;
            }

            let arg_ptr = usize::from_le_bytes([
                buffers[0][0],
                buffers[0][1],
                buffers[0][2],
                buffers[0][3],
                buffers[0][4],
                buffers[0][5],
                buffers[0][6],
                buffers[0][7],
            ]);

            if arg_ptr == 0 {
                break;
            }

            // 验证参数指针有效性
            if arg_ptr < 0x1000 || arg_ptr >= 0x8000_0000_0000_0000 {
                error!("sys_execve: invalid argument pointer: 0x{:x}", arg_ptr);
                return -14; // EFAULT
            }

            let arg_str = translated_str(token, arg_ptr as *const u8);

            // 验证参数长度
            if arg_str.len() > MAX_ARG_LEN {
                error!("sys_execve: argument too long (max {})", MAX_ARG_LEN);
                return -7; // E2BIG
            }

            total_arg_size += arg_str.len() + 1; // +1 for null terminator
            if total_arg_size > MAX_TOTAL_ARG_SIZE {
                error!(
                    "sys_execve: total argument size too large (max {})",
                    MAX_TOTAL_ARG_SIZE
                );
                return -7; // E2BIG
            }

            args.push(arg_str);
            i += 1;
        }
    }

    // Parse envp with strict limits
    let mut envs = Vec::new();
    const MAX_ENVS: usize = 256; // 限制最大环境变量数量
    const MAX_ENV_LEN: usize = 4096; // 限制单个环境变量最大长度
    const MAX_TOTAL_ENV_SIZE: usize = 128 * 1024; // 限制所有环境变量总大小
    let mut total_env_size = 0;

    if !envp.is_null() {
        let mut i = 0;
        loop {
            if i >= MAX_ENVS {
                error!(
                    "sys_execve: too many environment variables (max {})",
                    MAX_ENVS
                );
                return -7; // E2BIG
            }

            let env_ptr_addr = envp as usize + i * core::mem::size_of::<*const u8>();

            // 验证指针地址是否有效
            let buffers = translated_byte_buffer(
                token,
                env_ptr_addr as *const u8,
                core::mem::size_of::<*const u8>(),
            );
            if buffers.is_empty() || buffers[0].len() < core::mem::size_of::<*const u8>() {
                break;
            }

            let env_ptr = usize::from_le_bytes([
                buffers[0][0],
                buffers[0][1],
                buffers[0][2],
                buffers[0][3],
                buffers[0][4],
                buffers[0][5],
                buffers[0][6],
                buffers[0][7],
            ]);

            if env_ptr == 0 {
                break;
            }

            // 验证环境变量指针有效性
            if env_ptr < 0x1000 || env_ptr >= 0x8000_0000_0000_0000 {
                error!("sys_execve: invalid environment pointer: 0x{:x}", env_ptr);
                return -14; // EFAULT
            }

            let env_str = translated_str(token, env_ptr as *const u8);

            // 验证环境变量长度
            if env_str.len() > MAX_ENV_LEN {
                error!(
                    "sys_execve: environment variable too long (max {})",
                    MAX_ENV_LEN
                );
                return -7; // E2BIG
            }

            // 验证环境变量格式 (必须包含=)
            if !env_str.contains('=') {
                error!(
                    "sys_execve: invalid environment variable format: {}",
                    env_str
                );
                return -22; // EINVAL
            }

            total_env_size += env_str.len() + 1; // +1 for null terminator
            if total_env_size > MAX_TOTAL_ENV_SIZE {
                error!(
                    "sys_execve: total environment size too large (max {})",
                    MAX_TOTAL_ENV_SIZE
                );
                return -7; // E2BIG
            }

            envs.push(env_str);
            i += 1;
        }
    }

    // Set default arguments if none provided
    if args.is_empty() {
        args.push(path_str.clone());
    }

    // Set default environment if none provided
    if envs.is_empty() {
        envs.push("PATH=/bin:/usr/bin".to_string());
        envs.push("HOME=/".to_string());
        envs.push("USER=root".to_string());
    }

    if let Some(elf_data) = get_app_data_by_name(&path_str) {
        let task = current_task().unwrap();
        match task.exec_with_args(&path_str, &elf_data, Some(&args.as_slice()), Some(&envs.as_slice())) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    } else {
        -1
    }
}

pub fn sys_get_args(argc_buf: *mut usize, argv_buf: *mut u8, buf_len: usize) -> isize {
    let current_task = current_task().unwrap();
    let token = current_user_token();

    // Get the arguments from the current task
    let args = current_task.args.lock();
    let default_args = Vec::new();
    let args = args.as_ref().unwrap_or(&default_args);

    if args.is_empty() {
        return 0;
    }

    // Write argc to user buffer
    if !argc_buf.is_null() {
        let argc_ptr = translated_ref_mut(token, argc_buf);
        *argc_ptr = args.len();
    }

    // Write argv strings to user buffer
    if !argv_buf.is_null() && buf_len > 0 {
        let mut offset = 0;

        for arg in args.iter() {
            let arg_bytes = arg.as_bytes();
            let needed_space = arg_bytes.len() + 1; // +1 for null terminator

            if offset + needed_space > buf_len {
                return -1; // Buffer too small
            }

            // Copy argument string
            let mut buffers = translated_byte_buffer(
                token,
                (argv_buf as usize + offset) as *const u8,
                arg_bytes.len(),
            );
            if !buffers.is_empty() && buffers[0].len() >= arg_bytes.len() {
                buffers[0][..arg_bytes.len()].copy_from_slice(arg_bytes);

                // Add null terminator
                let mut null_buffers = translated_byte_buffer(
                    token,
                    (argv_buf as usize + offset + arg_bytes.len()) as *const u8,
                    1,
                );
                if !null_buffers.is_empty() && !null_buffers[0].is_empty() {
                    null_buffers[0][0] = 0;
                }
            }

            offset += needed_space;
        }
    }

    args.len() as isize
}

pub fn sys_shutdown() -> ! {
    crate::arch::sbi::shutdown();
    unreachable!();
}

// 权限相关系统调用实现

/// 获取用户ID
pub fn sys_getuid() -> isize {
    let task = current_task().unwrap();
    task.uid() as isize
}

/// 获取组ID
pub fn sys_getgid() -> isize {
    let task = current_task().unwrap();
    task.gid() as isize
}

/// 获取有效用户ID
pub fn sys_geteuid() -> isize {
    let task = current_task().unwrap();
    task.euid() as isize
}

/// 获取有效组ID
pub fn sys_getegid() -> isize {
    let task = current_task().unwrap();
    task.egid() as isize
}

/// 设置用户ID
pub fn sys_setuid(uid: u32) -> isize {
    let task = current_task().unwrap();
    match task.set_uid(uid) {
        Ok(()) => 0,
        Err(errno) => errno as isize,
    }
}

/// 设置组ID
pub fn sys_setgid(gid: u32) -> isize {
    let task = current_task().unwrap();
    match task.set_gid(gid) {
        Ok(()) => 0,
        Err(errno) => errno as isize,
    }
}

/// 设置有效用户ID
pub fn sys_seteuid(euid: u32) -> isize {
    let task = current_task().unwrap();
    match task.set_euid(euid) {
        Ok(()) => 0,
        Err(errno) => errno as isize,
    }
}

/// 设置有效组ID
pub fn sys_setegid(egid: u32) -> isize {
    let task = current_task().unwrap();
    match task.set_egid(egid) {
        Ok(()) => 0,
        Err(errno) => errno as isize,
    }
}
