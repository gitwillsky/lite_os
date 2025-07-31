/// 统一的任务管理器
///
/// 这个模块是系统中所有进程管理的中心，提供统一的抽象接口。
/// 它隐藏了进程在不同状态下的存储细节（调度器队列、睡眠队列、当前运行等），
/// 对外只暴露简洁的进程管理API。

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use spin::RwLock;
use lazy_static::lazy_static;

use crate::{
    arch::hart::MAX_CORES,
    task::{TaskControlBlock, TaskStatus, multicore::CORE_MANAGER},
};

/// 调度策略
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SchedulingPolicy {
    FIFO,
    RoundRobin,
    Priority,
    CFS,
}

/// 进程状态统计
#[derive(Debug, Clone, Copy)]
pub struct ProcessStats {
    pub total: u32,
    pub running: u32,
    pub ready: u32,
    pub sleeping: u32,
    pub zombie: u32,
}

/// 统一的任务管理器
///
/// 这是系统中唯一的进程状态权威源，所有其他组件都通过这个管理器来操作进程。
pub struct TaskManager {
    /// 全局进程表：PID -> TaskControlBlock
    /// 这里存储系统中所有进程，无论其状态如何
    processes: RwLock<BTreeMap<usize, Arc<TaskControlBlock>>>,

    /// init 进程的引用，用于特殊处理
    init_process: RwLock<Option<Arc<TaskControlBlock>>>,

    /// 当前的调度策略
    scheduling_policy: RwLock<SchedulingPolicy>,

