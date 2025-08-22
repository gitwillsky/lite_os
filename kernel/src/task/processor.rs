use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use spin::{Mutex, RwLock};

use crate::{
    arch::hart::{MAX_CORES, hart_id, is_valid_hart_id},
    task::{
        TaskControlBlock, TaskStatus,
        context::TaskContext,
        scheduler::{Scheduler, cfs_scheduler::CFScheduler},
    },
    timer::get_time_us,
};

/// idle返回函数 - 当从任务切换回idle时执行
/// 这个函数永远不应该被调用，因为idle循环不会切换回来
/// 但我们需要一个有效的返回地址以避免异常
#[unsafe(no_mangle)]
pub extern "C" fn idle_return() -> ! {
    // 如果执行到这里，说明出现了严重错误
    panic!("idle_return should never be called!");
}

pub struct Processor {
    /// 当前核心ID
    pub hart_id: usize,
    /// 当前正在执行的任务
    pub current: Option<Arc<TaskControlBlock>>,
    /// idle任务上下文
    pub idle_context: TaskContext,
    /// 本地调度器
    pub scheduler: Box<dyn Scheduler>,
    /// 本地任务计数
    pub task_count: AtomicUsize,
    /// 核心是否活跃
    pub active: AtomicBool,
}

impl Processor {
    pub fn new(hart_id: usize) -> Self {
        // 创建一个有效的idle上下文
        // 重要：idle上下文的ra必须指向一个有效的返回地址
        let mut idle_ctx = TaskContext::zero_init();
        // 设置返回地址为idle_return函数
        idle_ctx.set_ra(idle_return as usize);

        Self {
            hart_id,
            current: None,
            idle_context: idle_ctx,
            scheduler: Box::new(CFScheduler::new()),
            task_count: AtomicUsize::new(0),
            active: AtomicBool::new(false),
        }
    }

    /// 获取idle上下文指针
    pub fn idle_context_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_context
    }

    /// 添加任务到本地调度器
    pub fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        self.scheduler.add_task(task);
        self.task_count.fetch_add(1, Ordering::Relaxed);
    }

    /// 从本地调度器获取任务
    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        if let Some(task) = self.scheduler.fetch_task() {
            self.task_count.fetch_sub(1, Ordering::Relaxed);
            Some(task)
        } else {
            None
        }
    }

    /// 获取本地任务数量
    pub fn task_count(&self) -> usize {
        self.task_count.load(Ordering::Relaxed)
    }

    /// 尝试窃取一个任务
    pub fn steal_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        // 只有当任务数量大于1时才允许窃取
        // 使用原子操作检查任务数量，避免竞态
        let count = self.task_count.load(Ordering::Acquire);
        if count > 1 {
            // 尝试获取任务，如果成功则减少计数
            if let Some(task) = self.scheduler.fetch_task() {
                self.task_count.fetch_sub(1, Ordering::Release);

                // 验证任务状态，确保不窃取正在运行或睡眠的任务
                let status = task.task_status.lock();
                if *status != TaskStatus::Ready {
                    // 任务状态不合适，放回并恢复计数
                    drop(status);
                    self.scheduler.add_task(task.clone());
                    self.task_count.fetch_add(1, Ordering::Release);
                    return None;
                }
                drop(status);

                Some(task)
            } else {
                None
            }
        } else {
            None
        }
    }
}

// Per-CPU 变量，每个核心完全独立
static mut PER_CPU_PROCESSORS: [Option<Processor>; MAX_CORES] = [const { None }; MAX_CORES];
static ACTIVE_CORES: AtomicUsize = AtomicUsize::new(0);

/// 无锁访问当前 CPU 的处理器
pub fn current_processor() -> &'static mut Processor {
    let hart = hart_id();
    unsafe {
        if PER_CPU_PROCESSORS[hart].is_none() {
            PER_CPU_PROCESSORS[hart] = Some(Processor::new(hart));
        }
        PER_CPU_PROCESSORS[hart].as_mut().unwrap()
    }
}

/// 获取活跃核心数量
pub fn active_core_count() -> usize {
    ACTIVE_CORES.load(Ordering::Relaxed)
}

/// 标记核心为活跃状态
pub fn mark_core_active() {
    ACTIVE_CORES.fetch_add(1, Ordering::Relaxed);
}
