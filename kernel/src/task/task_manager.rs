use alloc::{collections::{vec_deque::VecDeque, binary_heap::BinaryHeap}, sync::Arc};
use lazy_static::lazy_static;
use core::cmp::Ordering;

use crate::{sync::UPSafeCell, task::task::{TaskControlBlock, TaskStatus}};

/// CFS调度器中的任务包装器，用于按vruntime排序
#[derive(Debug)]
struct CFSTask {
    task: Arc<TaskControlBlock>,
    vruntime: u64,
}

impl CFSTask {
    fn new(task: Arc<TaskControlBlock>) -> Self {
        let vruntime = task.inner_exclusive_access().sched.vruntime;
        Self { task, vruntime }
    }
}

impl PartialEq for CFSTask {
    fn eq(&self, other: &Self) -> bool {
        self.vruntime == other.vruntime
    }
}

impl Eq for CFSTask {}

impl PartialOrd for CFSTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CFSTask {
    fn cmp(&self, other: &Self) -> Ordering {
        // 最小堆：vruntime小的任务优先级高
        other.vruntime.cmp(&self.vruntime)
    }
}

/// 调度策略枚举
#[derive(Debug, Clone, Copy)]
pub enum SchedulingPolicy {
    FIFO,           // 先进先出
    Priority,       // 优先级调度
    RoundRobin,     // 时间片轮转
    CFS,            // 完全公平调度器
}

/// 调度器统计信息
#[derive(Debug, Default, Clone)]
pub struct SchedulerStats {
    /// 总任务数
    pub total_tasks: usize,
    /// 运行中任务数
    pub running_tasks: usize,
    /// 就绪任务数
    pub ready_tasks: usize,
    /// 阻塞任务数
    pub blocked_tasks: usize,
    /// 调度切换次数
    pub context_switches: u64,
    /// 平均时间片利用率
    pub avg_time_slice_usage: f32,
}

impl SchedulerStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn inc_context_switches(&mut self) {
        self.context_switches += 1;
    }

    pub fn update_task_counts(&mut self, ready: usize, running: usize, blocked: usize) {
        self.ready_tasks = ready;
        self.running_tasks = running;
        self.blocked_tasks = blocked;
        self.total_tasks = ready + running + blocked;
    }

    pub fn update_time_slice_usage(&mut self, usage: f32) {
        // 简单的滑动平均
        self.avg_time_slice_usage = self.avg_time_slice_usage * 0.9 + usage * 0.1;
    }
}
/// 优化的任务管理器，添加统计信息
struct TaskManager {
    /// 调度策略
    scheduling_policy: SchedulingPolicy,
    /// FIFO就绪队列
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
    /// 多级优先级队列 (0-39)
    priority_queues: [VecDeque<Arc<TaskControlBlock>>; 40],
    /// CFS红黑树模拟（使用BinaryHeap）
    cfs_queue: BinaryHeap<CFSTask>,
    /// 初始进程
    pub init_proc: Option<Arc<TaskControlBlock>>,
    /// 全局最小vruntime
    min_vruntime: u64,
    /// 调度统计信息
    stats: SchedulerStats,
}

impl TaskManager {
    pub fn new() -> Self {
        // 创建40个空的优先级队列
        let priority_queues: [VecDeque<Arc<TaskControlBlock>>; 40] = core::array::from_fn(|_| VecDeque::new());

        Self {
            scheduling_policy: SchedulingPolicy::CFS, // 默认使用CFS
            ready_queue: VecDeque::new(),
            priority_queues,
            cfs_queue: BinaryHeap::new(),
            init_proc: None,
            min_vruntime: 0,
            stats: SchedulerStats::new(),
        }
    }

    pub fn set_scheduling_policy(&mut self, policy: SchedulingPolicy) {
        self.scheduling_policy = policy;
    }

    pub fn set_init_proc(&mut self, init_proc: Arc<TaskControlBlock>) {
        self.add_task(init_proc.clone());
        self.init_proc = Some(init_proc);
    }

    /// 将任务添加到相应的调度队列
    pub fn add_task(&mut self, task: Arc<TaskControlBlock>) {
        match self.scheduling_policy {
            SchedulingPolicy::FIFO => {
                self.ready_queue.push_back(task);
            },
            SchedulingPolicy::Priority | SchedulingPolicy::RoundRobin => {
                let priority = task.inner_exclusive_access().get_dynamic_priority() as usize;
                let priority = priority.min(39); // 确保不越界
                self.priority_queues[priority].push_back(task);
            },
            SchedulingPolicy::CFS => {
                // 更新全局最小vruntime
                let task_inner = task.inner_exclusive_access();
                let task_vruntime = task_inner.sched.vruntime;
                drop(task_inner);

                // 如果任务的vruntime太小，将其设置为当前最小值
                if task_vruntime < self.min_vruntime {
                    let mut task_inner = task.inner_exclusive_access();
                    task_inner.sched.vruntime = self.min_vruntime;
                    drop(task_inner);
                }

                self.cfs_queue.push(CFSTask::new(task));
            }
        }
        // 更新统计信息
        self.update_stats();
    }

    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        let task = match self.scheduling_policy {
            SchedulingPolicy::FIFO => {
                self.ready_queue.pop_front()
            },
            SchedulingPolicy::Priority | SchedulingPolicy::RoundRobin => {
                // 从高优先级到低优先级查找任务
                let mut result = None;
                for queue in &mut self.priority_queues {
                    if let Some(task) = queue.pop_front() {
                        result = Some(task);
                        break;
                    }
                }
                result
            },
            SchedulingPolicy::CFS => {
                if let Some(cfs_task) = self.cfs_queue.pop() {
                    // 更新全局最小vruntime
                    self.min_vruntime = cfs_task.vruntime;
                    Some(cfs_task.task)
                } else {
                    None
                }
            }
        };
        
