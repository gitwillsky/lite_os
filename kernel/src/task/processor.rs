use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    ops::DerefMut,
    sync::atomic::{AtomicU64, Ordering},
};
use lazy_static::lazy_static;
use riscv::asm::wfi;

use crate::{
    arch::{sbi::shutdown, hart::hart_id},
    task::{
        __switch,
        context::TaskContext,
        multicore::{current_processor, CORE_MANAGER},
        task::{TaskControlBlock, TaskStatus},
        task_manager::{self, SchedulingPolicy, get_scheduling_policy},
    },
    timer::get_time_us,
    trap::TrapContext,
};

// =============================================================================
// 任务处理器 - 负责任务调度、切换和生命周期管理
//
// 主要功能：
// - 任务调度循环
// - 任务切换和上下文管理
// - 进程退出和僵尸进程清理
// - 信号处理
// - CPU时间统计
// =============================================================================

// =============================================================================
// 常量和静态变量
// =============================================================================

pub const IDLE_PID: usize = 0;
const DEBUG_INTERVAL_US: u64 = 5_000_000; // 5秒调试间隔

/// 使用原子变量替换unsafe的全局变量，提高线程安全性
static LAST_DEBUG_TIME: AtomicU64 = AtomicU64::new(0);

// =============================================================================
// 当前任务管理
// =============================================================================

/// 获取并移除当前任务
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().lock().current.take()
}

/// 获取当前任务的引用
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().lock().current.clone()
}

/// 获取当前任务的用户空间页表令牌
pub fn current_user_token() -> usize {
    current_task()
        .expect("No current task when getting user token")
        .mm
        .memory_set
        .lock()
        .token()
}

/// 获取当前任务的陷阱上下文
pub fn current_trap_context() -> &'static mut TrapContext {
    current_task()
        .expect("No current task when getting trap context")
        .mm
        .trap_context()
}

/// 获取当前工作目录
pub fn current_cwd() -> String {
    current_task()
        .map(|task| task.cwd.lock().clone())
        .unwrap_or_else(|| "/".to_string())
}

// =============================================================================
// 任务调度和切换
// =============================================================================

/// 主调度循环 - 多核心版本
pub fn run_tasks() -> ! {
    let current_hart = hart_id();
    debug!("Core {} entering scheduling loop", current_hart);

    // 每隔一段时间打印调试信息
    let mut debug_counter = 0u64;

    loop {
        // 在主调度循环中喂狗，表明系统正常运行
        if let Err(_) = crate::watchdog::feed() {
            // Watchdog 可能被禁用，这是正常的
        }

        // 1. 尝试从本地调度器获取任务
        let task = {
            let mut processor = current_processor().lock();
            processor.fetch_task()
        };

        if let Some(task) = task {
            if !task.is_zombie() {
                // 处理信号检查
                if task.signal_state.lock().has_deliverable_signals() {
                    if !handle_task_signals(&task) {
                        // 信号处理后任务不应该继续调度（可能被终止或停止）
                        continue;
                    }
                }

                // 检查任务是否处于睡眠状态
                if *task.task_status.lock() == TaskStatus::Sleeping {
                    // 睡眠状态的任务不应该被调度，跳过
                    continue;
                }

                // 切换到任务
                switch_to_task(task);
                continue;
            }
        }

        // 2. 尝试工作窃取
        if let Some(stolen_task) = CORE_MANAGER.steal_work(current_hart) {
            if !stolen_task.is_zombie() {
                // 检查被窃取的任务是否处于睡眠状态
                if *stolen_task.task_status.lock() == TaskStatus::Sleeping {
                    // 睡眠状态的任务不应该被调度，跳过
                    continue;
                }
                
                switch_to_task(stolen_task);
                continue;
            }
        }

        // 3. 没有任务，进入空闲状态
        wfi();
    }
}

/// 切换到指定任务
fn switch_to_task(task: Arc<TaskControlBlock>) {
    let mut processor = current_processor().lock();

    let next_task_cx_ptr = {
        let task_context = task.mm.task_cx.lock();
        let next_task_cx_ptr = &*task_context as *const TaskContext;
        *task.task_status.lock() = TaskStatus::Running;

        // 记录任务开始运行的时间
        let start_time = get_time_us();
        task.last_runtime.store(start_time, Ordering::Relaxed);

        next_task_cx_ptr
    };

    processor.current = Some(task.clone());
    let idle_task_cx_ptr = processor.idle_context_ptr();
    drop(processor);

    // 切换到任务
    unsafe {
        __switch(idle_task_cx_ptr, next_task_cx_ptr);
    }
}

