use alloc::{
    boxed::Box,
    collections::{binary_heap::BinaryHeap, vec_deque::VecDeque},
    sync::Arc,
    vec::Vec,
};
use core::{cmp::Ordering, sync::atomic::AtomicUsize};
use lazy_static::lazy_static;
use spin::{Mutex, RwLock};

use crate::{
    sync::UPSafeCell,
    task::{
        current_task,
        pid::INIT_PID,
        scheduler::{
            Scheduler, cfs_scheduler::CFScheduler, fifo_scheduler::FIFOScheduler,
            priority_scheduler::PriorityScheduler,
        },
        task::{TaskControlBlock, TaskStatus},
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

    init_proc: Option<Arc<TaskControlBlock>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            policy: RwLock::new(SchedulingPolicy::CFS), // 默认使用CFS
            scheduler: Mutex::new(Box::new(CFScheduler::new())),
            min_vruntime: AtomicUsize::new(0),
            init_proc: None,
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
        if task.pid() == INIT_PID {
            self.init_proc = Some(task.clone());
        }
        self.scheduler.lock().add_task(task);
    }

    pub fn init_proc(&self) -> Option<Arc<TaskControlBlock>> {
        self.init_proc.clone()
    }

    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.scheduler.lock().fetch_task()
    }

    /// 更新任务的运行时间统计
    pub fn update_task_runtime(&mut self, task: &Arc<TaskControlBlock>, runtime_us: u64) {
        task.sched.lock().update_vruntime(runtime_us);
    }

    /// 获取当前调度策略
    pub fn scheduling_policy(&self) -> SchedulingPolicy {
        *self.policy.read()
    }

    /// 统计可调度任务数量
    pub fn schedulable_task_count(&self) -> usize {
        self.scheduler.lock().count()
    }

    pub fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        let all_tasks = self.get_all_tasks();
        all_tasks.into_iter().find(|task| task.pid() == pid)
    }

    /// 获取所有任务列表（包括调度器中的、当前运行的和init进程）
    pub fn get_all_tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let mut all_tasks = self.scheduler.lock().get_all_tasks();

        // 添加当前运行的任务（如果不在调度器队列中）
        if let Some(current) = current_task() {
            if !all_tasks.iter().any(|task| task.pid() == current.pid()) {
                all_tasks.push(current);
            }
        }

        // 添加init进程（如果不在其他列表中）
        if let Some(init_proc) = &self.init_proc {
            if !all_tasks.iter().any(|task| task.pid() == init_proc.pid()) {
                all_tasks.push(init_proc.clone());
            }
        }

        // 添加睡眠中的任务
        all_tasks.extend(crate::timer::get_sleeping_tasks());

        all_tasks
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

/// 设置调度策略
pub fn set_scheduling_policy(policy: SchedulingPolicy) {
    TASK_MANAGER
        .exclusive_access()
        .set_scheduling_policy(policy);
}

/// 获取当前调度策略
pub fn get_scheduling_policy() -> SchedulingPolicy {
    TASK_MANAGER.exclusive_access().scheduling_policy()
}

/// 获取可调度任务数量
pub fn schedulable_task_count() -> usize {
    TASK_MANAGER.exclusive_access().schedulable_task_count()
}

/// 获取init进程
pub fn init_proc() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().init_proc()
}

pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().find_task_by_pid(pid)
}

/// 获取所有任务的列表
pub fn get_all_tasks() -> Vec<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().get_all_tasks()
}
