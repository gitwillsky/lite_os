use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use spin::RwLock;

use crate::{
    arch::hart::{hart_id, is_valid_hart_id, MAX_CORES},
    sync::UPSafeCell,
    task::{
        context::TaskContext,
        scheduler::{Scheduler, cfs_scheduler::CFScheduler},
        task::{TaskControlBlock, TaskStatus},
    },
};

/// 核心本地处理器数据
pub struct CoreProcessor {
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

impl CoreProcessor {
    pub fn new(hart_id: usize) -> Self {
        Self {
            hart_id,
            current: None,
            idle_context: TaskContext::zero_init(),
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
        if self.task_count() > 1 {
            self.fetch_task()
        } else {
            None
        }
    }
}

/// 多核心管理器
pub struct CoreManager {
    /// 每个核心的处理器
    pub processors: [UPSafeCell<CoreProcessor>; MAX_CORES],
    /// 活跃核心数量
    pub active_cores: AtomicUsize,
    /// 启动屏障 - 等待所有核心就绪
    pub boot_barrier: AtomicUsize,
    /// 全局任务列表（用于监控和调试）
    pub global_tasks: RwLock<Vec<Arc<TaskControlBlock>>>,
}

impl CoreManager {
    pub fn new() -> Self {
        // 手动创建处理器数组，避免const函数限制
        Self {
            processors: [
                UPSafeCell::new(CoreProcessor::new(0)),
                UPSafeCell::new(CoreProcessor::new(1)),
                UPSafeCell::new(CoreProcessor::new(2)),
                UPSafeCell::new(CoreProcessor::new(3)),
                UPSafeCell::new(CoreProcessor::new(4)),
                UPSafeCell::new(CoreProcessor::new(5)),
                UPSafeCell::new(CoreProcessor::new(6)),
                UPSafeCell::new(CoreProcessor::new(7)),
            ],
            active_cores: AtomicUsize::new(0), // 初始时没有核心活跃
            boot_barrier: AtomicUsize::new(0),
            global_tasks: RwLock::new(Vec::new()),
        }
    }

    /// 获取指定核心的处理器
    pub fn get_processor(&self, hart_id: usize) -> Option<&UPSafeCell<CoreProcessor>> {
        if is_valid_hart_id(hart_id) {
            Some(&self.processors[hart_id])
        } else {
            None
        }
    }

    /// 获取当前核心的处理器
    pub fn current_processor(&self) -> &UPSafeCell<CoreProcessor> {
        let hart = hart_id();
        &self.processors[hart]
    }

    /// 激活一个核心
    pub fn activate_core(&self, hart_id: usize) {
        if let Some(processor) = self.get_processor(hart_id) {
            let mut proc = processor.exclusive_access();
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
        let mut target_core = 0;

        for i in 0..MAX_CORES {
            if let Some(processor) = self.get_processor(i) {
                let proc = processor.exclusive_access();
                if proc.active.load(Ordering::Relaxed) {
                    let load = proc.task_count();
                    if load < min_load {
                        min_load = load;
                        target_core = i;
                    }
                }
            }
        }

        target_core
    }

    /// 工作窃取 - 从其他核心窃取任务
    pub fn steal_work(&self, idle_hart: usize) -> Option<Arc<TaskControlBlock>> {
        for hart in 0..MAX_CORES {
            if hart != idle_hart {
                if let Some(processor) = self.get_processor(hart) {
                    let mut proc = processor.exclusive_access();
                    if proc.active.load(Ordering::Relaxed) {
                        if let Some(task) = proc.steal_task() {
                            return Some(task);
                        }
                    }
                }
            }
        }
        None
    }

    /// 添加任务 - 智能分配到合适的核心
    pub fn add_task(&self, task: Arc<TaskControlBlock>) {
        // 简单策略：分配到负载最轻的核心
        let target_core = self.find_least_loaded_core();
        
        if let Some(processor) = self.get_processor(target_core) {
            processor.exclusive_access().add_task(task.clone());
        }

        // 添加到全局任务列表（用于监控）
        self.global_tasks.write().push(task);
    }

    /// 获取所有任务
    pub fn get_all_tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let mut all_tasks = Vec::new();

        // 收集各核心调度器中的任务
        for i in 0..MAX_CORES {
            if let Some(processor) = self.get_processor(i) {
                let proc = processor.exclusive_access();
                if proc.active.load(Ordering::Relaxed) {
                    all_tasks.extend(proc.scheduler.get_all_tasks());
                }
            }
        }

        // 添加当前运行的任务
        for i in 0..MAX_CORES {
            if let Some(processor) = self.get_processor(i) {
                let proc = processor.exclusive_access();
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
                let proc = processor.exclusive_access();
                if proc.active.load(Ordering::Relaxed) {
                    count += proc.task_count();
                }
            }
        }
        count
    }
}

lazy_static! {
    pub static ref CORE_MANAGER: CoreManager = CoreManager::new();
}

/// 获取当前核心的处理器
pub fn current_processor() -> &'static UPSafeCell<CoreProcessor> {
    CORE_MANAGER.current_processor()
}

/// 获取指定核心的处理器
pub fn get_processor(hart_id: usize) -> Option<&'static UPSafeCell<CoreProcessor>> {
    CORE_MANAGER.get_processor(hart_id)
}