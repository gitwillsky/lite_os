use alloc::{string::ToString, sync::Arc, vec::Vec};

use crate::{
    memory::{
        page_table::{translated_byte_buffer, translated_ref_mut, translated_str},
        frame_allocator,
    },
    task::{
        self, SchedulingPolicy, TaskStatus, ProcessStats, block_current_and_run_next, current_task,
        current_user_token, exit_current_and_run_next, get_scheduling_policy,
        loader::get_app_data_by_name, set_scheduling_policy, suspend_current_and_run_next,
        get_all_tasks, get_all_pids, get_task_count, find_task_by_pid, get_process_statistics,
    },
};

/// CPU核心信息
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CpuCoreInfo {
    pub total_cores: u32,     // 总核心数
    pub active_cores: u32,    // 活跃核心数
}

/// 进程状态的用户空间表示
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub status: u32,     // 0=Ready, 1=Running, 2=Zombie, 3=Sleeping
    pub priority: i32,
    pub nice: i32,
    pub vruntime: u64,
    pub heap_base: usize,
    pub heap_top: usize,
    pub last_runtime: u64,
    pub total_cpu_time: u64,  // 总CPU时间（微秒）
    pub cpu_percent: u32,     // CPU使用率百分比（0-10000，支持两位小数）
    pub core_id: u32,         // 进程运行的核心ID
    pub name: [u8; 32],       // 进程名（固定长度，以0结尾）
}

/// 系统统计信息
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemStats {
    pub total_processes: u32,
    pub running_processes: u32,
    pub sleeping_processes: u32,
    pub zombie_processes: u32,
    pub total_memory: usize,
    pub used_memory: usize,
    pub free_memory: usize,
    pub system_uptime: u64,      // 系统运行时间（微秒）
    pub cpu_user_time: u64,      // 用户态CPU时间（微秒）
    pub cpu_system_time: u64,    // 系统态CPU时间（微秒）
    pub cpu_idle_time: u64,      // 空闲CPU时间（微秒）
    pub cpu_usage_percent: u32,  // 总CPU使用率百分比（0-10000）
}

