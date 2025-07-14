use crate::{
    task::{current_task, suspend_current_and_run_next},
    thread::{
        create_thread as kernel_create_thread,
        exit_thread as kernel_exit_thread,
        join_thread as kernel_join_thread,
        get_sync_manager,
        ThreadId,
        send_signal_to_thread,
    },
    memory::thread_safe::{register_current_thread, unregister_current_thread},
};

/// 线程创建参数结构
#[repr(C)]
#[derive(Debug)]
pub struct ThreadAttr {
    pub stack_size: usize,
    pub detached: bool,
    pub priority: i32,
}

/// 创建线程系统调用
/// args[0]: 线程入口点函数地址
/// args[1]: 线程参数
/// args[2]: 线程属性 (可选，为空则使用默认值)
/// 返回值: 线程ID，或错误码
pub fn sys_thread_create(entry_point: usize, arg: usize, attr_ptr: *const ThreadAttr) -> isize {
    if entry_point == 0 {
        return -1; // 无效的入口点
    }

    // 获取当前任务
    let current_task = match current_task() {
        Some(task) => task,
        None => return -1,
    };

    // 确保当前任务支持多线程
    current_task.init_thread_manager();

    // 解析线程属性
    let (stack_size, joinable, _priority) = if attr_ptr.is_null() {
        (8192, true, 0) // 默认值：8KB栈，可join，默认优先级
    } else {
        // 这里应该从用户空间安全地读取属性
        // 简化处理，使用默认值
        (8192, true, 0)
    };

    // 创建线程
    match kernel_create_thread(entry_point, stack_size, arg, joinable) {
        Ok(thread_id) => {
            // 注册线程到内存管理器
            register_current_thread(thread_id, Some(1024 * 1024)); // 1MB内存限制
            
            info!("Created thread {} with entry point {:#x}", thread_id.0, entry_point);
            thread_id.0 as isize
        }
        Err(_) => -1,
    }
}

/// 线程退出系统调用
/// args[0]: 退出码
pub fn sys_thread_exit(exit_code: i32) -> ! {
    // 获取当前线程ID（如果有的话）
    if let Some(current_task) = current_task() {
        let task_inner = current_task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
            if let Some(current_thread) = thread_manager.get_current_thread() {
                let thread_id = current_thread.get_thread_id();
                drop(task_inner);
                
                // 注销线程的内存管理
                unregister_current_thread(thread_id);
                
                // 调用内核线程退出
                kernel_exit_thread(exit_code);
            }
        }
    }
    
    // 如果没有线程管理器，则退出进程
    crate::task::exit_current_and_run_next(exit_code);
}

/// 等待线程结束系统调用
/// args[0]: 目标线程ID
/// args[1]: 接收退出码的指针 (可选)
pub fn sys_thread_join(thread_id: usize, exit_code_ptr: *mut i32) -> isize {
    if thread_id == 0 {
        return -1; // 无效的线程ID
    }

    let target_thread_id = ThreadId(thread_id);
    
    match kernel_join_thread(target_thread_id) {
        Ok(exit_code) => {
            // 如果提供了退出码指针，将退出码写入用户空间
            if !exit_code_ptr.is_null() {
                // 这里应该安全地写入用户空间
                // 简化处理，假设指针有效
                unsafe {
                    *exit_code_ptr = exit_code;
                }
            }
            
            // 注销线程的内存管理
            unregister_current_thread(target_thread_id);
            
            info!("Thread {} joined successfully with exit code {}", thread_id, exit_code);
            0 // 成功
        }
        Err(_) => -1, // 失败
    }
}

/// 线程让步系统调用
pub fn sys_thread_yield() -> isize {
    // 使用进程级别的让步
    suspend_current_and_run_next();
    0
}

/// 获取当前线程ID系统调用
pub fn sys_get_thread_id() -> isize {
    if let Some(current_task) = current_task() {
        let task_inner = current_task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
            if let Some(current_thread) = thread_manager.get_current_thread() {
                return current_thread.get_thread_id().0 as isize;
            }
        }
    }
    
    // 如果没有线程管理器，返回进程ID
    if let Some(current_task) = current_task() {
        current_task.get_pid() as isize
    } else {
        -1
    }
}

/// 设置线程私有数据系统调用
/// args[0]: 数据值
pub fn sys_set_thread_local(data: usize) -> isize {
    if let Some(current_task) = current_task() {
        let task_inner = current_task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
            if let Some(current_thread) = thread_manager.get_current_thread() {
                current_thread.set_thread_local_data(data);
                return 0; // 成功
            }
        }
    }
    -1 // 失败
}