        if task.is_some() {
            self.stats.inc_context_switches();
        }
        self.update_stats();
        task
    }

    /// 更新任务的运行时间统计
    pub fn update_task_runtime(&mut self, task: &Arc<TaskControlBlock>, runtime_us: u64) {
        let mut task_inner = task.inner_exclusive_access();

        match self.scheduling_policy {
            SchedulingPolicy::CFS => {
                task_inner.update_vruntime(runtime_us);
                task_inner.sched.last_runtime = runtime_us;
            },
            _ => {
                task_inner.sched.last_runtime = runtime_us;
            }
        }
    }

    /// 获取当前调度策略
    pub fn get_scheduling_policy(&self) -> SchedulingPolicy {
        self.scheduling_policy
    }

    /// 统计就绪任务数量
    pub fn ready_task_count(&self) -> usize {
        match self.scheduling_policy {
            SchedulingPolicy::FIFO => self.ready_queue.len(),
            SchedulingPolicy::Priority | SchedulingPolicy::RoundRobin => {
                self.priority_queues.iter().map(|q| q.len()).sum()
            },
            SchedulingPolicy::CFS => self.cfs_queue.len(),
        }
    }

    /// 根据PID查找任务（搜索所有可能的位置）
    pub fn find_task_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        // 搜索就绪队列
        match self.scheduling_policy {
            SchedulingPolicy::FIFO => {
                for task in &self.ready_queue {
                    if task.get_pid() == pid {
                        return Some(task.clone());
                    }
                }
            },
            SchedulingPolicy::Priority | SchedulingPolicy::RoundRobin => {
                for queue in &self.priority_queues {
                    for task in queue {
                        if task.get_pid() == pid {
                            return Some(task.clone());
                        }
                    }
                }
            },
            SchedulingPolicy::CFS => {
                for cfs_task in &self.cfs_queue {
                    if cfs_task.task.get_pid() == pid {
                        return Some(cfs_task.task.clone());
                    }
                }
            }
        }

        // 检查初始进程
        if let Some(ref init_proc) = self.init_proc {
            if init_proc.get_pid() == pid {
                return Some(init_proc.clone());
            }
        }

        None
    }
    
    /// 获取调度统计信息
    pub fn get_stats(&self) -> &SchedulerStats {
        &self.stats
    }
    
    /// 更新统计信息
    fn update_stats(&mut self) {
        let ready = self.ready_task_count();
        let running = 1; // 简化：假设当前只有一个运行任务
        let blocked = 0; // 简化：暂时不统计阻塞任务
        self.stats.update_task_counts(ready, running, blocked);
    }
    
    /// 重置统计信息
    pub fn reset_stats(&mut self) {
        self.stats = SchedulerStats::new();
    }
    
    /// 获取调度效率信息
    pub fn get_efficiency_info(&self) -> (f32, u64, usize) {
        let avg_usage = self.stats.avg_time_slice_usage;
        let switches = self.stats.context_switches;
        let total_tasks = self.stats.total_tasks;
        (avg_usage, switches, total_tasks)
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

pub fn set_init_proc(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().set_init_proc(task);
}

pub fn get_init_proc() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER
        .exclusive_access()
        .init_proc
        .as_ref()
        .map(|f| f.clone())
}

/// 设置调度策略
pub fn set_scheduling_policy(policy: SchedulingPolicy) {
    TASK_MANAGER.exclusive_access().set_scheduling_policy(policy);
}

/// 获取当前调度策略
pub fn get_scheduling_policy() -> SchedulingPolicy {
    TASK_MANAGER.exclusive_access().get_scheduling_policy()
}

/// 更新任务运行时间统计
pub fn update_task_runtime(task: &Arc<TaskControlBlock>, runtime_us: u64) {
    TASK_MANAGER.exclusive_access().update_task_runtime(task, runtime_us);
}

/// 获取调度统计信息
pub fn get_scheduler_stats() -> SchedulerStats {
    TASK_MANAGER.exclusive_access().get_stats().clone()
}

/// 获取就绪任务数量
pub fn ready_task_count() -> usize {
    TASK_MANAGER.exclusive_access().ready_task_count()
}

/// 重置调度统计信息
pub fn reset_scheduler_stats() {
    TASK_MANAGER.exclusive_access().reset_stats();
}

/// 获取调度效率信息
pub fn get_scheduler_efficiency() -> (f32, u64, usize) {
    TASK_MANAGER.exclusive_access().get_efficiency_info()
}

/// 唤醒任务，将其从睡眠状态转为就绪状态
pub fn wakeup_task(task: Arc<TaskControlBlock>) {
    let mut inner = task.inner_exclusive_access();
    if inner.sched.task_status == TaskStatus::Sleeping {
        inner.sched.task_status = TaskStatus::Ready;
        drop(inner);
        // 将任务添加到就绪队列
        add_task(task);
    }
}

/// 根据PID查找任务，包括当前运行的任务
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    // 首先检查当前运行的任务
    if let Some(current) = crate::task::processor::current_task() {
        if current.get_pid() == pid {
            return Some(current);
        }
    }

    // 搜索任务管理器中的任务
    TASK_MANAGER.exclusive_access().find_task_by_pid(pid)
}
