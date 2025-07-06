use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::{
    sync::UPSafeCell,
    task::{
        __switch,
        context::TaskContext,
        task::{TaskControlBlock, TaskStatus},
    },
    trap::TrapContext,
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
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .get_trap_cx()
}

/// 在内核初始化完毕之后，会通过调用 run_tasks 函数来进入 idle 控制流
pub fn run_tasks() -> ! {
    loop {
        let mut processor = PROCESSOR.exclusive_access();
        if let Some(task) = super::task_manager::fetch_task() {
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            let mut task_inner = task.inner_exclusive_access();
            let next_task_cx_ptr = &task_inner.task_cx as *const TaskContext;
            task_inner.task_status = TaskStatus::Running;
            drop(task_inner);
            processor.current = Some(task);
            drop(processor);

            // 这里在切换时保存了当前 __switch 返回地址到 idle_task_cx 的 ra 中，下面的 schedule
            // 切换到 idle_task_cx 时又从 __switch 后面开始执行, 保证了持续调度
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
        }
    }
}

/// 当一个应用用尽了内核本轮分配给它的时间片或者它主动调用 yield 系统调用交出 CPU 使用权之后，
/// 内核会调用 schedule 函数来切换到 idle 控制流并开启新一轮的任务调度
pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let mut processor = PROCESSOR.exclusive_access();
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
    drop(processor);

    println!("schedule");

    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
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