pub fn sys_exit(exit_code: i32) -> ! {
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
    
    // 添加到统一任务管理器（会自动处理调度器添加）
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

        // Return the total number of bytes written to the buffer
        return offset as isize;
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

/// 获取进程列表
/// 参数：
/// - pids: 用户空间的进程ID数组缓冲区
/// - max_count: 缓冲区最大容量
/// 返回值：实际进程数量（如果超过max_count则只填充max_count个）
pub fn sys_get_process_list(pids: *mut u32, max_count: usize) -> isize {
    let token = current_user_token();
    
    if max_count == 0 {
        return get_task_count() as isize;
    }

    // 使用新的统一接口获取所有PID
    let all_pids = get_all_pids();
    let actual_count = all_pids.len().min(max_count);

    // 将PIDs写入用户空间缓冲区
    for i in 0..actual_count {
        let pid_ptr = unsafe { pids.add(i) };
        let mut pid_buffers = translated_byte_buffer(token, pid_ptr as *const u8, core::mem::size_of::<u32>());

        if !pid_buffers.is_empty() && pid_buffers[0].len() >= core::mem::size_of::<u32>() {
            let pid_bytes = (all_pids[i] as u32).to_le_bytes();
            pid_buffers[0][..4].copy_from_slice(&pid_bytes);
        }
    }

    actual_count as isize
}

/// 获取特定进程的详细信息
/// 参数：
/// - pid: 进程ID
/// - info: 用户空间的ProcessInfo结构体指针
/// 返回值：成功返回0，失败返回-1
pub fn sys_get_process_info(pid: u32, info: *mut ProcessInfo) -> isize {
    let token = current_user_token();

    // 查找进程
    let task = if let Some(task) = find_task_by_pid(pid as usize) {
        task
    } else {
        return -1; // 进程不存在
    };

    // 构建进程信息
    let sched = task.sched.lock();
    let status = task.task_status.lock();

    // 计算CPU使用率
    let current_time = crate::timer::get_time_us();
    let total_cpu_time = task.total_cpu_time.load(core::sync::atomic::Ordering::Relaxed);
    let creation_time = task.creation_time.load(core::sync::atomic::Ordering::Relaxed);
    let process_lifetime = if current_time > creation_time {
        current_time - creation_time
    } else {
        1 // 避免除零
    };

    // CPU使用率 = (总CPU时间 / 进程生存时间) * 10000，支持两位小数
    // 限制最大为100% (10000)，避免计算错误导致的异常值
    let cpu_percent = if process_lifetime == 0 {
        0  // 生存时间为0，使用率为0
    } else if total_cpu_time == 0 {
        0  // CPU时间为0，使用率为0
    } else if total_cpu_time >= process_lifetime {
        10000  // CPU时间大于等于生存时间，使用率为100%
    } else {
        // 使用安全的128位运算计算百分比
        let percent_128 = (total_cpu_time as u128 * 10000) / process_lifetime as u128;
        if percent_128 > 10000 {
            10000  // 限制最大为100%
        } else {
            percent_128 as u32
        }
    };

    // 获取进程名并转换为固定长度数组
    let name_str = task.name();
    let mut name_bytes = [0u8; 32];
    let name_len = name_str.len().min(31); // 保留一个位置给null终止符
    name_bytes[..name_len].copy_from_slice(&name_str.as_bytes()[..name_len]);
    // name_bytes[name_len] = 0; // 已经初始化为0了

    // 获取进程当前运行的核心ID（如果正在运行），否则使用调用者的核心ID
    let core_id = if matches!(*status, crate::task::TaskStatus::Running) {
        // 对于正在运行的任务，尝试找到它所在的核心
        let mut running_core_id = 0;
        for i in 0..crate::arch::hart::MAX_CORES {
            if let Some(processor) = crate::task::multicore::CORE_MANAGER.get_processor(i) {
                let proc = processor.lock();
                if let Some(current_task) = &proc.current {
                    if current_task.pid() == task.pid() {
                        running_core_id = i;
                        break;
                    }
                }
            }
        }
        running_core_id as u32
    } else {
        // 对于非运行状态的任务，使用当前调用者的核心ID
        crate::arch::hart::hart_id() as u32
    };

    let process_info = ProcessInfo {
        pid: task.pid() as u32,
        ppid: task.parent().map(|p| p.pid() as u32).unwrap_or(0),
        uid: task.uid(),
        gid: task.gid(),
        euid: task.euid(),
        egid: task.egid(),
        status: match *status {
            TaskStatus::Ready => 0,
            TaskStatus::Running => 1,
            TaskStatus::Zombie => 2,
            TaskStatus::Sleeping => 3,
        },
        priority: sched.priority,
        nice: sched.nice,
        vruntime: sched.vruntime,
        heap_base: task.mm.heap_base.load(core::sync::atomic::Ordering::Relaxed),
        heap_top: task.mm.heap_top.load(core::sync::atomic::Ordering::Relaxed),
        last_runtime: task.last_runtime.load(core::sync::atomic::Ordering::Relaxed),
        total_cpu_time,
        cpu_percent,
        core_id,
        name: name_bytes,
    };

    // 将信息写入用户空间
    let mut info_buffers = translated_byte_buffer(token, info as *const u8, core::mem::size_of::<ProcessInfo>());

    if !info_buffers.is_empty() && info_buffers[0].len() >= core::mem::size_of::<ProcessInfo>() {
        let info_bytes = unsafe {
            core::slice::from_raw_parts(
                &process_info as *const ProcessInfo as *const u8,
                core::mem::size_of::<ProcessInfo>()
            )
        };
        info_buffers[0][..core::mem::size_of::<ProcessInfo>()].copy_from_slice(info_bytes);
        0
    } else {
        -1
    }
}

/// 获取系统统计信息
/// 参数：
/// - stats: 用户空间的SystemStats结构体指针
/// 返回值：成功返回0，失败返回-1
pub fn sys_get_system_stats(stats: *mut SystemStats) -> isize {
    let token = current_user_token();
    
    // 使用新的统一接口获取进程统计信息
    let process_stats = get_process_statistics();
    let all_tasks = get_all_tasks();
    
    let mut total_cpu_user_time = 0u64;
    let mut total_cpu_kernel_time = 0u64;

    // 累计CPU时间
    for task in &all_tasks {
        total_cpu_user_time += task.user_cpu_time.load(core::sync::atomic::Ordering::Relaxed);
        total_cpu_kernel_time += task.kernel_cpu_time.load(core::sync::atomic::Ordering::Relaxed);
    }

    // 计算系统运行时间和CPU使用率
    let current_time = crate::timer::get_time_us();
    let system_uptime = current_time; // 系统运行时间

    // 获取当前激活的核心数量
    let active_cores = crate::task::multicore::CORE_MANAGER.active_core_count();
    let total_active_cpu_time = total_cpu_user_time + total_cpu_kernel_time;

    // 在多核系统中，总可用CPU时间 = 系统时间 × 核心数
    // CPU使用率 = min(活跃时间 / (系统时间 × 核心数), 1.0) * 100%
    let cpu_usage_percent = if system_uptime == 0 || active_cores == 0 {
        0  // 系统时间或核心数为0，使用率为0
    } else if total_active_cpu_time == 0 {
        0  // 活跃CPU时间为0，使用率为0
    } else {
        let total_available_cpu_time = system_uptime * active_cores as u64;
        if total_active_cpu_time >= total_available_cpu_time {
            10000  // 活跃时间大于等于可用时间，使用率为100%
        } else {
            // 使用安全的128位运算
            let percent_128 = (total_active_cpu_time as u128 * 10000) / total_available_cpu_time as u128;
            core::cmp::min(percent_128 as u64, 10000) as u32
        }
    };

    let total_available_cpu_time = system_uptime * active_cores as u64;
    let cpu_idle_time = if total_available_cpu_time > total_active_cpu_time {
        total_available_cpu_time - total_active_cpu_time
    } else {
        0  // 如果活跃时间超过总可用时间，说明计算有误，设为0
    };

    // 获取真实的内存统计信息（只调用一次）
    let (total_memory, used_memory, free_memory) = frame_allocator::get_memory_stats();

    let system_stats = SystemStats {
        total_processes: process_stats.total,
        running_processes: process_stats.running,
        sleeping_processes: process_stats.sleeping,
        zombie_processes: process_stats.zombie,
        total_memory,
        used_memory,
        free_memory,
        system_uptime,
        cpu_user_time: total_cpu_user_time,
        cpu_system_time: total_cpu_kernel_time,
        cpu_idle_time,
        cpu_usage_percent,
    };

    // 将统计信息写入用户空间
    let mut stats_buffers = translated_byte_buffer(token, stats as *const u8, core::mem::size_of::<SystemStats>());

    if !stats_buffers.is_empty() && stats_buffers[0].len() >= core::mem::size_of::<SystemStats>() {
        let stats_bytes = unsafe {
            core::slice::from_raw_parts(
                &system_stats as *const SystemStats as *const u8,
                core::mem::size_of::<SystemStats>()
            )
        };
        stats_buffers[0][..core::mem::size_of::<SystemStats>()].copy_from_slice(stats_bytes);
        0
    } else {
        -1
    }
}

/// 获取CPU核心信息
/// 参数：
/// - core_info: 用户空间的CpuCoreInfo结构体指针
/// 返回值：成功返回0，失败返回-1
pub fn sys_get_cpu_core_info(core_info: *mut CpuCoreInfo) -> isize {
    let token = current_user_token();
    
    let cpu_core_info = CpuCoreInfo {
        total_cores: crate::arch::hart::MAX_CORES as u32,
        active_cores: crate::task::multicore::CORE_MANAGER.active_core_count() as u32,
    };
    
    // 将核心信息写入用户空间
    let mut info_buffers = translated_byte_buffer(token, core_info as *const u8, core::mem::size_of::<CpuCoreInfo>());
    
    if !info_buffers.is_empty() && info_buffers[0].len() >= core::mem::size_of::<CpuCoreInfo>() {
        let info_bytes = unsafe {
            core::slice::from_raw_parts(
                &cpu_core_info as *const CpuCoreInfo as *const u8,
                core::mem::size_of::<CpuCoreInfo>()
            )
        };
        info_buffers[0][..core::mem::size_of::<CpuCoreInfo>()].copy_from_slice(info_bytes);
        0
    } else {
        -1
    }
}
