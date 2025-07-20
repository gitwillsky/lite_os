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
    arch::sbi::shutdown,
    sync::UPSafeCell,
    task::{
        __switch,
        context::TaskContext,
        task::{TaskControlBlock, TaskStatus},
        task_manager::{self, SchedulingPolicy, get_scheduling_policy},
    },
    timer::get_time_us,
    trap::TrapContext,
};

lazy_static! {
    static ref PROCESSOR: UPSafeCell<Processor> = UPSafeCell::new(Processor::new());
}

// 使用原子变量替换unsafe的全局变量，提高线程安全性
static LAST_DEBUG_TIME: AtomicU64 = AtomicU64::new(0);

pub const IDLE_PID: usize = 0;
const DEBUG_INTERVAL_US: u64 = 5_000_000; // 5秒调试间隔

/// 获取并移除当前任务
pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().take_current()
}

/// 获取当前任务的引用
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().current()
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

/// 主调度循环 - 在内核初始化完毕之后进入idle控制流
pub fn run_tasks() -> ! {
    loop {
        if let Some(task) = task_manager::fetch_task() {
            // 处理信号检查
            if task.signal_state.lock().has_deliverable_signals() {
                handle_task_signals(&task);
                continue;
            }
            // 切换到任务
            let mut processor = PROCESSOR.exclusive_access();

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
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            drop(processor);

            // 当前的 run_tasks 函数是 idle 任务，切换到下一个任务
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
        } else {
            // 没有可运行的任务，让出CPU等待中断
            wfi();
        }
    }
}

/// 调度函数 - 切换到idle控制流
fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let idle_task_cx_ptr = {
        let mut processor = PROCESSOR.exclusive_access();
        processor.get_idle_task_cx_ptr()
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
    print_debug_info_if_needed(end_time, &task);

    let (task_cx_ptr, runtime, should_readd) = {
        let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));
        let task_cx_ptr = &mut *task.mm.task_cx.lock() as *mut _;
        let task_status = *task.task_status.lock();

        // 更新运行时间统计
        update_task_runtime_stats(&task, runtime);

        let should_readd = task_status == TaskStatus::Running;
        if should_readd {
            *task.task_status.lock() = TaskStatus::Ready;
        }

        (task_cx_ptr, runtime, should_readd)
    };

    // 如果任务应该重新加入就绪队列
    if should_readd {
        task.sched.lock().update_vruntime(runtime);
        super::add_task(task);
    }

    schedule(task_cx_ptr);
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");
    let end_time = get_time_us();

    let (task_cx_ptr, runtime) = {
        let runtime = end_time.saturating_sub(task.last_runtime.load(Ordering::Relaxed));
        *task.task_status.lock() = TaskStatus::Sleeping;
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

    // 将进程挂给 init_proc, 等待回收
    if let Some(init_proc) = task_manager::get_init_proc() {
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

    // 清理资源
    task.children.lock().clear();
    task.file.lock().close_all_fds_and_cleanup_locks(pid);
    task.mm.memory_set.lock().recycle_data_pages();

    // 调度到下一个任务
    schedule(&mut *task.mm.task_cx.lock() as *mut _);
}

/// 处理任务信号
fn handle_task_signals(task: &Arc<TaskControlBlock>) {
    let (should_continue, exit_code) = crate::task::check_and_handle_signals();
    if !should_continue {
        if let Some(code) = exit_code {
            // 如果信号要求终止进程，则终止进程
            *task.task_status.lock() = TaskStatus::Zombie;
            task.set_exit_code(code);
        }
    }
}

/// 更新任务运行时间统计
fn update_task_runtime_stats(task: &Arc<TaskControlBlock>, runtime: u64) {
    task.sched.lock().update_vruntime(runtime);
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
                "[SCHED DEBUG] Kernel alive - scheduling task PID:{}, ready_tasks:{}, time:{}us",
                task.pid(),
                super::task_manager::ready_task_count(),
                current_time
            );
        }
    }
}

/// 检查并移除当前任务（如果匹配）
fn check_and_remove_current_task(task: &Arc<TaskControlBlock>) -> bool {
    let is_current_task = {
        let processor = PROCESSOR.exclusive_access();
        processor
            .current()
            .map(|current| Arc::ptr_eq(&current, task))
            .unwrap_or(false)
    };

    if is_current_task {
        take_current_task();
    }

    is_current_task
}

/// 描述CPU执行状态
struct Processor {
    /// 当前正在执行的任务
    current: Option<Arc<TaskControlBlock>>,
    /// 当前处理器上idle任务的上下文
    idle_task_cx: TaskContext,
}

impl Processor {
    /// 创建新的处理器实例
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }

    /// 获取并移除当前任务
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }

    /// 获取当前任务的引用
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }

    /// 获取idle任务上下文的可变指针
    pub fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_cx
    }
}

impl Default for Processor {
    fn default() -> Self {
        Self::new()
    }
}
