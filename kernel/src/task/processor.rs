use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use spin::{RwLock, Mutex};

use crate::{
    arch::hart::{hart_id, is_valid_hart_id, MAX_CORES},
    task::{
        context::TaskContext,
        scheduler::{Scheduler, cfs_scheduler::CFScheduler},
        TaskControlBlock, TaskStatus,
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

/// 多核心管理器
pub struct MultiProcessorManager {
    /// 每个核心的处理器
    pub processors: [Mutex<Processor>; MAX_CORES],
    /// 活跃核心数量
    pub active_cores: AtomicUsize,
    /// 启动屏障 - 等待所有核心就绪
    pub boot_barrier: AtomicUsize,
}

impl MultiProcessorManager {
    pub fn new() -> Self {
        // 手动创建处理器数组，避免const函数限制
        Self {
            processors: [
                Mutex::new(Processor::new(0)),
                Mutex::new(Processor::new(1)),
                Mutex::new(Processor::new(2)),
                Mutex::new(Processor::new(3)),
                Mutex::new(Processor::new(4)),
                Mutex::new(Processor::new(5)),
                Mutex::new(Processor::new(6)),
                Mutex::new(Processor::new(7)),
            ],
            active_cores: AtomicUsize::new(0), // 初始时没有核心活跃
            boot_barrier: AtomicUsize::new(0),
        }
    }

    /// 获取指定核心的处理器
    pub fn get_processor(&self, hart_id: usize) -> Option<&Mutex<Processor>> {
        if is_valid_hart_id(hart_id) {
            Some(&self.processors[hart_id])
        } else {
            None
        }
    }

    /// 获取当前核心的处理器
    pub fn current_processor(&self) -> &Mutex<Processor> {
        let hart = hart_id();
        &self.processors[hart]
    }

    /// 激活一个核心
    pub fn activate_core(&self, hart_id: usize) {
        if let Some(processor) = self.get_processor(hart_id) {
            let mut proc = processor.lock();
            if !proc.active.load(Ordering::Relaxed) {
                proc.active.store(true, Ordering::Relaxed);
                self.active_cores.fetch_add(1, Ordering::Relaxed);
                debug!("Core {} activated, total active cores: {}", hart_id, self.active_cores.load(Ordering::Relaxed));
            }
        }
    }

    /// 获取活跃核心数量
    pub fn active_core_count(&self) -> usize {
        self.active_cores.load(Ordering::Relaxed)
    }

    /// 寻找负载最轻的核心
    pub fn find_least_loaded_core(&self) -> usize {
        let mut min_load = usize::MAX;
        let mut target_core = None;
        let mut active_cores = Vec::new();

        // 首先收集所有活跃核心的负载信息
        for i in 0..MAX_CORES {
            if let Some(processor) = self.get_processor(i) {
                let proc = processor.lock();
                if proc.active.load(Ordering::Relaxed) {
                    let load = proc.task_count();
                    active_cores.push((i, load));
                    if load < min_load {
                        min_load = load;
                        target_core = Some(i);
                    }
                }
            }
        }

        // 如果找到目标核心，返回它；否则返回第一个活跃核心
        target_core.unwrap_or_else(|| {
            if let Some((core_id, _)) = active_cores.first() {
                *core_id
            } else {
                0 // 后备选择
            }
        })
    }

    /// 工作窃取 - 从其他核心窃取任务
    pub fn steal_work(&self, idle_hart: usize) -> Option<Arc<TaskControlBlock>> {
        // 使用更细粒度的锁策略，减少持锁时间
        let mut loaded_cores = Vec::new();
        
        // 快速收集负载信息，不持有锁
        for hart in 0..MAX_CORES {
            if hart != idle_hart {
                if let Some(processor) = self.get_processor(hart) {
                    // 快速检查活跃状态和负载
                    let is_active = processor.lock().active.load(Ordering::Acquire);
                    if is_active {
                        // 原子读取任务计数，避免长时间持锁
                        let load = processor.lock().task_count.load(Ordering::Acquire);
                        if load > 1 { // 只从有多个任务的核心窃取
                            loaded_cores.push((hart, load));
                        }
                    }
                }
            }
        }

        // 按负载降序排序，优先从最忙的核心窃取
        loaded_cores.sort_by(|a, b| b.1.cmp(&a.1));

        // 尝试从负载最重的核心窃取任务
        for (hart, _load) in loaded_cores {
            if let Some(processor) = self.get_processor(hart) {
                // 使用try_lock避免死锁
                if let Some(mut proc) = processor.try_lock() {
                    if let Some(task) = proc.steal_task() {
                        // 内存屏障确保任务状态的可见性
                        core::sync::atomic::fence(Ordering::Acquire);
                        return Some(task);
                    }
                }
            }
        }

        None
    }

    /// 添加任务 - 智能分配到合适的核心
    pub fn add_task(&self, task: Arc<TaskControlBlock>) {
        use crate::arch::hart::hart_id;

        let target_core = if task.pid() == crate::task::pid::INIT_PID {
            // init任务分配到当前boot核心，确保系统启动顺序正确
            let current_hart = hart_id();
            current_hart
        } else {
            // 其他任务分配到负载最轻的核心
            let selected_core = self.find_least_loaded_core();
            selected_core
        };

        if let Some(processor) = self.get_processor(target_core) {
            processor.lock().add_task(task.clone());
        } else {
            warn!("Failed to add task PID {} to core {} (invalid core)", task.pid(), target_core);
        }
    }

    /// 获取所有任务
    pub fn get_all_tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let mut all_tasks = Vec::new();

        // 收集各核心调度器中的任务
        for i in 0..MAX_CORES {
            if let Some(processor) = self.get_processor(i) {
                let proc = processor.lock();
                if proc.active.load(Ordering::Relaxed) {
                    all_tasks.extend(proc.scheduler.get_all_tasks());
                }
            }
        }

        // 添加当前运行的任务
        for i in 0..MAX_CORES {
            if let Some(processor) = self.get_processor(i) {
                let proc = processor.lock();
                if let Some(current) = &proc.current {
                    all_tasks.push(current.clone());
                }
            }
        }

        all_tasks
    }

    /// 统计总任务数量
    pub fn total_task_count(&self) -> usize {
        let mut count = 0;
        for i in 0..MAX_CORES {
            if let Some(processor) = self.get_processor(i) {
                let proc = processor.lock();
                if proc.active.load(Ordering::Relaxed) {
                    count += proc.task_count();
                }
            }
        }
        count
    }
}

lazy_static! {
    pub static ref CORE_MANAGER: MultiProcessorManager = MultiProcessorManager::new();
}

/// 获取当前核心的处理器
pub fn current_processor() -> &'static Mutex<Processor> {
    CORE_MANAGER.current_processor()
}

/// 获取指定核心的处理器
pub fn get_processor(hart_id: usize) -> Option<&'static Mutex<Processor>> {
    CORE_MANAGER.get_processor(hart_id)
}