/// 调度函数 - 切换到idle控制流
fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let idle_task_cx_ptr = {
        let mut processor = current_processor().lock();
        processor.idle_context_ptr()
    };

    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}

/// 挂起当前任务并运行下一个任务
pub fn suspend_current_and_run_next() {
    let task = take_current_task().expect("No current task to suspend");
    let task_cx_ptr = prepare_task_for_suspend(&task);
    
    // 如果任务应该重新加入就绪队列
    let should_readd = *task.task_status.lock() == TaskStatus::Running;
    if should_readd {
        *task.task_status.lock() = TaskStatus::Ready;
        CORE_MANAGER.add_task(task);
    }

    schedule(task_cx_ptr);
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");
    let task_cx_ptr = prepare_task_for_suspend(&task);
    schedule(task_cx_ptr);
}

/// 退出当前任务并运行下一个任务
pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().expect("No current task to exit");
    
    // 执行完整的任务清理
    perform_task_exit_cleanup(&task, exit_code, false);
    
    // 调度到下一个任务
    schedule(&mut *task.mm.task_cx.lock() as *mut _);
}

/// 处理任务信号
/// 返回是否应该继续调度这个任务
fn handle_task_signals(task: &Arc<TaskControlBlock>) -> bool {
    use crate::task::signal::SignalDelivery;

    let (should_continue, exit_code) = SignalDelivery::handle_signals_safe(task);

    if !should_continue {
        if let Some(code) = exit_code {
            debug!("Task {} terminated by signal with exit code {}", task.pid(), code);
            // 执行任务清理，但不调度（因为我们在调度循环中）
            perform_task_exit_cleanup(task, code, true);
        }
        return false; // 不应该继续调度
    }
    
    // 检查任务是否被信号停止（例如 SIGTSTP/Ctrl+Z）
    if *task.task_status.lock() == TaskStatus::Stopped {
        debug!("Task {} was stopped by signal", task.pid());
        return false; // 被停止的任务不应该被调度
    }
    
    true // 可以继续调度
}

/// 统一的任务退出清理函数
/// 
/// # 参数
/// - task: 要清理的任务
/// - exit_code: 退出码
/// - from_signal: 是否来自信号终止（影响父子关系处理）
fn perform_task_exit_cleanup(task: &Arc<TaskControlBlock>, exit_code: i32, from_signal: bool) {
    let pid = task.pid();
    
    // 设置退出状态
    task.set_exit_code(exit_code);
    *task.task_status.lock() = TaskStatus::Zombie;

    // 关闭所有文件描述符并清理文件锁
    task.file.lock().close_all_fds_and_cleanup_locks(pid);

    // 重新父化子进程到init进程
    reparent_children_to_init(task);
    
    // 处理父子关系
    handle_parent_child_relationship(task, from_signal);
}

/// 将进程的子进程重新父化给init进程
fn reparent_children_to_init(task: &Arc<TaskControlBlock>) {
    let pid = task.pid();
    
    if let Some(init_proc) = task_manager::init_proc() {
        if pid == init_proc.pid() {
            error!("init process exit with exit_code {}", task.exit_code());
            return;
        }
        
        let children_to_reparent: Vec<_> = task
            .children
            .lock()
            .iter()
            .filter(|child| child.pid() != pid)
            .cloned()
            .collect();
            
        if !children_to_reparent.is_empty() {
            // 设置子进程的新父进程
            for child in &children_to_reparent {
                child.set_parent(Arc::downgrade(&init_proc));
            }
            
            // 将子进程添加到init进程的子进程列表
            let mut init_children = init_proc.children.lock();
            for child in children_to_reparent {
                init_children.push(child);
            }
        }
    }
}

/// 处理进程退出时的父子关系
fn handle_parent_child_relationship(task: &Arc<TaskControlBlock>, from_signal: bool) {
    let Some(parent) = task.parent() else {
        return;
    };
    
    let pid = task.pid();
    
    // 如果是信号终止，需要从父进程的子进程列表中移除并转移给init
    if from_signal {
        let removed = remove_from_parent_children(&parent, task);
        
        if removed {
            transfer_to_init_if_needed(task, &parent);
        }
    }
    
    // 唤醒等待的父进程
    wake_waiting_parent(&parent);
}

