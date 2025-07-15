use alloc::{string::{String, ToString}, sync::Arc};
use lazy_static::lazy_static;
use riscv::asm::wfi;

use crate::{
    arch::sbi::shutdown,
    sync::UPSafeCell,
    task::{
        __switch,
        TaskContext,
        task::{TaskControlBlock, TaskStatus},
        task_manager::{SchedulingPolicy, get_scheduling_policy},
    },
    trap::TrapContext,
    timer::get_time_us,
};

lazy_static! {
    static ref PROCESSOR: UPSafeCell<Processor> = UPSafeCell::new(Processor::new());
}

pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().take_current()
}

pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.exclusive_access().current()
}

pub fn current_user_token() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .get_user_token()
}

pub fn current_trap_context() -> &'static mut TrapContext {
    if let Some(task) = current_task() {
        let task_inner = task.inner_exclusive_access();

        // 检查是否有线程管理器（多线程进程）
        if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
            if let Some(current_thread) = thread_manager.get_current_thread() {
                // 返回当前线程的陷入上下文
                return current_thread.get_trap_cx();
            }
        }

        // 单线程进程的陷入上下文
        task_inner.get_trap_cx()
    } else {
        panic!("No current task");
    }
}

/// 在内核初始化完毕之后，会通过调用 run_tasks 函数来进入 idle 控制流
pub fn run_tasks() -> ! {
    loop {
        let mut processor = PROCESSOR.exclusive_access();
        if let Some(task) = super::task_manager::fetch_task() {
            // 在运行任务前检查信号
            {
                let inner = task.inner_exclusive_access();
                if inner.has_pending_signals() {
                    drop(inner);
                    drop(processor);
                    // 如果有待处理的信号，让任务先处理信号
                    let (should_continue, exit_code) = crate::task::check_and_handle_signals();
                    if !should_continue {
                        if let Some(code) = exit_code {
                            // 如果信号要求终止进程，则终止进程
                            let mut inner = task.inner_exclusive_access();
                            inner.sched.task_status = TaskStatus::Zombie;
                            inner.process.exit_code = code;
                            drop(inner);
                            continue;
                        }
                    }
                    // 重新获取processor锁
                    processor = PROCESSOR.exclusive_access();
                }
            }

            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            let mut task_inner = task.inner_exclusive_access();
            let next_task_cx_ptr = &task_inner.sched.task_cx as *const TaskContext;
            task_inner.sched.task_status = TaskStatus::Running;

            // 记录任务开始运行的时间
            let start_time = get_time_us();
            task_inner.sched.last_runtime = start_time;

            drop(task_inner);
            processor.current = Some(task);
            drop(processor);

            // 这里在切换时保存了当前 __switch 返回地址到 idle_task_cx 的 ra 中，下面的 schedule
            // 切换到 idle_task_cx 时又从 __switch 后面开始执行, 保证了持续调度
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
        } else {
            // 没有可运行的任务，让出 CPU 等待下一次中断（比如时钟中断）
            wfi();
        }
    }
}

/// 当一个应用用尽了内核本轮分配给它的时间片或者它主动调用 yield 系统调用交出 CPU 使用权之后，
/// 内核会调用 schedule 函数来切换到 idle 控制流并开启新一轮的任务调度
pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let mut processor = PROCESSOR.exclusive_access();
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
    drop(processor);

    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}

/// 线程级别的调度函数 - 在同一进程内切换线程
pub fn schedule_thread(current_thread_cx_ptr: *mut TaskContext, next_thread_cx_ptr: *const TaskContext) {
    unsafe {
        __switch(current_thread_cx_ptr, next_thread_cx_ptr);
    }
}

/// 在多线程进程中处理线程调度
fn handle_multithreaded_process(task: &Arc<TaskControlBlock>) -> bool {
    let mut task_inner = task.inner_exclusive_access();
    
    if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
        // 检查是否有可运行的线程
        if thread_manager.has_active_threads() {
            // 如果有活跃线程，让线程管理器处理调度
            if let Some(current_thread) = thread_manager.get_current_thread() {
                if current_thread.get_status() == crate::thread::ThreadStatus::Running {
                    // 当前线程仍在运行，直接返回到用户空间
                    drop(task_inner);
                    return true;
                }
            }
            
            // 尝试调度下一个线程
            thread_manager.schedule_next();
            drop(task_inner);
            return true;
        } else {
            // 没有活跃线程，进程应该退出
            task_inner.sched.task_status = TaskStatus::Zombie;
            task_inner.process.exit_code = 0;
            drop(task_inner);
            return false;
        }
    }
    drop(task_inner);
    false
}

