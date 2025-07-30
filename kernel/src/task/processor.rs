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
    sync::UPSafeCell,
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

// 使用原子变量替换unsafe的全局变量，提高线程安全性
static LAST_DEBUG_TIME: AtomicU64 = AtomicU64::new(0);

pub const IDLE_PID: usize = 0;
const DEBUG_INTERVAL_US: u64 = 5_000_000; // 5秒调试间隔

/// 获取并移除当前任务
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().exclusive_access().current.take()
}

/// 获取当前任务的引用
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().exclusive_access().current.clone()
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
            let mut processor = current_processor().exclusive_access();
            processor.fetch_task()
        };

        if let Some(task) = task {
            if !task.is_zombie() {
                // 处理信号检查
                if task.signal_state.lock().has_deliverable_signals() {
                    handle_task_signals(&task);
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
                debug!("Core {} stole task PID {} from other core", current_hart, stolen_task.pid());
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
    let mut processor = current_processor().exclusive_access();

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
        let mut processor = current_processor().exclusive_access();
        processor.idle_context_ptr()
    };

    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}

/// 挂起当前任务并运行下一个任务
pub fn suspend_current_and_run_next() {
    let task = take_current_task().expect("No current task to suspend");
    let end_time = get_time_us();
    // 调试信息输出
    // print_debug_info_if_needed(end_time, &task);

    let (task_cx_ptr, runtime, should_readd) = {
        let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));
        let task_cx_ptr = &mut *task.mm.task_cx.lock() as *mut _;

        update_task_runtime_stats(&task, runtime);

        let should_readd = *task.task_status.lock() == TaskStatus::Running;
        if should_readd {
            *task.task_status.lock() = TaskStatus::Ready;
        }

        (task_cx_ptr, runtime, should_readd)
    };

    // 如果任务应该重新加入就绪队列
    if should_readd {
        CORE_MANAGER.add_task(task);
    }

    schedule(task_cx_ptr);
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");
    let end_time = get_time_us();

    let (task_cx_ptr, runtime) = {
        let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));
        update_task_runtime_stats(&task, runtime);
        let task_cx_ptr = &mut *task.mm.task_cx.lock() as *mut _;

        (task_cx_ptr, runtime)
    };

    schedule(task_cx_ptr);
}

/// 退出当前任务并运行下一个任务
pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().expect("No current task to exit");
    let pid = task.pid();

    // 处理任务退出
    task.set_exit_code(exit_code);
    *task.task_status.lock() = TaskStatus::Zombie;

    // 关闭所有文件描述符并清理文件锁
    task.file.lock().close_all_fds_and_cleanup_locks(pid);

    // 将进程挂给 init_proc, 等待回收
    if let Some(init_proc) = task_manager::init_proc() {
        if pid == init_proc.pid() {
            error!("init process exit with exit_code {}", exit_code);
        } else {
            // 收集需要重新父化的子进程
            let mut children_to_reparent: Vec<_> = task
                .children
                .lock()
                .iter()
                .filter(|child| child.pid() != pid)
                .cloned()
                .collect();
            if !children_to_reparent.is_empty() {
                // 先处理子进程的parent指针
                for child in &children_to_reparent {
                    child.set_parent(Arc::downgrade(&init_proc.clone()));
                }
                // 然后处理init_proc的children列表
                let mut init_proc_children = init_proc.children.lock();
                for child in children_to_reparent {
                    init_proc_children.push(child);
                }
            }
        }
    }

    // 唤醒等待的父进程
    if let Some(parent) = task.parent() {
        if *parent.task_status.lock() == TaskStatus::Sleeping {
            // 父进程可能在等待子进程，唤醒它
            parent.wakeup();
        }
    }

    // 调度到下一个任务
    schedule(&mut *task.mm.task_cx.lock() as *mut _);
}

/// 处理任务信号
fn handle_task_signals(task: &Arc<TaskControlBlock>) {
    // 使用安全的信号处理方法，避免获取trap context导致死锁
    use crate::task::signal::SignalDelivery;

    let (should_continue, exit_code) = SignalDelivery::handle_signals_safe(task);

    if !should_continue {
        if let Some(code) = exit_code {
            // 如果信号要求终止进程，则设置为僵尸状态
            debug!("Task {} terminated by signal with exit code {}", task.pid(), code);
            *task.task_status.lock() = TaskStatus::Zombie;
            task.set_exit_code(code);
        }
    }
}

/// 更新任务运行时间统计
fn update_task_runtime_stats(task: &Arc<TaskControlBlock>, runtime: u64) {
    // 更新调度器的虚拟运行时间
    task.sched.lock().update_vruntime(runtime);

    // 注意：不在这里更新CPU时间统计，避免与 mark_kernel_entry/exit 重复计算
    // 用户态/内核态时间的详细统计由 mark_kernel_entry/exit 函数负责
    // 这里只更新调度器需要的虚拟运行时间
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

/// 检查并移除当前任务（如果匹配）
fn check_and_remove_current_task(task: &Arc<TaskControlBlock>) -> bool {
    let is_current_task = {
        let processor = current_processor().exclusive_access();
        processor
            .current
            .as_ref()
            .map(|current| Arc::ptr_eq(current, task))
            .unwrap_or(false)
    };

    if is_current_task {
        take_current_task();
    }

    is_current_task
}

