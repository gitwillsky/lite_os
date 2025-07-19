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
        task_manager::{SchedulingPolicy, get_scheduling_policy},
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

// ===== 公共接口函数 =====

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
        .inner_exclusive_access()
        .get_user_token()
}

/// 获取当前任务的陷阱上下文
pub fn current_trap_context() -> &'static mut TrapContext {
    current_task()
        .expect("No current task when getting trap context")
        .inner_exclusive_access()
        .get_trap_cx()
}

/// 获取当前工作目录
pub fn current_cwd() -> String {
    current_task()
        .map(|task| task.inner_exclusive_access().cwd.clone())
        .unwrap_or_else(|| "/".to_string())
}

// ===== 调度相关函数 =====

/// 主调度循环 - 在内核初始化完毕之后进入idle控制流
pub fn run_tasks() -> ! {
    loop {
        let mut processor = PROCESSOR.exclusive_access();

        if let Some(task) = super::task_manager::fetch_task() {
            // 处理信号检查
            if should_handle_signals(&task) {
                drop(processor);
                handle_task_signals(&task);
                continue;
            }

            // 切换到任务
            execute_task(processor, task);
        } else {
            drop(processor);
            // 没有可运行的任务，让出CPU等待中断
            wfi();
        }
    }
}

/// 调度函数 - 切换到idle控制流
pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
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
        let mut task_inner = task.inner_exclusive_access();
        let runtime = end_time.saturating_sub(task_inner.last_runtime);
        let task_cx_ptr = &mut task_inner.task_cx as *mut _;
        let task_status = task_inner.task_status;

        // 更新运行时间统计
        update_task_runtime_stats(&mut task_inner, runtime);

        let should_readd = task_status == TaskStatus::Running;
        if should_readd {
            task_inner.task_status = TaskStatus::Ready;
        }

        (task_cx_ptr, runtime, should_readd)
    };

    // 如果任务应该重新加入就绪队列
    if should_readd {
        super::task_manager::update_task_runtime(&task, runtime);
        super::add_task(task);
    }

    schedule(task_cx_ptr);
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().expect("No current task to block");
    let end_time = get_time_us();

    let (task_cx_ptr, runtime) = {
        let mut task_inner = task.inner_exclusive_access();
        let runtime = end_time.saturating_sub(task_inner.last_runtime);
        let task_cx_ptr = &mut task_inner.task_cx as *mut _;

        task_inner.task_status = TaskStatus::Sleeping;
        update_task_runtime_stats(&mut task_inner, runtime);

        (task_cx_ptr, runtime)
    };

    // 更新任务管理器中的运行时间统计
    super::task_manager::update_task_runtime(&task, runtime);

    // 不将任务加入就绪队列，让它保持阻塞状态
    schedule(task_cx_ptr);
}

/// 退出当前任务并运行下一个任务
pub fn exit_current_and_run_next(exit_code: i32) {
    let task = take_current_task().expect("No current task to exit");
    exit_task_and_run_next(task, exit_code);
}

/// 退出指定任务并运行下一个任务
pub fn exit_task_and_run_next(task: Arc<TaskControlBlock>, exit_code: i32) {
    let pid = task.get_pid();

    // 检查是否是idle进程
    if pid == IDLE_PID {
        debug!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        shutdown();
    }

    // 处理任务退出
    handle_task_exit(&task, exit_code);

    // 调度到下一个任务
    let mut unused_context = TaskContext::zero_init();
    schedule(&mut unused_context as *mut _);
}

/// 无需调度切换的任务退出，用于信号处理等场景
pub fn exit_task_without_schedule(task: Arc<TaskControlBlock>, exit_code: i32) {
    let pid = task.get_pid();

    // 检查是否是idle进程
    if pid == IDLE_PID {
        debug!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        shutdown();
    }

    // 如果要退出的任务就是当前任务，从处理器中移除
    let is_current_task = check_and_remove_current_task(&task);

    // 处理任务退出
    handle_task_exit(&task, exit_code);
}

// ===== 私有辅助函数 =====

/// 检查任务是否有待处理的信号
fn should_handle_signals(task: &Arc<TaskControlBlock>) -> bool {
    task.inner_exclusive_access().has_pending_signals()
}

/// 处理任务信号
fn handle_task_signals(task: &Arc<TaskControlBlock>) {
    let (should_continue, exit_code) = crate::task::check_and_handle_signals();
    if !should_continue {
        if let Some(code) = exit_code {
            // 如果信号要求终止进程，则终止进程
            let mut inner = task.inner_exclusive_access();
            inner.task_status = TaskStatus::Zombie;
            inner.exit_code = code;
        }
    }
}

/// 执行任务切换
fn execute_task(mut processor: impl DerefMut<Target = Processor>, task: Arc<TaskControlBlock>) {
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();

    let next_task_cx_ptr = {
        let mut task_inner = task.inner_exclusive_access();
        let next_task_cx_ptr = &task_inner.task_cx as *const TaskContext;
        task_inner.task_status = TaskStatus::Running;

        // 记录任务开始运行的时间
        let start_time = get_time_us();
        task_inner.last_runtime = start_time;

        next_task_cx_ptr
    };

    processor.current = Some(task);
    drop(processor);

    unsafe {
        __switch(idle_task_cx_ptr, next_task_cx_ptr);
    }
}

/// 更新任务运行时间统计
fn update_task_runtime_stats(
    task_inner: &mut crate::task::task::TaskControlBlockInner,
    runtime: u64,
) {
    match get_scheduling_policy() {
        SchedulingPolicy::CFS => {
            task_inner.update_vruntime(runtime);
        }
        _ => {
            task_inner.last_runtime = runtime;
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
                "[SCHED DEBUG] Kernel alive - scheduling task PID:{}, ready_tasks:{}, time:{}us",
                task.get_pid(),
                super::task_manager::ready_task_count(),
                current_time
            );
        }
    }
}

/// 处理任务退出的核心逻辑
fn handle_task_exit(task: &Arc<TaskControlBlock>, exit_code: i32) {
    let pid = task.get_pid();
    let mut inner = task.inner_exclusive_access();

    inner.task_status = TaskStatus::Zombie;
    inner.exit_code = exit_code;

    // 处理子进程重新父化
    reparent_children_to_init(task, &mut inner);

    // 清理资源
    inner.children.clear();
    inner.close_all_fds_and_cleanup_locks(pid);
    inner.memory_set.recycle_data_pages();
}

/// 将子进程重新父化给init进程
fn reparent_children_to_init(
    task: &Arc<TaskControlBlock>,
    inner: &mut crate::task::task::TaskControlBlockInner,
) {
    let Some(init_proc) = super::task_manager::get_init_proc() else {
        warn!("No init process found for reparenting");
        return;
    };

    // 如果退出的任务就是init进程本身，跳过重新父化过程
    if Arc::ptr_eq(task, &init_proc) {
        debug!("Skipping reparenting because the exiting task is init process itself");
        return;
    }

    // 收集需要重新父化的子进程，避免自引用和重复借用
    let children_to_reparent: Vec<_> = inner
        .children
        .iter()
        .filter(|child| !Arc::ptr_eq(child, &init_proc))
        .cloned()
        .collect();

    if children_to_reparent.is_empty() {
        return;
    }

    // 先处理子进程的parent指针
    for child in &children_to_reparent {
        child.inner_exclusive_access().parent = Some(Arc::downgrade(&init_proc));
    }

    // 然后处理init_proc的children列表
    let mut init_proc_inner = init_proc.inner_exclusive_access();
    for child in children_to_reparent {
        init_proc_inner.children.push(child);
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

// ===== 处理器结构体 =====

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
