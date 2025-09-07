use alloc::{string::{String, ToString}, sync::Arc, vec::Vec};

use crate::{
    memory::{
        frame_allocator,
        page_table::{translated_byte_buffer, translated_ref_mut, translated_str},
    },
    syscall::errno,
    task::{
        self, ProcessStats, SchedulingPolicy, TaskStatus, block_current_and_run_next, current_task,
        current_user_token, exit_current_and_run_next, find_task_by_pid, get_all_pids,
        get_all_tasks, get_process_statistics, get_scheduling_policy, get_task_count,
        loader::get_app_data_by_name, set_scheduling_policy, suspend_current_and_run_next,
    },
};

/// CPU核心信息
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CpuCoreInfo {
    pub total_cores: u32,  // 总核心数
    pub active_cores: u32, // 活跃核心数
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
    pub status: u32, // 0=Ready, 1=Running, 2=Zombie, 3=Sleeping
    pub priority: i32,
    pub nice: i32,
    pub vruntime: u64,
    pub heap_base: usize,
    pub heap_top: usize,
    pub last_runtime: u64,
    pub total_cpu_time: u64, // 总CPU时间（微秒）
    pub cpu_percent: u32,    // CPU使用率百分比（0-10000，支持两位小数）
    pub core_id: u32,        // 进程运行的核心ID
    pub name: [u8; 32],      // 进程名（固定长度，以0结尾）
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
    pub system_uptime: u64,     // 系统运行时间（微秒）
    pub cpu_user_time: u64,     // 用户态CPU时间（微秒）
    pub cpu_system_time: u64,   // 系统态CPU时间（微秒）
    pub cpu_idle_time: u64,     // 空闲CPU时间（微秒）
    pub cpu_usage_percent: u32, // 总CPU使用率百分比（0-10000）
}

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    unreachable!()
}

pub fn sys_sched_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_get_pid() -> isize {
    current_task().unwrap().pid() as isize
}

pub fn sys_get_ppid() -> isize {
    if let Some(current) = current_task() {
        current.parent().unwrap().pid() as isize
    } else {
        -1
    }
}

pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let new_task = match current_task.fork() {
        Ok(task) => task,
        Err(_) => return -errno::ENOMEM,
    };
    let new_pid = new_task.pid();

    let trap_cx = new_task.mm.trap_context();

    // child fork return 0, so ra = 0
    trap_cx.x[10] = 0;

    // 添加到统一任务管理器（会自动处理调度器添加）
    task::add_task(new_task);

    new_pid as isize
}

/// clone - 创建子进程或线程，精确控制共享的资源
///
/// 参数：
/// - flags: 控制父子间资源共享的标志位
/// - child_stack: 子进程/线程的用户栈指针（0表示使用默认）
/// - parent_tid: 父进程中存储子进程TID的位置（暂未实现）
/// - child_tid: 子进程中存储自己TID的位置（暂未实现）
/// - tls: 线程本地存储指针（暂未实现）
///
/// 返回值：
/// - 成功：子进程/线程的PID/TID
/// - 失败：负的错误码
pub fn sys_clone(
    flags: i32,
    child_stack: usize,
    parent_tid: *mut i32,
    child_tid: *mut i32,
    tls: usize,
) -> isize {
    let current_task = current_task().unwrap();

    // 处理栈指针：0表示使用默认栈
    let stack = if child_stack == 0 { None } else { Some(child_stack) };

    // 暂时不支持 parent_tid, child_tid, tls 参数
    // 在完整实现中，这些参数用于高级线程创建场景
    if !parent_tid.is_null() || !child_tid.is_null() || tls != 0 {
        warn!("clone: parent_tid, child_tid, and tls parameters not yet implemented");
    }

    // 创建新的任务
    let new_task = match current_task.clone_with_flags(flags, stack, None) {
        Ok(task) => task,
        Err(_) => return -errno::ENOMEM,
    };

    let new_pid = new_task.pid();

    // 检查是否需要特殊处理 (如 CLONE_VFORK)
    const CLONE_VFORK: i32 = 0x00004000;
    let is_vfork = (flags & CLONE_VFORK) != 0;

    if is_vfork {
        // VFORK语义：父进程应该阻塞直到子进程调用exec或exit
        // 简化实现：暂时不实现阻塞，直接继续执行
        warn!("clone: CLONE_VFORK not fully implemented - parent will not block");
    }

    // 设置子进程/线程的返回值为0
    // 检查是否共享虚拟内存 (通过比较Arc指针地址)
    let shares_vm = Arc::ptr_eq(&new_task.mm.memory_set, &current_task.mm.memory_set);
    if !shares_vm {
        // 只有在不共享内存的情况下才需要单独设置返回值
        let trap_cx = new_task.mm.trap_context();
        trap_cx.x[10] = 0; // a0 寄存器 (返回值)
    }

    // 添加到任务管理器
    task::add_task(new_task);

    new_pid as isize
}

