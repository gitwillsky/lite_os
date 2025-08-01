use core::sync::atomic::{AtomicUsize, Ordering};
use alloc::collections::BTreeMap;
use spin::RwLock;

use crate::task::{TaskControlBlock, TaskStatus};
use super::signal_manager::{SIGNAL_EVENT_BUS, SignalEvent, notify_task_status_change};

/// 任务状态转换事件
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaskStateEvent {
    /// 任务被创建
    Created { pid: usize, parent_pid: Option<usize> },
    /// 任务状态改变
    StatusChanged { pid: usize, old_status: TaskStatus, new_status: TaskStatus },
    /// 任务退出
    Exited { pid: usize, exit_code: i32 },
    /// 任务被停止（信号）
    Stopped { pid: usize, signal: u8 },
    /// 任务被恢复（信号）
    Resumed { pid: usize },
    /// 任务睡眠
    Sleeping { pid: usize, wake_time_ns: u64 },
    /// 任务被唤醒
    Awakened { pid: usize },
}

/// 任务状态统计
#[derive(Debug, Clone, Default)]
pub struct TaskStatusStats {
    pub ready_count: usize,
    pub running_count: usize,
    pub sleeping_count: usize,
    pub stopped_count: usize,
    pub zombie_count: usize,
    pub total_count: usize,
}

/// 任务状态转换规则验证器
pub struct TaskStateTransitionValidator;

impl TaskStateTransitionValidator {
    /// 检查状态转换是否有效
    pub fn is_valid_transition(from: TaskStatus, to: TaskStatus) -> bool {
        use TaskStatus::*;

        match (from, to) {
            // Ready 状态可以转换到任何状态
            (Ready, _) => true,

            // Running 状态可以转换到除自身外的任何状态
            (Running, Running) => false,
            (Running, _) => true,

            // Sleeping 状态可以被唤醒或被信号停止/终止
            (Sleeping, Ready) => true,     // 正常唤醒
            (Sleeping, Stopped) => true,   // 被信号停止
            (Sleeping, Zombie) => true,    // 被信号终止
            (Sleeping, _) => false,

            // Stopped 状态只能被恢复或终止
            (Stopped, Ready) => true,      // SIGCONT 恢复
            (Stopped, Zombie) => true,     // 被致命信号终止
            (Stopped, _) => false,

            // Zombie 状态是最终状态，不能转换
            (Zombie, _) => false,
        }
    }

    /// 获取状态转换的描述
    pub fn get_transition_description(from: TaskStatus, to: TaskStatus) -> &'static str {
        use TaskStatus::*;

        match (from, to) {
            // From Ready state
            (Ready, Running) => "Task scheduled for execution",
            (Ready, Sleeping) => "Task entered sleep/wait state from ready",
            (Ready, Stopped) => "Ready task stopped by signal",
            (Ready, Zombie) => "Ready task terminated by signal",
            
            // From Running state
            (Running, Ready) => "Task preempted or yielded",
            (Running, Sleeping) => "Task entered sleep/wait state",
            (Running, Stopped) => "Running task stopped by signal",
            (Running, Zombie) => "Running task terminated",
            
            // From Sleeping state
            (Sleeping, Ready) => "Task awakened from sleep",
            (Sleeping, Stopped) => "Sleeping task stopped by signal",
            (Sleeping, Zombie) => "Sleeping task terminated by signal",
            
            // From Stopped state
            (Stopped, Ready) => "Stopped task resumed by SIGCONT",
            (Stopped, Zombie) => "Stopped task terminated by signal",
            
            // From Zombie state (should not happen)
            (Zombie, _) => "Invalid transition from zombie state",
            
            // Same state transitions
            (Ready, Ready) => "Task remains in ready state",
            (Running, Running) => "Invalid: task already running",
            (Sleeping, Sleeping) => "Task remains sleeping",
            (Stopped, Stopped) => "Task remains stopped",
            (Zombie, Zombie) => "Task remains zombie",
            
            // Any other combinations
            _ => "Unsupported state transition",
        }
    }
}

/// 任务状态管理器
/// 提供统一的任务状态管理和转换逻辑
pub struct TaskStateManager {
    /// 任务状态统计
    stats: RwLock<TaskStatusStats>,
    /// 状态转换计数器
    transition_counter: AtomicUsize,
    /// 每种状态的任务计数
    status_counts: RwLock<BTreeMap<TaskStatus, usize>>,
}