    /// 睡眠任务队列：以唤醒时间（纳秒）为键，任务为值
    /// 迁移自 timer 模块，现在统一在这里管理
    sleeping_tasks: RwLock<BTreeMap<u64, Arc<TaskControlBlock>>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            processes: RwLock::new(BTreeMap::new()),
            init_process: RwLock::new(None),
            scheduling_policy: RwLock::new(SchedulingPolicy::CFS),
            sleeping_tasks: RwLock::new(BTreeMap::new()),
        }
    }

    /// 添加新进程到系统
    /// 这是创建进程的统一入口点
    pub fn add_process(&self, task: Arc<TaskControlBlock>) {
        let pid = task.pid();

        // 添加到全局进程表
        {
            let mut processes = self.processes.write();
            processes.insert(pid, task.clone());
        }

        // 如果是 init 进程，特别记录
        if pid == crate::task::pid::INIT_PID {
            *self.init_process.write() = Some(task.clone());
        }

        // 添加到多核调度器（根据当前状态）
        let status = *task.task_status.lock();
        match status {
            TaskStatus::Ready => {
                CORE_MANAGER.add_task(task);
            }
            TaskStatus::Sleeping => {
                // 睡眠任务由 timer 模块管理，这里不做处理
            }
            TaskStatus::Running => {
                // 运行中的任务已经在某个核心上，不需要添加到调度器
            }
            TaskStatus::Zombie => {
                // 僵尸进程不需要调度
            }
        }

        debug!("Added process PID {} to unified task manager", pid);
    }

    /// 从系统中移除进程
    /// 这是进程回收的统一入口点
    pub fn remove_process(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        let mut processes = self.processes.write();
        if let Some(task) = processes.remove(&pid) {
            debug!("Removed process PID {} from unified task manager", pid);
            Some(task)
        } else {
            None
        }
    }

    /// 根据 PID 查找进程
    /// 这是查找进程的统一接口，性能优化的O(log n)查找
    pub fn find_process_by_pid(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        let processes = self.processes.read();
        processes.get(&pid).cloned()
    }

    /// 获取所有进程
    /// 这是获取进程列表的统一接口
    pub fn get_all_processes(&self) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.processes.read();
        processes.values().cloned().collect()
    }

    /// 获取所有进程的 PID 列表
    pub fn get_all_pids(&self) -> Vec<usize> {
        let processes = self.processes.read();
        processes.keys().cloned().collect()
    }

    /// 获取进程总数
    pub fn get_process_count(&self) -> usize {
        let processes = self.processes.read();
        processes.len()
    }

    /// 获取 init 进程
    pub fn get_init_process(&self) -> Option<Arc<TaskControlBlock>> {
        let init_proc = self.init_process.read();
        init_proc.clone()
    }

    /// 获取进程统计信息
    /// 统一计算各种状态的进程数量
    pub fn get_process_stats(&self) -> ProcessStats {
        let processes = self.processes.read();

        let mut running = 0u32;
        let mut ready = 0u32;
        let mut sleeping = 0u32;
        let mut zombie = 0u32;

        for task in processes.values() {
            let status = *task.task_status.lock();
            match status {
                TaskStatus::Running => running += 1,
                TaskStatus::Ready => ready += 1,
                TaskStatus::Sleeping => sleeping += 1,
                TaskStatus::Zombie => zombie += 1,
            }
        }

        ProcessStats {
            total: processes.len() as u32,
            running,
            ready,
            sleeping,
            zombie,
        }
    }

    /// 获取特定状态的进程
    pub fn get_processes_by_status(&self, status: TaskStatus) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.processes.read();
        processes
            .values()
            .filter(|task| *task.task_status.lock() == status)
            .cloned()
            .collect()
    }

    /// 获取在特定核心上运行的进程
    pub fn get_process_on_core(&self, core_id: usize) -> Option<Arc<TaskControlBlock>> {
        if let Some(processor) = CORE_MANAGER.get_processor(core_id) {
            let proc = processor.lock();
            proc.current.clone()
        } else {
            None
        }
    }

    /// 设置调度策略
    pub fn set_scheduling_policy(&self, policy: SchedulingPolicy) {
        *self.scheduling_policy.write() = policy;
        debug!("Scheduling policy changed to {:?}", policy);
    }

    /// 获取当前调度策略
    pub fn get_scheduling_policy(&self) -> SchedulingPolicy {
        *self.scheduling_policy.read()
    }

    /// 更新进程状态
    /// 当进程状态发生变化时，需要调用此函数来维护一致性
    pub fn update_process_status(&self, pid: usize, old_status: TaskStatus, new_status: TaskStatus) {
        if let Some(task) = self.find_process_by_pid(pid) {
            // 根据状态变化进行相应的调度器操作
            match (old_status, new_status) {
                (TaskStatus::Ready, TaskStatus::Running) => {
                    // 从调度器队列移动到某个核心的current，由调度器处理
                }
                (TaskStatus::Running, TaskStatus::Ready) => {
                    // 从某个核心的current移动到调度器队列
                    CORE_MANAGER.add_task(task);
                }
                (TaskStatus::Running, TaskStatus::Sleeping) => {
                    // 从某个核心的current移动到睡眠队列，由 timer 模块处理
                }
                (TaskStatus::Sleeping, TaskStatus::Ready) => {
                    // 从睡眠队列移动到调度器队列
                    CORE_MANAGER.add_task(task);
                }
                (_, TaskStatus::Zombie) => {
                    // 进程退出，不需要调度
                }
                _ => {
                    // 其他状态转换
                }
            }

            debug!("Process PID {} status updated: {:?} -> {:?}", pid, old_status, new_status);
        }
    }

    /// 同步所有进程状态
    /// 用于确保进程表与实际状态的一致性
    pub fn sync_all_process_states(&self) {
        let processes = self.processes.read();
        for task in processes.values() {
            let pid = task.pid();
            let current_status = *task.task_status.lock();

            // 这里可以添加状态一致性检查的逻辑
            // 例如检查声称在运行的进程是否真的在某个核心上
            if current_status == TaskStatus::Running {
                let mut found_on_core = false;
                for i in 0..MAX_CORES {
                    if let Some(running_task) = self.get_process_on_core(i) {
                        if running_task.pid() == pid {
                            found_on_core = true;
                            break;
                        }
                    }
                }
                if !found_on_core {
                    warn!("Process PID {} claims to be running but not found on any core", pid);
                }
            }
        }
    }

    /// === 睡眠任务管理方法 ===

    /// 添加任务到睡眠队列
    pub fn add_sleeping_task(&self, task: Arc<TaskControlBlock>, wake_time_ns: u64) {
        let mut sleeping_tasks = self.sleeping_tasks.write();

        // 避免时间冲突：如果已存在相同时间，则递增1纳秒
        let mut actual_wake_time = wake_time_ns;
        while sleeping_tasks.contains_key(&actual_wake_time) {
            actual_wake_time += 1;
        }

        sleeping_tasks.insert(actual_wake_time, task.clone());
        debug!("Added task PID {} to sleep queue, wake time: {} ns", task.pid(), actual_wake_time);
    }

    /// 获取所有睡眠任务
    pub fn get_sleeping_tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let sleeping_tasks = self.sleeping_tasks.read();
        sleeping_tasks.values().cloned().collect()
    }

    /// 检查并唤醒到期的睡眠任务
    /// 返回被唤醒的任务列表
    pub fn check_and_wakeup_sleeping_tasks(&self, current_time_ns: u64) -> Vec<Arc<TaskControlBlock>> {
        // 使用 try_write 避免死锁
        if let Some(mut sleeping_tasks) = self.sleeping_tasks.try_write() {
            let mut awakened_tasks = Vec::new();
            let mut keys_to_remove = Vec::new();

            // 收集需要唤醒的任务
            for (&wake_time, task) in sleeping_tasks.iter() {
                if wake_time <= current_time_ns {
                    awakened_tasks.push(task.clone());
                    keys_to_remove.push(wake_time);
                } else {
                    // BTreeMap是有序的，后面的都不会到期
                    break;
                }
            }

            // 从睡眠队列中移除
            for key in keys_to_remove {
                sleeping_tasks.remove(&key);
            }

            if !awakened_tasks.is_empty() {
                debug!("Awakened {} sleeping tasks", awakened_tasks.len());
            }

            awakened_tasks
        } else {
            // 如果获取锁失败，返回空列表
            Vec::new()
        }
    }

    /// 从睡眠队列中移除指定任务（用于提前唤醒）
    pub fn remove_sleeping_task(&self, task_pid: usize) -> bool {
        let mut sleeping_tasks = self.sleeping_tasks.write();

        // 需要遍历查找，因为我们只知道PID不知道wake_time
        let mut key_to_remove = None;
        for (&wake_time, task) in sleeping_tasks.iter() {
            if task.pid() == task_pid {
                key_to_remove = Some(wake_time);
                break;
            }
        }

        if let Some(key) = key_to_remove {
            sleeping_tasks.remove(&key);
            debug!("Removed task PID {} from sleep queue", task_pid);
            true
        } else {
            false
        }
    }

    /// 获取睡眠队列中的任务数量
    pub fn get_sleeping_task_count(&self) -> usize {
        let sleeping_tasks = self.sleeping_tasks.read();
        sleeping_tasks.len()
    }
}

