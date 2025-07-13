use alloc::{sync::Arc, vec::Vec, string::{String, ToString}};

use crate::{
    loader::get_app_data_by_name,
    memory::page_table::{translated_ref_mut, translated_str, translated_byte_buffer},
    task::{
        self, current_task, current_user_token, exit_current_and_run_next,
        suspend_current_and_run_next, set_scheduling_policy, get_scheduling_policy, SchedulingPolicy,
    },
};

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    unreachable!()
}

pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().get_pid() as isize
}

pub fn sys_fork() -> isize {
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.get_pid();

    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();

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
        task.exec(&elf_data);
        0
    } else {
        -1
    }
}

pub fn sys_wait_pid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    let task = current_task().unwrap();

    let mut inner = task.inner_exclusive_access();

    if inner
        .children
        .iter()
        .find(|p| pid == -1 || pid as usize == p.get_pid())
        .is_none()
    {
        return -1;
    }

    let pair = inner.children.iter().enumerate().find(|(_, t)| {
        t.inner_exclusive_access().is_zombie() && (pid == -1 || t.get_pid() == pid as usize)
    });

    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        assert_eq!(
            Arc::strong_count(&child),
            1,
            "Leaked Arc reference to child process!"
        );
        let found_pid = child.get_pid();
        let exit_code = child.inner_exclusive_access().exit_code;
        let parent_token = inner.get_user_token();
        *translated_ref_mut(parent_token, exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
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
        let mut inner = task.inner_exclusive_access();
        inner.set_nice(prio);
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
        let inner = task.inner_exclusive_access();
        inner.nice as isize
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
    let token = current_user_token();
    let path_str = translated_str(token, path);
    
    // Parse argv
    let mut args = Vec::new();
    if !argv.is_null() {
        let mut i = 0;
        loop {
            let arg_ptr_addr = argv as usize + i * core::mem::size_of::<*const u8>();
            let buffers = translated_byte_buffer(token, arg_ptr_addr as *const u8, core::mem::size_of::<*const u8>());
            if buffers.is_empty() || buffers[0].len() < core::mem::size_of::<*const u8>() {
                break;
            }
            
            let arg_ptr = usize::from_le_bytes([
                buffers[0][0], buffers[0][1], buffers[0][2], buffers[0][3],
                buffers[0][4], buffers[0][5], buffers[0][6], buffers[0][7],
            ]);
            
            if arg_ptr == 0 {
                break;
            }
            
            let arg_str = translated_str(token, arg_ptr as *const u8);
            args.push(arg_str);
            i += 1;
            
            // Prevent infinite loops
            if i > 1024 {
                return -1;
            }
        }
    }
    
    // Parse envp
    let mut envs = Vec::new();
    if !envp.is_null() {
        let mut i = 0;
        loop {
            let env_ptr_addr = envp as usize + i * core::mem::size_of::<*const u8>();
            let buffers = translated_byte_buffer(token, env_ptr_addr as *const u8, core::mem::size_of::<*const u8>());
            if buffers.is_empty() || buffers[0].len() < core::mem::size_of::<*const u8>() {
                break;
            }
            
            let env_ptr = usize::from_le_bytes([
                buffers[0][0], buffers[0][1], buffers[0][2], buffers[0][3],
                buffers[0][4], buffers[0][5], buffers[0][6], buffers[0][7],
            ]);
            
            if env_ptr == 0 {
                break;
            }
            
            let env_str = translated_str(token, env_ptr as *const u8);
            envs.push(env_str);
            i += 1;
            
            // Prevent infinite loops
            if i > 1024 {
                return -1;
            }
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
        match task.exec_with_args(&elf_data, &args, &envs) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    } else {
        -1
    }
}

pub fn sys_shutdown() -> ! {
    crate::arch::sbi::shutdown();
    unreachable!();
}