impl TaskStateManager {
    pub const fn new() -> Self {
        Self {
            stats: RwLock::new(TaskStatusStats {
                ready_count: 0,
                running_count: 0,
                sleeping_count: 0,
                stopped_count: 0,
                zombie_count: 0,
                total_count: 0,
            }),
            transition_counter: AtomicUsize::new(0),
            status_counts: RwLock::new(BTreeMap::new()),
        }
    }

    /// 更新任务状态（带验证和统计）
    pub fn update_task_status(&self, task: &TaskControlBlock, new_status: TaskStatus) -> Result<TaskStatus, &'static str> {
        let old_status = *task.task_status.lock();

        // 验证状态转换
        if !TaskStateTransitionValidator::is_valid_transition(old_status, new_status) {
            return Err("Invalid task status transition");
        }

        // 执行状态转换
        *task.task_status.lock() = new_status;

        // 更新统计信息
        self.update_statistics(old_status, new_status);

        // 递增转换计数器
        self.transition_counter.fetch_add(1, Ordering::Relaxed);

        // 发布状态转换事件
        notify_task_status_change(task.pid(), old_status, new_status);

        // 记录状态转换（调试用）
        debug!("Task PID {} status changed: {:?} -> {:?} ({})",
               task.pid(), old_status, new_status,
               TaskStateTransitionValidator::get_transition_description(old_status, new_status));