// 全局统一任务管理器实例
lazy_static! {
    pub static ref UNIFIED_TASK_MANAGER: TaskManager = TaskManager::new();
}

// 对外统一接口函数
// 这些函数隐藏了内部实现细节，提供简洁的API

/// 添加任务到系统
pub fn add_task(task: Arc<TaskControlBlock>) {
    UNIFIED_TASK_MANAGER.add_process(task);
}

/// 根据PID查找任务
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    UNIFIED_TASK_MANAGER.find_process_by_pid(pid)
}

/// 获取所有任务
pub fn get_all_tasks() -> Vec<Arc<TaskControlBlock>> {
    UNIFIED_TASK_MANAGER.get_all_processes()
}

/// 获取所有PID
pub fn get_all_pids() -> Vec<usize> {
    UNIFIED_TASK_MANAGER.get_all_pids()
}

/// 获取任务数量
pub fn get_task_count() -> usize {
    UNIFIED_TASK_MANAGER.get_process_count()
}

/// 获取init进程
pub fn init_proc() -> Option<Arc<TaskControlBlock>> {
    UNIFIED_TASK_MANAGER.get_init_process()
}

/// 获取进程统计信息
pub fn get_process_statistics() -> ProcessStats {
    UNIFIED_TASK_MANAGER.get_process_stats()
}

