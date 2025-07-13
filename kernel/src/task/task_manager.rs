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
        let vruntime = task.inner_exclusive_access().vruntime;
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
                let task_vruntime = task_inner.vruntime;
                drop(task_inner);
                
                // 如果任务的vruntime太小，将其设置为当前最小值
                if task_vruntime < self.min_vruntime {
                    let mut task_inner = task.inner_exclusive_access();
                    task_inner.vruntime = self.min_vruntime;
                    drop(task_inner);
                }
                
                self.cfs_queue.push(CFSTask::new(task));
            }
        }
    }

    pub fn fetch_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        match self.scheduling_policy {
            SchedulingPolicy::FIFO => {
                self.ready_queue.pop_front()
            },
            SchedulingPolicy::Priority | SchedulingPolicy::RoundRobin => {
                // 从高优先级到低优先级查找任务
                for queue in &mut self.priority_queues {
                    if let Some(task) = queue.pop_front() {
                        return Some(task);
                    }
                }
                None
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
        }
    }

    /// 更新任务的运行时间统计
    pub fn update_task_runtime(&mut self, task: &Arc<TaskControlBlock>, runtime_us: u64) {
        let mut task_inner = task.inner_exclusive_access();
        
        match self.scheduling_policy {
            SchedulingPolicy::CFS => {
                task_inner.update_vruntime(runtime_us);
                task_inner.last_runtime = runtime_us;
            },
            _ => {
                task_inner.last_runtime = runtime_us;
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

    /// 根据PID查找任务（仅搜索任务管理器中的队列）
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

/// 获取就绪任务数量
pub fn ready_task_count() -> usize {
    TASK_MANAGER.exclusive_access().ready_task_count()
}

/// 唤醒任务，将其从睡眠状态转为就绪状态
pub fn wakeup_task(task: Arc<TaskControlBlock>) {
    let mut inner = task.inner_exclusive_access();
    if inner.task_status == TaskStatus::Sleeping {
        inner.task_status = TaskStatus::Ready;
        drop(inner);
        // 将任务添加到就绪队列
        add_task(task);
    }
}

/// 根据PID查找任务，包括当前运行的任务
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    // 搜索任务管理器中的任务
    TASK_MANAGER.exclusive_access().find_task_by_pid(pid)
}
