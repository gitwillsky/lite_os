use alloc::{string::{String, ToString}, sync::Arc};
use lazy_static::lazy_static;
use riscv::asm::wfi;

use crate::{
    arch::sbi::shutdown,
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
    static ref PROCESSOR: spin::Mutex<Processor> = spin::Mutex::new(Processor::new());
}

pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.lock().take_current()
}

pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    PROCESSOR.lock().current()
}

pub fn current_user_token() -> usize {
    if let Some(task) = current_task() {
        let task_inner = task.inner_exclusive_access();
        // 对于多线程进程，我们仍然使用进程的用户页表 token
        // 因为所有线程共享同一个地址空间
        task_inner.get_user_token()
    } else {
        // 这种情况不应该在正常的用户空间陷入中发生
        // 如果发生了，说明调度逻辑有严重问题
        error!("current_user_token() called with no current task - this indicates a serious scheduling bug!");
        
        // 记录调用栈以便调试
        // 在生产环境中，这应该是一个严重错误
        panic!("No current task when getting user token");
    }
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
        let mut processor = PROCESSOR.lock();
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
                    processor = PROCESSOR.lock();
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
    let mut processor = PROCESSOR.lock();
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

pub fn suspend_current_and_run_next() {
    debug!("suspend_current_and_run_next: starting");
    let task = take_current_task().unwrap();
    let end_time = get_time_us();

    // 统一处理运行时间统计
    let mut task_inner = task.inner_exclusive_access();
    let runtime = end_time.saturating_sub(task_inner.sched.last_runtime);
    let task_cx_ptr = &mut task_inner.sched.task_cx as *mut _;
    let task_status = task_inner.sched.task_status;

    debug!("suspend_current_and_run_next: task_status={:?}", task_status);

    // 根据调度策略更新任务统计信息
    match get_scheduling_policy() {
        SchedulingPolicy::CFS => {
            task_inner.update_vruntime(runtime);
        },
        _ => {
            task_inner.sched.last_runtime = runtime;
        }
    }

    // 处理多线程进程的线程调度
    debug!("suspend_current_and_run_next: checking for thread manager, task PID: {}, task addr: {:p}", 
           task.get_pid(), task.as_ref());
    if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
        debug!("suspend_current_and_run_next: found thread manager with {} threads", 
               thread_manager.thread_count());
        // 多线程进程：更新当前线程状态并调度下一个线程
        if let Some(current_thread) = thread_manager.get_current_thread() {
            debug!("suspend_current_and_run_next: current thread is {}", current_thread.get_thread_id().0);
            current_thread.set_status(crate::thread::ThreadStatus::Ready);
            
            // 将当前线程加入就绪队列
            thread_manager.add_thread_to_ready_queue(current_thread.get_thread_id());
            
            // 调度下一个线程（仅更新状态，不做上下文切换）
            thread_manager.schedule_next_no_switch();
        }
        debug!("suspend_current_and_run_next: thread manager stats: {:?}", thread_manager.get_thread_stats());
    } else {
        debug!("suspend_current_and_run_next: no thread manager found for task PID: {}, task addr: {:p}", 
               task.get_pid(), task.as_ref());
    }

    // 统一的任务状态处理
    if task_status == TaskStatus::Running {
        task_inner.sched.task_status = TaskStatus::Ready;
        drop(task_inner);

        // 更新任务管理器中的运行时间统计
        super::task_manager::update_task_runtime(&task, runtime);

        debug!("suspend_current_and_run_next: adding task back to ready queue");
        // push back to ready queue
        super::add_task(task);
    } else {
        // 如果任务是Sleeping状态，不要重新加入就绪队列
        drop(task_inner);
    }

    debug!("suspend_current_and_run_next: calling schedule");
    // 所有进程都必须经过统一的调度流程
    schedule(task_cx_ptr);
}

/// 阻塞当前任务并切换到下一个任务
pub fn block_current_and_run_next() {
    let task = take_current_task().unwrap();
    let end_time = get_time_us();

    // 统一处理运行时间统计
    let mut task_inner = task.inner_exclusive_access();
    let runtime = end_time.saturating_sub(task_inner.sched.last_runtime);
    let task_cx_ptr = &mut task_inner.sched.task_cx as *mut _;
    task_inner.sched.task_status = TaskStatus::Sleeping;

    // 处理多线程进程的线程调度
    if let Some(thread_manager) = task_inner.thread_manager.as_mut() {
        // 多线程进程中的线程阻塞
        if let Some(current_thread) = thread_manager.get_current_thread() {
            current_thread.set_status(crate::thread::ThreadStatus::Blocked);
            // 调度下一个线程
            thread_manager.schedule_next();
        }
    }

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

    // 对于多线程进程，如果还有活跃线程，重新加入队列
    // 对于单线程进程，不加入队列（保持阻塞状态）
    {
        let task_inner = task.inner_exclusive_access();
        if let Some(thread_manager) = task_inner.thread_manager.as_ref() {
            if thread_manager.has_active_threads() {
                drop(task_inner);
                super::add_task(task);
            }
        }
    }

    // 所有进程都必须经过统一的调度流程
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
