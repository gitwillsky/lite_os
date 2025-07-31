/// 统一的任务管理器
///
/// 这个模块是系统中所有进程管理的中心，提供统一的抽象接口。
/// 它隐藏了进程在不同状态下的存储细节（调度器队列、睡眠队列、当前运行等），
/// 对外只暴露简洁的进程管理API。
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use lazy_static::lazy_static;
use spin::RwLock;

use crate::{
    arch::hart::MAX_CORES,
    task::{multicore::CORE_MANAGER, TaskControlBlock, TaskStatus}, timer::get_time_ns,
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
/// 睡眠管理现在直接基于进程的wake_time_ns字段，不需要单独存储。
pub struct TaskManager {
    /// 全局进程表：PID -> TaskControlBlock
    /// 这里存储系统中所有进程，无论其状态如何
    processes: RwLock<BTreeMap<usize, Arc<TaskControlBlock>>>,

    /// init 进程的引用，用于特殊处理
    init_process: RwLock<Option<Arc<TaskControlBlock>>>,

    /// 当前的调度策略
    scheduling_policy: RwLock<SchedulingPolicy>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self {
            processes: RwLock::new(BTreeMap::new()),
            init_process: RwLock::new(None),
            scheduling_policy: RwLock::new(SchedulingPolicy::CFS),
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
                // 睡眠任务通过wake_time_ns字段管理，无需额外处理
            }
            TaskStatus::Running => {
                // 运行中的任务已经在某个核心上，不需要添加到调度器
            }
            TaskStatus::Zombie => {
                // 僵尸进程不需要调度
            }
        }

    }

    /// 从系统中移除进程
    /// 这是进程回收的统一入口点
    pub fn remove_process(&self, pid: usize) -> Option<Arc<TaskControlBlock>> {
        let mut processes = self.processes.write();
        if let Some(task) = processes.remove(&pid) {
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
    }

    /// 获取当前调度策略
    pub fn get_scheduling_policy(&self) -> SchedulingPolicy {
        *self.scheduling_policy.read()
    }

    /// 更新进程状态
    /// 当进程状态发生变化时，需要调用此函数来维护一致性
    pub fn update_process_status(
        &self,
        pid: usize,
        old_status: TaskStatus,
        new_status: TaskStatus,
    ) {
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
                    warn!(
                        "Process PID {} claims to be running but not found on any core",
                        pid
                    );
                }
            }
        }
    }

    /// 添加任务到睡眠状态
    pub fn add_sleeping_task(&self, task: Arc<TaskControlBlock>, wake_time_ns: u64) {
        // 直接在任务的wake_time_ns字段设置唤醒时间
        task.wake_time_ns
            .store(wake_time_ns, core::sync::atomic::Ordering::Relaxed);
    }

    /// 获取所有睡眠任务
    pub fn get_sleeping_tasks(&self) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.processes.read();
        processes
            .values()
            .filter(|task| {
                *task.task_status.lock() == TaskStatus::Sleeping
                    && task
                        .wake_time_ns
                        .load(core::sync::atomic::Ordering::Relaxed)
                        > 0
            })
            .cloned()
            .collect()
    }

    /// 检查并唤醒到期的睡眠任务
    /// 返回被唤醒的任务列表
    pub fn check_and_wakeup_sleeping_tasks(
        &self,
        current_time_ns: u64,
    ) -> Vec<Arc<TaskControlBlock>> {
        let processes = self.processes.read();
        let mut awakened_tasks = Vec::new();

        // 遍历所有进程，检查睡眠状态的进程是否到期
        for task in processes.values() {
            if *task.task_status.lock() == TaskStatus::Sleeping {
                let wake_time = task
                    .wake_time_ns
                    .load(core::sync::atomic::Ordering::Relaxed);
                if wake_time > 0 && wake_time <= current_time_ns {
                    // 清零唤醒时间，表示不再睡眠
                    task.wake_time_ns
                        .store(0, core::sync::atomic::Ordering::Relaxed);
                    awakened_tasks.push(task.clone());
                }
            }
        }
        awakened_tasks
    }

    /// 从睡眠状态中移除指定任务（用于提前唤醒）
    pub fn remove_sleeping_task(&self, task_pid: usize) -> bool {
        if let Some(task) = self.find_process_by_pid(task_pid) {
            if *task.task_status.lock() == TaskStatus::Sleeping {
                // 清零唤醒时间，表示不再睡眠
                task.wake_time_ns
                    .store(0, core::sync::atomic::Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    /// 获取睡眠任务数量
    pub fn get_sleeping_task_count(&self) -> usize {
        let processes = self.processes.read();
        processes
            .values()
            .filter(|task| {
                *task.task_status.lock() == TaskStatus::Sleeping
                    && task
                        .wake_time_ns
                        .load(core::sync::atomic::Ordering::Relaxed)
                        > 0
            })
            .count()
    }
}

// 全局统一任务管理器实例
lazy_static! {
    pub static ref TASK_MANAGER: TaskManager = TaskManager::new();
}

// 对外统一接口函数
// 这些函数隐藏了内部实现细节，提供简洁的API

/// 添加任务到系统
pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.add_process(task);
}

/// 根据PID查找任务
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.find_process_by_pid(pid)
}

/// 获取所有任务
pub fn get_all_tasks() -> Vec<Arc<TaskControlBlock>> {
    TASK_MANAGER.get_all_processes()
}

/// 获取所有PID
pub fn get_all_pids() -> Vec<usize> {
    TASK_MANAGER.get_all_pids()
}

/// 获取任务数量
pub fn get_task_count() -> usize {
    TASK_MANAGER.get_process_count()
}

/// 获取init进程
pub fn init_proc() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.get_init_process()
}