/// 创建线程：在当前进程地址空间内生成一个轻量级任务
/// 参数：entry 用户入口、user_sp 用户栈顶、arg 传入a0
pub fn sys_thread_create(entry: usize, user_sp: usize, arg: usize) -> isize {
    let current = current_task().unwrap();
    match current.spawn_thread(entry, user_sp, arg) {
        Ok(t) => {
            let tid = t.pid();
            crate::task::add_task(t);
            tid as isize
        }
        Err(_) => -errno::ENOMEM,
    }
}

/// 线程退出
pub fn sys_thread_exit(code: i32) -> ! {
    let task = current_task().unwrap();
    // 主线程退出仍然走进程退出逻辑
    if task.pid() == task.tgid() {
        exit_current_and_run_next(code);
    } else {
        crate::task::exit_current_thread_and_run_next(code);
    }
    unreachable!()
}

/// 线程等待
pub fn sys_thread_join(tid: usize, exit_code_ptr: *mut i32) -> isize {
    use crate::task::{TaskStatus, current_user_token, find_task_by_pid, set_task_status};
    let waiter = current_task().unwrap();
    // 自己不能 join 自己
    if waiter.pid() == tid {
        return -errno::EINVAL;
    }

    loop {
        if let Some(target) = find_task_by_pid(tid) {
            if target.is_zombie() {
                // 拿退出码
                if !exit_code_ptr.is_null() {
                    let token = current_user_token();
                    unsafe {
                        *translated_ref_mut(token, exit_code_ptr) = target.exit_code();
                    }
                }
                // 移除目标
                crate::task::remove_task(tid);
                return 0;
            }
            // 注册等待并睡眠
            crate::task::task_manager::register_thread_join_waiter(tid, waiter.clone());
            set_task_status(&waiter, TaskStatus::Sleeping);
            block_current_and_run_next();
        } else {
            return -errno::ESRCH; // 不存在
        }
    }
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

        // 从全局任务管理器中移除僵尸进程
        crate::task::remove_task(found_pid);

        return found_pid as isize;
    }

    -2
}

/// waitid 调用使用的 idtype 常量
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IdType {
    P_ALL = 0,     // 等待任何子进程
    P_PID = 1,     // 等待指定PID的子进程
    P_PGID = 2,    // 等待指定进程组的任何子进程
    P_PIDFD = 3,   // 等待pidfd对应的子进程
}

impl IdType {
    fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(IdType::P_ALL),
            1 => Some(IdType::P_PID),
            2 => Some(IdType::P_PGID),
            3 => Some(IdType::P_PIDFD),
            _ => None,
        }
    }
}

/// waitid 选项常量
pub mod waitid_options {
    pub const WEXITED: i32 = 0x00000004;     // 等待已退出的子进程
    pub const WSTOPPED: i32 = 0x00000002;    // 等待被停止的子进程
    pub const WCONTINUED: i32 = 0x00000008;  // 等待被继续的子进程
    pub const WNOHANG: i32 = 0x00000001;     // 非阻塞等待
    pub const WNOWAIT: i32 = 0x01000000;     // 不移除子进程的僵尸状态
}

/// siginfo_t 结构体（简化版）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigInfo {
    pub si_signo: i32,      // 信号编号 (SIGCHLD)
    pub si_errno: i32,      // 错误号
    pub si_code: i32,       // 信号代码
    pub si_pid: i32,        // 发送信号的进程PID
    pub si_uid: i32,        // 发送信号的进程UID
    pub si_status: i32,     // 退出状态或信号编号
    pub si_utime: i64,      // 用户CPU时间
    pub si_stime: i64,      // 系统CPU时间
    pub _pad: [u8; 104],    // 填充以匹配标准大小
}

impl SigInfo {
    fn new() -> Self {
        Self {
            si_signo: 0,
            si_errno: 0,
            si_code: 0,
            si_pid: 0,
            si_uid: 0,
            si_status: 0,
            si_utime: 0,
            si_stime: 0,
            _pad: [0; 104],
        }
    }
}

