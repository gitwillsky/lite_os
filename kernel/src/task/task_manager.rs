use alloc::{sync::Arc, vec::Vec};

use crate::{
    task::{
        multicore::CORE_MANAGER,
        task::TaskControlBlock,
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

/// 添加任务到多核心调度器
pub fn add_task(task: Arc<TaskControlBlock>) {
    CORE_MANAGER.add_task(task);
}

/// 获取可调度任务数量
pub fn schedulable_task_count() -> usize {
    CORE_MANAGER.total_task_count()
}

/// 获取init进程
pub fn init_proc() -> Option<Arc<TaskControlBlock>> {
    // 从多核心管理器的全局任务列表中查找init进程
    CORE_MANAGER.get_all_tasks()
        .into_iter()
        .find(|task| task.pid() == crate::task::pid::INIT_PID)
}

/// 根据PID查找任务
pub fn find_task_by_pid(pid: usize) -> Option<Arc<TaskControlBlock>> {
    CORE_MANAGER.get_all_tasks()
        .into_iter()
        .find(|task| task.pid() == pid)
}

/// 获取所有任务的列表
pub fn get_all_tasks() -> Vec<Arc<TaskControlBlock>> {
    let mut tasks = CORE_MANAGER.get_all_tasks();
    
    // 添加睡眠中的任务
    tasks.extend(crate::timer::get_sleeping_tasks());
    
    tasks
}

/// 设置调度策略（当前简化实现，未来可扩展到各核心独立设置）
pub fn set_scheduling_policy(_policy: SchedulingPolicy) {
    // TODO: 为每个核心设置调度策略
    debug!("Scheduling policy change requested (not implemented for multi-core yet)");
}

/// 获取当前调度策略
pub fn get_scheduling_policy() -> SchedulingPolicy {
    // 默认返回CFS，实际可从各核心查询
    SchedulingPolicy::CFS
}
