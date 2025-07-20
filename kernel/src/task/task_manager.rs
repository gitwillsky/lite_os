use alloc::{
    boxed::Box,
    collections::{binary_heap::BinaryHeap, vec_deque::VecDeque},
    sync::Arc,
};
use core::{cmp::Ordering, sync::atomic::AtomicUsize};
use lazy_static::lazy_static;
use spin::{Mutex, RwLock};

use crate::{
    sync::UPSafeCell,
    task::{
        pid::INIT_PID, scheduler::{
            cfs_scheduler::CFScheduler, fifo_scheduler::FIFOScheduler, priority_scheduler::PriorityScheduler, Scheduler
        }, task::{TaskControlBlock, TaskStatus}
    },
};

/// 调度策略枚举
#[derive(Debug, Clone, Copy)]
pub enum SchedulingPolicy {
    FIFO,       // 先进先出
    Priority,   // 优先级调度
    RoundRobin, // 时间片轮转
    CFS,        // 完全公平调度器
}

struct TaskManager {
    /// 调度策略
    policy: RwLock<SchedulingPolicy>,
    scheduler: Mutex<Box<dyn Scheduler>>,
    /// 全局最小vruntime
    min_vruntime: AtomicUsize,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            policy: RwLock::new(SchedulingPolicy::CFS), // 默认使用CFS
            scheduler: Mutex::new(Box::new(CFScheduler::new())),
            min_vruntime: AtomicUsize::new(0),
        }
    }

    pub fn set_scheduling_policy(&mut self, policy: SchedulingPolicy) {
        match policy {
            SchedulingPolicy::FIFO => {
                self.scheduler = Mutex::new(Box::new(FIFOScheduler::new()));
            }
            SchedulingPolicy::Priority => {
                self.scheduler = Mutex::new(Box::new(PriorityScheduler::new()));
            }
            SchedulingPolicy::RoundRobin => {
                self.scheduler = Mutex::new(Box::new(FIFOScheduler::new()));
            }
            SchedulingPolicy::CFS => {}
        }
        *self.policy.write() = policy;
    }

    /// 将任务添加到相应的调度队列
    pub fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        self.scheduler.lock().add_task(task);
    }

    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.scheduler.lock().fetch_task()
    }

    /// 更新任务的运行时间统计
    pub fn update_task_runtime(&mut self, task: &Arc<TaskControlBlock>, runtime_us: u64) {
        task.sched.lock().update_vruntime(runtime_us);
    }

    /// 获取当前调度策略
    pub fn get_scheduling_policy(&self) -> SchedulingPolicy {
        *self.policy.read()
    }

    /// 统计就绪任务数量
    pub fn ready_task_count(&self) -> usize {
        self.scheduler.lock().ready_task_count()
    }

    /// 根据PID查找任务（搜索所有可能的位置）
    pub fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        self.scheduler.lock().find_task_by_pid(pid)
    }
}

lazy_static! {
    static ref TASK_MANAGER: UPSafeCell<TaskManager> = UPSafeCell::new(TaskManager::new());
}

pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().add_task(task);
}

pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().fetch_task()
}

pub fn get_init_proc() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().find_task_by_pid(INIT_PID)
}

/// 设置调度策略
pub fn set_scheduling_policy(policy: SchedulingPolicy) {
    TASK_MANAGER
        .exclusive_access()
        .set_scheduling_policy(policy);
}

/// 获取当前调度策略
pub fn get_scheduling_policy() -> SchedulingPolicy {
    TASK_MANAGER.exclusive_access().get_scheduling_policy()
}


/// 获取就绪任务数量
pub fn ready_task_count() -> usize {
    TASK_MANAGER.exclusive_access().ready_task_count()
}

/// 唤醒任务，将其从睡眠状态转为就绪状态
pub fn wakeup_task(task: Arc<TaskControlBlock>) {
    if *task.task_status.lock() == TaskStatus::Sleeping {
        *task.task_status.lock() = TaskStatus::Ready;
        // 将任务添加到就绪队列
        add_task(task);
    }
}

/// 根据PID查找任务，包括当前运行的任务
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().find_task_by_pid(pid)
}