/// 设置调度策略
pub fn set_scheduling_policy(policy: SchedulingPolicy) {
    UNIFIED_TASK_MANAGER.set_scheduling_policy(policy);
}

/// 获取调度策略
pub fn get_scheduling_policy() -> SchedulingPolicy {
    UNIFIED_TASK_MANAGER.get_scheduling_policy()
}

/// 移除任务（用于进程回收）
pub fn remove_task(pid: usize) -> Option<Arc<TaskControlBlock>> {
    UNIFIED_TASK_MANAGER.remove_process(pid)
}

/// 更新任务状态
pub fn update_task_status(pid: usize, old_status: TaskStatus, new_status: TaskStatus) {
    UNIFIED_TASK_MANAGER.update_process_status(pid, old_status, new_status);
}

/// 同步所有任务状态
pub fn sync_all_task_states() {
    UNIFIED_TASK_MANAGER.sync_all_process_states();
}

/// 获取在特定核心上运行的任务
pub fn get_task_on_core(core_id: usize) -> Option<Arc<TaskControlBlock>> {
    UNIFIED_TASK_MANAGER.get_process_on_core(core_id)
}

/// 安全的状态更新函数
/// 这个函数应该被用来替代直接修改 task.task_status
pub fn set_task_status(task: &Arc<TaskControlBlock>, new_status: TaskStatus) {
    let old_status = {
        let mut status_guard = task.task_status.lock();
        let old = *status_guard;
        *status_guard = new_status;
        old
    };

    // 通知统一任务管理器状态已改变
    if old_status != new_status {
        update_task_status(task.pid(), old_status, new_status);
    }
}

// === 睡眠任务管理的便利函数 ===

/// 添加任务到睡眠队列
pub fn add_sleeping_task(task: Arc<TaskControlBlock>, wake_time_ns: u64) {
    UNIFIED_TASK_MANAGER.add_sleeping_task(task, wake_time_ns);
}

/// 获取所有睡眠任务
pub fn get_sleeping_tasks() -> Vec<Arc<TaskControlBlock>> {
    UNIFIED_TASK_MANAGER.get_sleeping_tasks()
}

/// 检查并唤醒到期的睡眠任务
pub fn check_and_wakeup_sleeping_tasks(current_time_ns: u64) -> Vec<Arc<TaskControlBlock>> {
    let awakened_tasks = UNIFIED_TASK_MANAGER.check_and_wakeup_sleeping_tasks(current_time_ns);

    // 将唤醒的任务状态设置为Ready并添加到调度器
    for task in &awakened_tasks {
        set_task_status(task, TaskStatus::Ready);
        CORE_MANAGER.add_task(task.clone());
    }

    awakened_tasks
}

/// 从睡眠队列中移除指定任务
pub fn remove_sleeping_task(task_pid: usize) -> bool {
    UNIFIED_TASK_MANAGER.remove_sleeping_task(task_pid)
}

/// 获取睡眠任务数量
pub fn get_sleeping_task_count() -> usize {
    UNIFIED_TASK_MANAGER.get_sleeping_task_count()
}

/// 获取可调度任务数量（用于调试）
pub fn schedulable_task_count() -> usize {
    // 返回Ready和Running状态的任务数量
    let process_stats = UNIFIED_TASK_MANAGER.get_process_stats();
    (process_stats.ready + process_stats.running) as usize
}