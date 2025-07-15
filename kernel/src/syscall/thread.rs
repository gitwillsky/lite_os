use crate::{
    memory::{
        self,
        thread_safe::{register_current_thread, unregister_current_thread},
    },
    task::{current_task, suspend_current_and_run_next},
    thread::{
        ThreadId, create_thread as kernel_create_thread, exit_thread as kernel_exit_thread,
        get_sync_manager, join_thread as kernel_join_thread, send_signal_to_thread,
    },
};

/// 线程创建参数结构
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ThreadAttr {
    pub stack_size: usize,
    pub detached: bool,
    pub priority: i32,
}

/// 从用户空间安全读取线程属性
fn read_thread_attr_from_user(attr_ptr: *const ThreadAttr) -> Result<ThreadAttr, &'static str> {
    if attr_ptr.is_null() {
        return Err("Null pointer");
    }

    // 检查地址是否在用户空间范围内
    let addr = attr_ptr as usize;
    if addr < 0x10000 || addr >= 0x80000000 {
        return Err("Invalid user address");
    }

    // 获取当前任务的页表token进行地址转换
    if let Some(current_task) = current_task() {
        let token = current_task.inner_exclusive_access().get_user_token();

        // 使用页表转换安全地读取用户数据
        let attr_ref = memory::page_table::translated_ref_mut(token, attr_ptr as *mut ThreadAttr);
        Ok(*attr_ref)
    } else {
        Err("No current task")
    }
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

    {
        // 获取当前任务
        let current_task = match current_task() {
            Some(task) => task,
            None => return -1,
        };

        // 确保当前任务支持多线程
        current_task.init_thread_manager();
    }

    // 解析线程属性
    let (stack_size, joinable, priority) = if attr_ptr.is_null() {
        (8192, true, 0) // 默认值：8KB栈，可join，默认优先级
    } else {
        // 从用户空间安全地读取属性
        match read_thread_attr_from_user(attr_ptr) {
            Ok(attr) => {
                let stack_size = if attr.stack_size > 0 {
                    attr.stack_size.max(4096) // 最小4KB栈
                } else {
                    8192
                };
                let joinable = !attr.detached;
                let priority = attr.priority.max(-20).min(19); // 限制优先级范围
                (stack_size, joinable, priority)
            }
            Err(_) => {
                return -1; // 无效的属性指针
            }
        }
    };

    // 创建线程
    match kernel_create_thread(entry_point, stack_size, arg, joinable) {
        Ok(thread_id) => {
            // 注册线程到内存管理器
            register_current_thread(thread_id, Some(1024 * 1024)); // 1MB内存限制

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

/// 安全地将退出码写入用户空间
fn write_exit_code_to_user(exit_code_ptr: *mut i32, exit_code: i32) -> Result<(), &'static str> {
    if exit_code_ptr.is_null() {
        return Err("Null pointer");
    }

    // 检查地址是否在用户空间范围内
    let addr = exit_code_ptr as usize;
    if addr < 0x10000 || addr >= 0x80000000 {
        return Err("Invalid user address");
    }

    // 获取当前任务的页表token进行地址转换
    if let Some(current_task) = current_task() {
        let task_inner = current_task.inner_exclusive_access();
        let token = task_inner.get_user_token();
        drop(task_inner);

        // 使用页表转换安全地写入用户数据
        let exit_code_ref = crate::memory::page_table::translated_ref_mut(token, exit_code_ptr);
        *exit_code_ref = exit_code;
        Ok(())
    } else {
        Err("No current task")
    }
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
                // 安全地写入用户空间
                match write_exit_code_to_user(exit_code_ptr, exit_code) {
                    Ok(()) => {}
                    Err(_) => return -1, // EFAULT
                }
            }

            // 注销线程的内存管理
            unregister_current_thread(target_thread_id);

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