/// 获取进程统计信息
pub fn get_process_statistics() -> ProcessStats {
    TASK_MANAGER.get_process_stats()
}

/// 设置调度策略
pub fn set_scheduling_policy(policy: SchedulingPolicy) {
    TASK_MANAGER.set_scheduling_policy(policy);
}

/// 获取调度策略
pub fn get_scheduling_policy() -> SchedulingPolicy {
    TASK_MANAGER.get_scheduling_policy()
}

/// 移除任务（用于进程回收）
pub fn remove_task(pid: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.remove_process(pid)
}

/// 更新任务状态
pub fn update_task_status(pid: usize, old_status: TaskStatus, new_status: TaskStatus) {
    TASK_MANAGER.update_process_status(pid, old_status, new_status);
}

/// 同步所有任务状态
pub fn sync_all_task_states() {
    TASK_MANAGER.sync_all_process_states();
}

/// 获取在特定核心上运行的任务
pub fn get_task_on_core(core_id: usize) -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.get_process_on_core(core_id)
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

/// 添加任务到睡眠队列
pub fn add_sleeping_task(task: Arc<TaskControlBlock>, wake_time_ns: u64) {
    TASK_MANAGER.add_sleeping_task(task, wake_time_ns);
}

/// 获取所有睡眠任务
pub fn get_sleeping_tasks() -> Vec<Arc<TaskControlBlock>> {
    TASK_MANAGER.get_sleeping_tasks()
}

/// 检查并唤醒到期的睡眠任务
pub fn check_and_wakeup_sleeping_tasks(current_time_ns: u64) -> Vec<Arc<TaskControlBlock>> {
    let awakened_tasks = TASK_MANAGER.check_and_wakeup_sleeping_tasks(current_time_ns);

    // 将唤醒的任务状态设置为Ready并添加到调度器
    for task in &awakened_tasks {
        set_task_status(task, TaskStatus::Ready);
        CORE_MANAGER.add_task(task.clone());
    }

    awakened_tasks
}

/// 从睡眠队列中移除指定任务
pub fn remove_sleeping_task(task_pid: usize) -> bool {
    TASK_MANAGER.remove_sleeping_task(task_pid)
}

/// 获取睡眠任务数量
pub fn get_sleeping_task_count() -> usize {
    TASK_MANAGER.get_sleeping_task_count()
}

/// 获取可调度任务数量（用于调试）
pub fn schedulable_task_count() -> usize {
    // 返回Ready和Running状态的任务数量
    let process_stats = TASK_MANAGER.get_process_stats();
    (process_stats.ready + process_stats.running) as usize
}

// nanosleep 实现
pub fn nanosleep(nanoseconds: u64) -> isize {
    if nanoseconds == 0 {
        return 0;
    }

    let start_time = get_time_ns();

    // 无论时间长短，都使用睡眠队列来保证准确性
    if let Some(current_task) = crate::task::current_task() {
        let wake_time = start_time + nanoseconds;

        // 使用统一的任务状态更新方法
        crate::task::set_task_status(&current_task, crate::task::TaskStatus::Sleeping);

        // 将当前任务加入睡眠队列
        add_sleeping_task(current_task, wake_time);

        // 让出CPU，等待被唤醒（此时任务状态为Sleeping，不会被重新加入就绪队列）
        crate::task::block_current_and_run_next();

        // 醒来后检查实际时间
        let end_time = get_time_ns();
        let actual_sleep = end_time - start_time;
    } else {
        // 如果没有当前任务，使用忙等待（不推荐，但作为备用方案）
        let start_time = get_time_ns();
        while get_time_ns() - start_time < nanoseconds {
            // 忙等待
        }
    }

    0
}