        Ok(old_status)
    }

    /// 安全地停止任务（处理信号停止）
    pub fn stop_task(&self, task: &TaskControlBlock, preserve_sleep: bool) -> Result<(), &'static str> {
        let old_status = *task.task_status.lock();

        // 保存停止前的状态
        *task.prev_status_before_stop.lock() = Some(old_status);

        // 如果是睡眠状态且需要保留睡眠信息，不清除唤醒时间
        if !preserve_sleep || old_status != TaskStatus::Sleeping {
            task.wake_time_ns.store(0, Ordering::Relaxed);
        }

        // 转换到停止状态
        self.update_task_status(task, TaskStatus::Stopped)?;

        Ok(())
    }

    /// 恢复被停止的任务（处理 SIGCONT）
    pub fn resume_task(&self, task: &TaskControlBlock) -> Result<(), &'static str> {
        let current_status = *task.task_status.lock();

        if current_status != TaskStatus::Stopped {
            return Err("Task is not in stopped state");
        }

        // 获取停止前的状态
        let restored_status = task.prev_status_before_stop.lock()
            .take()
            .unwrap_or(TaskStatus::Ready);

        // 恢复到之前的状态
        self.update_task_status(task, restored_status)?;

        debug!("Task PID {} resumed from stopped state to {:?}", task.pid(), restored_status);

        Ok(())
    }

    /// 使任务进入睡眠状态
    pub fn sleep_task(&self, task: &TaskControlBlock, wake_time_ns: u64) -> Result<(), &'static str> {
        // 设置唤醒时间
        task.wake_time_ns.store(wake_time_ns, Ordering::Relaxed);

        // 转换到睡眠状态
        self.update_task_status(task, TaskStatus::Sleeping)?;

        SIGNAL_EVENT_BUS.publish(SignalEvent::TaskStatusChanged {
            pid: task.pid(),
            old_status: TaskStatus::Running,
            new_status: TaskStatus::Sleeping,
        });

        Ok(())
    }

    /// 唤醒睡眠中的任务
    pub fn wakeup_task(&self, task: &TaskControlBlock) -> Result<(), &'static str> {
        let current_status = *task.task_status.lock();

        if current_status != TaskStatus::Sleeping {
            return Err("Task is not sleeping");
        }

        // 清除唤醒时间
        task.wake_time_ns.store(0, Ordering::Relaxed);

        // 转换到就绪状态
        self.update_task_status(task, TaskStatus::Ready)?;

        Ok(())
    }

    /// 终止任务
    pub fn terminate_task(&self, task: &TaskControlBlock, exit_code: i32) -> Result<(), &'static str> {
        // 设置退出码
        task.set_exit_code(exit_code);

        // 转换到僵尸状态
        self.update_task_status(task, TaskStatus::Zombie)?;

        // 发布任务退出事件
        SIGNAL_EVENT_BUS.publish(SignalEvent::TaskExited {
            pid: task.pid(),
            exit_code,
        });

        Ok(())
    }

    /// 更新统计信息（内部方法）
    fn update_statistics(&self, old_status: TaskStatus, new_status: TaskStatus) {
        let mut stats = self.stats.write();
        let mut counts = self.status_counts.write();

        // 减少旧状态计数
        if let Some(count) = counts.get_mut(&old_status) {
            if *count > 0 {
                *count -= 1;
            }
        }

        // 增加新状态计数
        *counts.entry(new_status).or_insert(0) += 1;

        // 更新快速统计
        stats.ready_count = *counts.get(&TaskStatus::Ready).unwrap_or(&0);
        stats.running_count = *counts.get(&TaskStatus::Running).unwrap_or(&0);
        stats.sleeping_count = *counts.get(&TaskStatus::Sleeping).unwrap_or(&0);
        stats.stopped_count = *counts.get(&TaskStatus::Stopped).unwrap_or(&0);
        stats.zombie_count = *counts.get(&TaskStatus::Zombie).unwrap_or(&0);
        stats.total_count = counts.values().sum();
    }

    /// 注册新任务
    pub fn register_task(&self, task: &TaskControlBlock) {
        let status = *task.task_status.lock();

        // 更新统计
        let mut counts = self.status_counts.write();
        *counts.entry(status).or_insert(0) += 1;

        self.update_statistics(status, status); // 触发统计更新

        // 发布任务创建事件
        let parent_pid = task.parent().map(|p| p.pid());

        SIGNAL_EVENT_BUS.publish(SignalEvent::TaskCreated {
            pid: task.pid(),
            parent_pid,
        });
    }

    /// 注销任务（任务完全清理时调用）
    pub fn unregister_task(&self, task: &TaskControlBlock) {
        let status = *task.task_status.lock();

        let mut counts = self.status_counts.write();
        if let Some(count) = counts.get_mut(&status) {
            if *count > 0 {
                *count -= 1;
            }
        }

        // 重新计算统计
        drop(counts);
        self.update_statistics(status, status);
    }

    /// 获取当前统计信息
    pub fn get_statistics(&self) -> TaskStatusStats {
        self.stats.read().clone()
    }

    /// 获取状态转换总数
    pub fn get_transition_count(&self) -> usize {
        self.transition_counter.load(Ordering::Relaxed)
    }

    /// 获取指定状态的任务数量
    pub fn get_status_count(&self, status: TaskStatus) -> usize {
        self.status_counts.read().get(&status).copied().unwrap_or(0)
    }

    /// 验证系统状态一致性（调试用）
    pub fn validate_consistency(&self) -> Result<(), &'static str> {
        let stats = self.stats.read();
        let expected_total = stats.ready_count + stats.running_count +
                           stats.sleeping_count + stats.stopped_count + stats.zombie_count;

        if expected_total != stats.total_count {
            error!("Task statistics inconsistency: expected {}, actual {}",
                   expected_total, stats.total_count);
            return Err("Task statistics inconsistency detected");
        }

        Ok(())
    }
}

/// 全局任务状态管理器
pub static TASK_STATE_MANAGER: TaskStateManager = TaskStateManager::new();

/// 便捷函数：更新任务状态
pub fn update_task_status(task: &TaskControlBlock, new_status: TaskStatus) -> Result<TaskStatus, &'static str> {
    TASK_STATE_MANAGER.update_task_status(task, new_status)
}

/// 便捷函数：停止任务
pub fn stop_task(task: &TaskControlBlock, preserve_sleep: bool) -> Result<(), &'static str> {
    TASK_STATE_MANAGER.stop_task(task, preserve_sleep)
}

/// 便捷函数：恢复任务
pub fn resume_task(task: &TaskControlBlock) -> Result<(), &'static str> {
    TASK_STATE_MANAGER.resume_task(task)
}

/// 便捷函数：终止任务
pub fn terminate_task(task: &TaskControlBlock, exit_code: i32) -> Result<(), &'static str> {
    TASK_STATE_MANAGER.terminate_task(task, exit_code)
}

/// 获取任务状态统计
pub fn get_task_statistics() -> TaskStatusStats {
    TASK_STATE_MANAGER.get_statistics()
}