/// 获取线程私有数据系统调用
pub fn sys_get_thread_local() -> isize {
    if let Some(current_task) = current_task() {
        let task_inner = current_task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
            if let Some(current_thread) = thread_manager.get_current_thread() {
                if let Some(data) = current_thread.get_thread_local_data() {
                    return data as isize;
                }
            }
        }
    }
    0 // 返回0表示没有设置私有数据
}

/// 互斥锁创建系统调用
pub fn sys_mutex_create() -> isize {
    let mut sync_manager = get_sync_manager();
    let mutex_id = sync_manager.create_mutex();
    debug!("Created mutex with ID {}", mutex_id);
    mutex_id as isize
}

/// 互斥锁加锁系统调用
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    let sync_manager = get_sync_manager();
    if let Some(mutex) = sync_manager.get_mutex(mutex_id) {
        drop(sync_manager);
        let _guard = mutex.lock();
        debug!("Mutex {} locked", mutex_id);
        0
    } else {
        -1
    }
}

/// 互斥锁解锁系统调用
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    let sync_manager = get_sync_manager();
    if let Some(mutex) = sync_manager.get_mutex(mutex_id) {
        drop(sync_manager);
        mutex.unlock();
        debug!("Mutex {} unlocked", mutex_id);
        0
    } else {
        -1
    }
}

/// 条件变量创建系统调用
pub fn sys_condvar_create() -> isize {
    let mut sync_manager = get_sync_manager();
    let condvar_id = sync_manager.create_condvar();
    debug!("Created condvar with ID {}", condvar_id);
    condvar_id as isize
}

/// 条件变量等待系统调用
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    let sync_manager = get_sync_manager();
    if let Some(condvar) = sync_manager.get_condvar(condvar_id) {
        if let Some(mutex) = sync_manager.get_mutex(mutex_id) {
            drop(sync_manager);
            let guard = mutex.lock();
            let _guard = condvar.wait(guard);
            debug!("Condvar {} wait completed with mutex {}", condvar_id, mutex_id);
            0
        } else {
            -1
        }
    } else {
        -1
    }
}

/// 条件变量通知系统调用
pub fn sys_condvar_notify(condvar_id: usize, notify_all: bool) -> isize {
    let sync_manager = get_sync_manager();
    if let Some(condvar) = sync_manager.get_condvar(condvar_id) {
        drop(sync_manager);
        if notify_all {
            condvar.notify_all();
            debug!("Condvar {} notify_all", condvar_id);
        } else {
            condvar.notify_one();
            debug!("Condvar {} notify_one", condvar_id);
        }
        0
    } else {
        -1
    }
}

/// 信号量创建系统调用
pub fn sys_semaphore_create(initial_count: usize) -> isize {
    let mut sync_manager = get_sync_manager();
    let sem_id = sync_manager.create_semaphore(initial_count);
    debug!("Created semaphore with ID {} and count {}", sem_id, initial_count);
    sem_id as isize
}

/// 信号量等待系统调用
pub fn sys_semaphore_wait(sem_id: usize) -> isize {
    let sync_manager = get_sync_manager();
    if let Some(semaphore) = sync_manager.get_semaphore(sem_id) {
        drop(sync_manager);
        semaphore.wait();
        debug!("Semaphore {} wait completed", sem_id);
        0
    } else {
        -1
    }
}

/// 信号量释放系统调用
pub fn sys_semaphore_signal(sem_id: usize) -> isize {
    let sync_manager = get_sync_manager();
    if let Some(semaphore) = sync_manager.get_semaphore(sem_id) {
        drop(sync_manager);
        semaphore.signal();
        debug!("Semaphore {} signal completed", sem_id);
        0
    } else {
        -1
    }
}

/// 线程信号发送系统调用
pub fn sys_thread_kill(thread_id: usize, signal: u32) -> isize {
    use crate::task::signal::Signal;
    
    if let Some(signal_enum) = Signal::from_u8(signal as u8) {
        if let Some(current_task) = current_task() {
            let target_thread_id = ThreadId(thread_id);
            
            match send_signal_to_thread(&current_task, target_thread_id, signal_enum) {
                Ok(()) => {
                    info!("Signal {} sent to thread {}", signal, thread_id);
                    0
                }
                Err(_) => -1,
            }
        } else {
            -1
        }
    } else {
        -1 // 无效的信号
    }
}