/// waitid 信号代码常量
pub mod waitid_si_codes {
    pub const CLD_EXITED: i32 = 1;       // 子进程已退出
    pub const CLD_KILLED: i32 = 2;       // 子进程被信号终止
    pub const CLD_DUMPED: i32 = 3;       // 子进程被信号终止并产生核心转储
    pub const CLD_STOPPED: i32 = 5;      // 子进程被停止
    pub const CLD_TRAPPED: i32 = 4;      // 子进程因调试陷阱而停止
    pub const CLD_CONTINUED: i32 = 6;    // 子进程被继续
}

/// waitid - Linux标准的waitid系统调用
///
/// 参数：
/// - idtype: 指定等待的进程类型 (P_ALL, P_PID, P_PGID, P_PIDFD)
/// - id: 根据idtype指定的进程ID
/// - infop: 用于返回子进程信息的siginfo_t结构体指针
/// - options: 等待选项的组合 (WEXITED, WSTOPPED, WCONTINUED, WNOHANG, WNOWAIT)
///
/// 返回值：
/// - 成功：0
/// - 失败：负的错误码
pub fn sys_wait_id(idtype: i32, id: i32, infop: *mut SigInfo, options: i32) -> isize {
    // 步骤1: 验证参数
    let idtype_enum = match IdType::from_i32(idtype) {
        Some(t) => t,
        None => return -errno::EINVAL,
    };

    // 验证选项参数
    if options == 0 {
        return -errno::EINVAL; // 必须指定至少一个等待选项
    }

    // 检查是否指定了有效的等待条件
    let wait_for_exited = (options & waitid_options::WEXITED) != 0;
    let wait_for_stopped = (options & waitid_options::WSTOPPED) != 0;
    let wait_for_continued = (options & waitid_options::WCONTINUED) != 0;

    if !wait_for_exited && !wait_for_stopped && !wait_for_continued {
        return -errno::EINVAL; // 必须指定等待条件
    }

    let no_hang = (options & waitid_options::WNOHANG) != 0;
    let no_wait = (options & waitid_options::WNOWAIT) != 0;

    // 步骤2: 获取当前任务
    let current_task = match current_task() {
        Some(task) => task,
        None => return -errno::ESRCH,
    };

    // 步骤3: 验证infop指针并准备写入用户空间
    if infop.is_null() {
        return -errno::EFAULT;
    }

    let token = current_user_token();

    // 步骤4: 查找匹配的子进程
    loop {
        let mut matched_child = None;
        let mut child_index = None;

        {
            let children = current_task.children.lock();

            // 检查是否有符合条件的子进程
            let has_matching_children = match idtype_enum {
                IdType::P_ALL => !children.is_empty(),
                IdType::P_PID => {
                    if id <= 0 {
                        return -errno::EINVAL;
                    }
                    children.iter().any(|child| child.pid() == id as usize)
                },
                IdType::P_PGID => {
                    // 简化实现：暂不支持进程组
                    return -errno::ENOSYS;
                },
                IdType::P_PIDFD => {
                    // 简化实现：暂不支持pidfd
                    return -errno::ENOSYS;
                },
            };

            if !has_matching_children {
                return -errno::ECHILD;
            }

            // 查找满足等待条件的子进程
            for (idx, child) in children.iter().enumerate() {
                let child_matches_id = match idtype_enum {
                    IdType::P_ALL => true,
                    IdType::P_PID => child.pid() == id as usize,
                    IdType::P_PGID | IdType::P_PIDFD => continue, // 已在上面处理
                };

                if !child_matches_id {
                    continue;
                }

                let child_status = *child.task_status.lock();
                let mut should_return = false;
                let mut sig_info = SigInfo::new();

                match child_status {
                    TaskStatus::Zombie if wait_for_exited => {
                        // 子进程已退出
                        sig_info.si_signo = 17; // SIGCHLD
                        sig_info.si_code = waitid_si_codes::CLD_EXITED;
                        sig_info.si_pid = child.pid() as i32;
                        sig_info.si_uid = child.uid() as i32;
                        sig_info.si_status = child.exit_code();
                        sig_info.si_utime = child.user_cpu_time.load(core::sync::atomic::Ordering::Relaxed) as i64;
                        sig_info.si_stime = child.kernel_cpu_time.load(core::sync::atomic::Ordering::Relaxed) as i64;
                        should_return = true;
                    },
                    TaskStatus::Stopped if wait_for_stopped => {
                        // 子进程被停止
                        sig_info.si_signo = 17; // SIGCHLD
                        sig_info.si_code = waitid_si_codes::CLD_STOPPED;
                        sig_info.si_pid = child.pid() as i32;
                        sig_info.si_uid = child.uid() as i32;
                        sig_info.si_status = 19; // SIGSTOP
                        sig_info.si_utime = child.user_cpu_time.load(core::sync::atomic::Ordering::Relaxed) as i64;
                        sig_info.si_stime = child.kernel_cpu_time.load(core::sync::atomic::Ordering::Relaxed) as i64;
                        should_return = true;
                    },
                    _ => {
                        // 其他状态暂不处理继续状态
                        continue;
                    }
                }

                if should_return {
                    matched_child = Some(sig_info);
                    if child_status == TaskStatus::Zombie && !no_wait {
                        child_index = Some(idx);
                    }
                    break;
                }
            }
        }

        // 步骤5: 如果找到匹配的子进程
        if let Some(sig_info) = matched_child {
            // 写入siginfo_t到用户空间
            let info_ref = match unsafe { translated_ref_mut(token, infop) } {
                info => info,
            };
            *info_ref = sig_info;

            // 如果是已退出的子进程且没有WNOWAIT标志，则移除子进程
            if let Some(idx) = child_index {
                let removed_child = current_task.children.lock().remove(idx);
                let child_pid = removed_child.pid();

                // 从全局任务管理器中移除僵尸进程
                crate::task::remove_task(child_pid);
            }

            return 0; // 成功
        }

        // 步骤6: 如果没有找到匹配的子进程
        if no_hang {
            // WNOHANG: 非阻塞模式，直接返回
            // 清空siginfo_t结构体
            let info_ref = unsafe { translated_ref_mut(token, infop) };
            *info_ref = SigInfo::new();
            return 0;
        }

        // 步骤7: 阻塞等待
        // 在真实实现中，这里应该将当前进程挂起并等待SIGCHLD信号
        // 简化实现：暂停当前任务并重新调度
        suspend_current_and_run_next();
    }
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

/// execve - 执行新程序，完全替换当前进程的内存映像
///
/// 这是Linux标准的execve系统调用实现，完全遵循POSIX规范：
/// - 成功时不返回，因为当前进程已被新程序替换
/// - 失败时返回-1并设置errno
/// - 保留PID，但重置所有其他进程状态
/// - 关闭标记了O_CLOEXEC的文件描述符
/// - 重置信号处理器为默认状态
pub fn sys_execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> isize {
    // 步骤1: 验证输入参数
    if path.is_null() {
        return -errno::EFAULT;
    }

    let token = current_user_token();

    // 步骤2: 解析并验证可执行文件路径
    let path_str = match safe_translated_str(token, path, 4096) {
        Ok(s) => s,
        Err(e) => return e,
    };

    if path_str.is_empty() {
        return -errno::ENOENT;
    }

    // 检查路径中是否包含空字符
    if path_str.contains('\0') {
        return -errno::EINVAL;
    }

    // 步骤3: 解析参数数组 argv
    let args = match parse_string_array(token, argv, "arguments") {
        Ok(args) => args,
        Err(e) => return e,
    };

    // 步骤4: 解析环境变量数组 envp
    let envs = match parse_string_array(token, envp, "environment") {
        Ok(envs) => envs,
        Err(e) => return e,
    };

    // 步骤5: 验证参数和环境变量的格式
    for (i, env) in envs.iter().enumerate() {
        if !env.contains('=') {
            error!("execve: invalid environment variable at index {}: {}", i, env);
            return -errno::EINVAL;
        }
    }

    // 步骤6: 获取当前任务
    let current_task = match current_task() {
        Some(task) => task,
        None => return -errno::ESRCH,
    };

    // 步骤7: 查找可执行文件
    let elf_data = match get_app_data_by_name(&path_str) {
        Some(data) => data,
        None => {
            error!("execve: executable not found: {}", path_str);
            return -errno::ENOENT;
        }
    };

    // 步骤8: 在execve过程中防止信号中断
    // 在真实实现中，这里应该阻塞所有信号直到execve完成

    // 步骤9: 执行程序替换
    // 注意：如果成功，这个调用不会返回
    match current_task.execve_replace(&path_str, &elf_data, &args, &envs) {
        Ok(()) => {
            // 如果到达这里，说明实现有问题，因为成功的execve不应该返回
            error!("execve: unexpected return from successful execve");
            -errno::EINVAL
        },
        Err(e) => {
            error!("execve: failed to execute {}: {:?}", path_str, e);
            match e {
                crate::memory::mm::MemoryError::OutOfMemory => -errno::ENOMEM,
                _ => -errno::EINVAL,
            }
        }
    }
}

/// 安全地翻译C字符串，带长度限制
fn safe_translated_str(token: usize, ptr: *const u8, max_len: usize) -> Result<String, isize> {
    if ptr.is_null() {
        return Err(-errno::EFAULT);
    }

    let mut result = String::new();
    let mut addr = ptr as usize;

    for _ in 0..max_len {
        let buffers = translated_byte_buffer(token, addr as *const u8, 1);
        if buffers.is_empty() {
            return Err(-errno::EFAULT);
        }

        let ch = buffers[0][0];
        if ch == 0 {
            break;
        }

        result.push(ch as char);
        addr += 1;
    }

    if result.len() >= max_len {
        return Err(-errno::ENAMETOOLONG);
    }

    Ok(result)
}

/// 解析字符串数组 (argv 或 envp)
fn parse_string_array(token: usize, array_ptr: *const *const u8, array_name: &str) -> Result<Vec<String>, isize> {
    if array_ptr.is_null() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    let mut i = 0;

    // Linux限制：最多256个参数/环境变量
    const MAX_ARRAY_SIZE: usize = 256;
    // Linux限制：单个字符串最大长度
    const MAX_STRING_LEN: usize = 4096;
    // Linux限制：所有字符串总大小
    const MAX_TOTAL_SIZE: usize = 128 * 1024;

    let mut total_size = 0;

    loop {
        if i >= MAX_ARRAY_SIZE {
            error!("execve: too many {} (max {})", array_name, MAX_ARRAY_SIZE);
            return Err(-errno::E2BIG);
        }

        let ptr_addr = array_ptr as usize + i * core::mem::size_of::<*const u8>();

        // 读取字符串指针
        let buffers = translated_byte_buffer(token, ptr_addr as *const u8, core::mem::size_of::<*const u8>());
        if buffers.is_empty() || buffers[0].len() < core::mem::size_of::<*const u8>() {
            break;
        }

        let str_ptr = usize::from_le_bytes([
            buffers[0][0], buffers[0][1], buffers[0][2], buffers[0][3],
            buffers[0][4], buffers[0][5], buffers[0][6], buffers[0][7],
        ]);

        // NULL指针表示数组结束
        if str_ptr == 0 {
            break;
        }

        // 验证指针有效性
        if str_ptr < 0x1000 || str_ptr >= 0x8000_0000_0000_0000 {
            error!("execve: invalid {} pointer at index {}: 0x{:x}", array_name, i, str_ptr);
            return Err(-errno::EFAULT);
        }

        // 解析字符串
        let string = match safe_translated_str(token, str_ptr as *const u8, MAX_STRING_LEN) {
            Ok(s) => s,
            Err(e) => return Err(e),
        };

        total_size += string.len() + 1; // +1 for null terminator
        if total_size > MAX_TOTAL_SIZE {
            error!("execve: total {} size too large (max {})", array_name, MAX_TOTAL_SIZE);
            return Err(-errno::E2BIG);
        }

        result.push(string);
        i += 1;
    }

    Ok(result)
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
    if let Err(e) = crate::arch::sbi::shutdown() {
        warn!("SBI shutdown error: {}", e);
    }
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
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
        let mut pid_buffers =
            translated_byte_buffer(token, pid_ptr as *const u8, core::mem::size_of::<u32>());

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
    let total_cpu_time = task
        .total_cpu_time
        .load(core::sync::atomic::Ordering::Relaxed);
    let creation_time = task
        .creation_time
        .load(core::sync::atomic::Ordering::Relaxed);
    let process_lifetime = if current_time > creation_time {
        current_time - creation_time
    } else {
        1 // 避免除零
    };

    // CPU使用率 = (总CPU时间 / 进程生存时间) * 10000，支持两位小数
    // 限制最大为100% (10000)，避免计算错误导致的异常值
    let cpu_percent = if process_lifetime == 0 {
        0 // 生存时间为0，使用率为0
    } else if total_cpu_time == 0 {
        0 // CPU时间为0，使用率为0
    } else if total_cpu_time >= process_lifetime {
        10000 // CPU时间大于等于生存时间，使用率为100%
    } else {
        // 使用安全的128位运算计算百分比
        let percent_128 = (total_cpu_time as u128 * 10000) / process_lifetime as u128;
        if percent_128 > 10000 {
            10000 // 限制最大为100%
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

    let core_id = {
        use crate::task::TaskStatus as TS;
        match *status {
            TS::Running => {
                if let Some(c) = crate::signal::find_process_core(task.pid()) {
                    c as u32
                } else {
                    task.last_cpu.load(core::sync::atomic::Ordering::Relaxed) as u32
                }
            }
            _ => task.last_cpu.load(core::sync::atomic::Ordering::Relaxed) as u32,
        }
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
            TaskStatus::Stopped => 4,
        },
        priority: sched.priority,
        nice: sched.nice,
        vruntime: sched.vruntime,
        heap_base: task
            .mm
            .heap_base
            .load(core::sync::atomic::Ordering::Relaxed),
        heap_top: task.mm.heap_top.load(core::sync::atomic::Ordering::Relaxed),
        last_runtime: task
            .last_runtime
            .load(core::sync::atomic::Ordering::Relaxed),
        total_cpu_time,
        cpu_percent,
        core_id,
        name: name_bytes,
    };

    // 将信息写入用户空间
    let mut info_buffers = translated_byte_buffer(
        token,
        info as *const u8,
        core::mem::size_of::<ProcessInfo>(),
    );

    if !info_buffers.is_empty() && info_buffers[0].len() >= core::mem::size_of::<ProcessInfo>() {
        let info_bytes = unsafe {
            core::slice::from_raw_parts(
                &process_info as *const ProcessInfo as *const u8,
                core::mem::size_of::<ProcessInfo>(),
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
        total_cpu_user_time += task
            .user_cpu_time
            .load(core::sync::atomic::Ordering::Relaxed);
        total_cpu_kernel_time += task
            .kernel_cpu_time
            .load(core::sync::atomic::Ordering::Relaxed);
    }

    // 计算系统运行时间和CPU使用率
    let current_time = crate::timer::get_time_us();
    let system_uptime = current_time; // 系统运行时间

    // 获取当前激活的核心数量
    let active_cores = crate::task::processor::active_core_count();
    let total_active_cpu_time = total_cpu_user_time + total_cpu_kernel_time;

    // 在多核系统中，总可用CPU时间 = 系统时间 × 核心数
    // CPU使用率 = min(活跃时间 / (系统时间 × 核心数), 1.0) * 100%
    let cpu_usage_percent = if system_uptime == 0 || active_cores == 0 {
        0 // 系统时间或核心数为0，使用率为0
    } else if total_active_cpu_time == 0 {
        0 // 活跃CPU时间为0，使用率为0
    } else {
        let total_available_cpu_time = system_uptime * active_cores as u64;
        if total_active_cpu_time >= total_available_cpu_time {
            10000 // 活跃时间大于等于可用时间，使用率为100%
        } else {
            // 使用安全的128位运算
            let percent_128 =
                (total_active_cpu_time as u128 * 10000) / total_available_cpu_time as u128;
            core::cmp::min(percent_128 as u64, 10000) as u32
        }
    };

    let total_available_cpu_time = system_uptime * active_cores as u64;
    let cpu_idle_time = if total_available_cpu_time > total_active_cpu_time {
        total_available_cpu_time - total_active_cpu_time
    } else {
        0 // 如果活跃时间超过总可用时间，说明计算有误，设为0
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
    let mut stats_buffers = translated_byte_buffer(
        token,
        stats as *const u8,
        core::mem::size_of::<SystemStats>(),
    );

    if !stats_buffers.is_empty() && stats_buffers[0].len() >= core::mem::size_of::<SystemStats>() {
        let stats_bytes = unsafe {
            core::slice::from_raw_parts(
                &system_stats as *const SystemStats as *const u8,
                core::mem::size_of::<SystemStats>(),
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
        active_cores: crate::task::processor::active_core_count() as u32,
    };

    // 将核心信息写入用户空间
    let mut info_buffers = translated_byte_buffer(
        token,
        core_info as *const u8,
        core::mem::size_of::<CpuCoreInfo>(),
    );

    if !info_buffers.is_empty() && info_buffers[0].len() >= core::mem::size_of::<CpuCoreInfo>() {
        let info_bytes = unsafe {
            core::slice::from_raw_parts(
                &cpu_core_info as *const CpuCoreInfo as *const u8,
                core::mem::size_of::<CpuCoreInfo>(),
            )
        };
        info_buffers[0][..core::mem::size_of::<CpuCoreInfo>()].copy_from_slice(info_bytes);
        0
    } else {
        -1
    }
}