/// 从父进程的子进程列表中移除指定任务
fn remove_from_parent_children(parent: &Arc<TaskControlBlock>, task: &Arc<TaskControlBlock>) -> bool {
    let mut parent_children = parent.children.lock();
    if let Some(pos) = parent_children.iter().position(|child| Arc::ptr_eq(child, task)) {
        parent_children.remove(pos);
        debug!("Removed zombie process {} from parent {} children list", task.pid(), parent.pid());
        true
    } else {
        false
    }
}

/// 如果需要，将任务转移给init进程
fn transfer_to_init_if_needed(task: &Arc<TaskControlBlock>, parent: &Arc<TaskControlBlock>) {
    let Some(init_proc) = task_manager::init_proc() else {
        return;
    };
    
    let pid = task.pid();
    
    // 只有当父进程不是init进程时才转移
    if pid != init_proc.pid() && parent.pid() != init_proc.pid() {
        init_proc.children.lock().push(task.clone());
        task.set_parent(Arc::downgrade(&init_proc));
        debug!("Transferred zombie process {} to init process", pid);
    }
}

/// 唤醒等待的父进程
fn wake_waiting_parent(parent: &Arc<TaskControlBlock>) {
    if *parent.task_status.lock() == TaskStatus::Sleeping {
        parent.wakeup();
    }
}

/// 准备任务以便挂起（统一处理运行时间统计）
/// 返回任务上下文指针
fn prepare_task_for_suspend(task: &Arc<TaskControlBlock>) -> *mut TaskContext {
    let end_time = get_time_us();
    let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));
    
    // 更新调度器的虚拟运行时间
    task.sched.lock().update_vruntime(runtime);
    
    &mut *task.mm.task_cx.lock() as *mut _
}

/// 标记进程进入内核态
pub fn mark_kernel_entry() {
    if let Some(task) = current_task() {
        let current_time = get_time_us();
        let mut in_kernel = task.in_kernel_mode.lock();

        // 如果之前在用户态，计算用户态时间
        if !*in_kernel {
            let last_runtime = task.last_runtime.load(Ordering::Relaxed);
            if current_time > last_runtime {
                let user_time = current_time - last_runtime;
                task.user_cpu_time.fetch_add(user_time, Ordering::Relaxed);
                task.total_cpu_time.fetch_add(user_time, Ordering::Relaxed);
            }

            // 记录进入内核态的时间
            task.kernel_enter_time.store(current_time, Ordering::Relaxed);
            *in_kernel = true;
        }
    }
}

/// 标记进程退出内核态
pub fn mark_kernel_exit() {
    if let Some(task) = current_task() {
        let current_time = get_time_us();
        let mut in_kernel = task.in_kernel_mode.lock();

        // 如果之前在内核态，计算内核态时间
        if *in_kernel {
            let kernel_enter_time = task.kernel_enter_time.load(Ordering::Relaxed);
            if current_time > kernel_enter_time {
                let kernel_time = current_time - kernel_enter_time;
                task.kernel_cpu_time.fetch_add(kernel_time, Ordering::Relaxed);
                task.total_cpu_time.fetch_add(kernel_time, Ordering::Relaxed);
            }

            // 更新最后运行时间为退出内核态的时间
            task.last_runtime.store(current_time, Ordering::Relaxed);
            *in_kernel = false;
        }
    }
}

/// 如果需要则打印调试信息（每5秒一次）
#[allow(dead_code)]
fn print_debug_info_if_needed(current_time: u64, task: &Arc<TaskControlBlock>) {
    let last_time = LAST_DEBUG_TIME.load(Ordering::Relaxed);
    if current_time.saturating_sub(last_time) >= DEBUG_INTERVAL_US {
        if LAST_DEBUG_TIME
            .compare_exchange_weak(
                last_time,
                current_time,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            debug!(
                "[SCHED DEBUG] Kernel alive - scheduling task: {:?}, schedulable_tasks:{}, time:{}us",
                &task,
                super::task_manager::schedulable_task_count(),
                current_time
            );
        }
    }
}