pub fn suspend_current_and_run_next() {
    let task = take_current_task().unwrap();

    // 检查是否有线程管理器（多线程进程）
    {
        let mut task_inner = task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
            // 多线程进程的调度
            if let Some(current_thread) = thread_manager.get_current_thread() {
                current_thread.set_status(crate::thread::ThreadStatus::Ready);
                thread_manager.yield_thread();
            }
            drop(task_inner);
            // 将进程重新加入调度队列
            super::add_task(task);
            return;
        }
    }

    // 单线程进程的原有调度逻辑
    let end_time = get_time_us();
    let mut task_inner = task.inner_exclusive_access();
    let runtime = end_time.saturating_sub(task_inner.sched.last_runtime);
    let task_cx_ptr = &mut task_inner.sched.task_cx as *mut _;
    let task_status = task_inner.sched.task_status;

    // 根据调度策略更新任务统计信息
    match get_scheduling_policy() {
        SchedulingPolicy::CFS => {
            task_inner.update_vruntime(runtime);
        },
        _ => {
            task_inner.sched.last_runtime = runtime;
        }
    }

    if task_status == TaskStatus::Running {
        task_inner.sched.task_status = TaskStatus::Ready;
        drop(task_inner);

        // 更新任务管理器中的运行时间统计
        super::task_manager::update_task_runtime(&task, runtime);

        // push back to ready queue
        super::add_task(task);
    } else {
        // 如果任务是Sleeping状态，不要重新加入就绪队列
        drop(task_inner);
    }

    // jump to schedule cycle
    schedule(task_cx_ptr);
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().unwrap();

    // 检查是否有线程管理器（多线程进程）
    {
        let mut task_inner = task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
            // 多线程进程中的线程阻塞
            if let Some(current_thread) = thread_manager.get_current_thread() {
                current_thread.set_status(crate::thread::ThreadStatus::Blocked);
                // 调度下一个线程
                thread_manager.schedule_next();
            }
            drop(task_inner);
            // 将进程重新加入调度队列
            super::add_task(task);
            return;
        }
    }

    // 单线程进程的原有阻塞逻辑
    let end_time = get_time_us();
    let mut task_inner = task.inner_exclusive_access();
    let runtime = end_time.saturating_sub(task_inner.sched.last_runtime);
    let task_cx_ptr = &mut task_inner.sched.task_cx as *mut _;
    task_inner.sched.task_status = TaskStatus::Sleeping;

    // 更新运行时间统计
    match get_scheduling_policy() {
        SchedulingPolicy::CFS => {
            task_inner.update_vruntime(runtime);
        },
        _ => {
            task_inner.sched.last_runtime = runtime;
        }
    }

    drop(task_inner);

    // 更新任务管理器中的运行时间统计
    super::task_manager::update_task_runtime(&task, runtime);

    // 不将任务加入就绪队列，让它保持阻塞状态
    // 任务将通过wakeup_task函数被唤醒

    // jump to schedule cycle
    schedule(task_cx_ptr);
}

pub const IDLE_PID: usize = 0;

pub fn exit_current_and_run_next(exit_code: i32) -> ! {
    let task = take_current_task().unwrap();

    let pid = task.get_pid();
    if pid == IDLE_PID {
        debug!(
            "[kernel] Idle process exit with exit_code {} ...",
            exit_code
        );
        if exit_code != 0 {
            shutdown()
        } else {
            shutdown()
        }
    }

    let mut inner = task.inner_exclusive_access();

    inner.sched.task_status = TaskStatus::Zombie;
    inner.process.exit_code = exit_code;

    {
        let init_proc = super::task_manager::get_init_proc().unwrap();
        let mut init_proc_inner = init_proc.inner_exclusive_access();
        for child in inner.process.children.iter() {
            child.inner_exclusive_access().process.parent = Some(Arc::downgrade(&init_proc));
            init_proc_inner.process.children.push(child.clone());
        }
    }

    inner.process.children.clear();
    // 关闭所有打开的文件描述符并清理文件锁
    inner.close_all_fds_and_cleanup_locks(pid);
    // deallocate user space
    inner.mm.memory_set.recycle_data_pages();
    drop(inner);

    drop(task);

    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
    loop {}
}

pub fn current_cwd() -> String {
    current_task()
        .map(|task| task.inner_exclusive_access().process.cwd.clone())
        .unwrap_or_else(|| "/".to_string())
}

/// 描述 CPU 执行状态
struct Processor {
    /// 当前正在执行的任务
    current: Option<Arc<TaskControlBlock>>,
    /// 当前处理器上 idle 任务的上下文
    idle_task_cx: TaskContext,
}

impl Processor {
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }

    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }

    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(|task| Arc::clone(task))
    }

    pub fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_cx
    }
}
