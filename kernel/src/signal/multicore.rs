//! 多核信号支持
//!
//! 简化的多核信号处理，提供跨核心信号投递

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::{Mutex, RwLock};

use super::core::{Signal, SignalError};
use crate::arch::hart::{MAX_CORES, hart_id};

//=============================================================================
// 核间信号消息
//=============================================================================

/// 核间信号消息类型
#[derive(Debug, Clone, Copy)]
pub enum SignalMessage {
    /// 信号投递通知
    SignalDelivery { target_pid: usize, signal: Signal },
    /// 检查待处理信号
    CheckSignals { target_pid: usize },
}

//=============================================================================
// 核心信号处理状态
//=============================================================================

/// 每个核心的信号处理状态
struct CoreSignalState {
    /// 核心ID
    core_id: usize,
    /// 是否活跃
    active: AtomicBool,
    /// 当前运行的任务PID
    current_task_pid: AtomicUsize,
    /// 待处理的消息队列
    message_queue: Mutex<alloc::vec::Vec<SignalMessage>>,
}

impl CoreSignalState {
    fn new(core_id: usize) -> Self {
        Self {
            core_id,
            active: AtomicBool::new(false),
            current_task_pid: AtomicUsize::new(0),
            message_queue: Mutex::new(alloc::vec::Vec::new()),
        }
    }

    /// 激活核心
    fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }

    /// 更新当前任务
    fn update_current_task(&self, pid: usize) {
        self.current_task_pid.store(pid, Ordering::Relaxed);
    }

    /// 添加消息到队列
    fn enqueue_message(&self, message: SignalMessage) {
        self.message_queue.lock().push(message);
    }

    /// 处理所有待处理消息
    fn process_messages(&self) -> usize {
        let mut queue = self.message_queue.lock();
        let count = queue.len();
        queue.clear(); // 简化处理：清除所有消息
        count
    }
}

//=============================================================================
// 多核信号管理器
//=============================================================================

/// 简化的多核信号管理器
pub struct MultiCoreSignalManager {
    /// 每个核心的状态
    core_states: [CoreSignalState; MAX_CORES],
    /// 进程到核心的映射
    process_core_map: RwLock<BTreeMap<usize, usize>>,
}

impl MultiCoreSignalManager {
    pub const fn new() -> Self {
        // 创建核心状态数组的const初始化
        const fn create_core_state(id: usize) -> CoreSignalState {
            CoreSignalState {
                core_id: id,
                active: AtomicBool::new(false),
                current_task_pid: AtomicUsize::new(0),
                message_queue: Mutex::new(alloc::vec::Vec::new()),
            }
        }

        Self {
            core_states: [
                create_core_state(0),
                create_core_state(1),
                create_core_state(2),
                create_core_state(3),
                create_core_state(4),
                create_core_state(5),
                create_core_state(6),
                create_core_state(7),
            ],
            process_core_map: RwLock::new(BTreeMap::new()),
        }
    }

    /// 初始化多核信号支持
    pub fn init(&self) {
        let current_core = hart_id();
        if current_core < MAX_CORES {
            self.core_states[current_core].activate();
        }
    }

    /// 更新任务在核心上的运行状态
    pub fn update_task_on_core(&self, core_id: usize, pid: usize) {
        if core_id < MAX_CORES {
            self.core_states[core_id].update_current_task(pid);
            if pid != 0 {
                self.process_core_map.write().insert(pid, core_id);
            }
        }
    }

    /// 清除任务在核心上的运行状态
    pub fn clear_task_on_core(&self, core_id: usize, pid: usize) {
        if core_id < MAX_CORES {
            let current_pid = self.core_states[core_id]
                .current_task_pid
                .load(Ordering::Relaxed);
            if current_pid == pid {
                self.core_states[core_id].update_current_task(0);
            }
            self.process_core_map.write().remove(&pid);
        }
    }

    /// 查找进程当前运行的核心
    pub fn find_process_core(&self, pid: usize) -> Option<usize> {
        self.process_core_map.read().get(&pid).copied()
    }

    /// 向进程发送信号（跨核心支持）
    pub fn send_signal_to_process(&self, pid: usize, signal: Signal) -> Result<(), SignalError> {
        if let Some(target_core) = self.find_process_core(pid) {
            let message = SignalMessage::SignalDelivery {
                target_pid: pid,
                signal,
            };
            self.send_message_to_core(target_core, message)
        } else {
            // 进程不在任何核心上运行，广播到所有活跃核心
            self.broadcast_signal_check(pid)
        }
    }

    /// 向指定核心发送消息
    fn send_message_to_core(
        &self,
        target_core: usize,
        message: SignalMessage,
    ) -> Result<(), SignalError> {
        if target_core >= MAX_CORES {
            return Err(SignalError::ProcessNotFound);
        }

        let current_core = hart_id();

        // 如果是同一个核心，直接处理
        if current_core == target_core {
            return Ok(());
        }

        // 检查目标核心是否活跃
        if !self.core_states[target_core].active.load(Ordering::Acquire) {
            return Err(SignalError::ProcessNotFound);
        }

        // 将消息加入目标核心队列
        self.core_states[target_core].enqueue_message(message);

        // 发送IPI唤醒目标核心
        self.send_ipi(target_core)
    }

    /// 广播信号检查请求到所有活跃核心
    fn broadcast_signal_check(&self, pid: usize) -> Result<(), SignalError> {
        let mut success_count = 0;

        for core_id in 0..MAX_CORES {
            if self.core_states[core_id].active.load(Ordering::Acquire) {
                let message = SignalMessage::CheckSignals { target_pid: pid };
                if self.send_message_to_core(core_id, message).is_ok() {
                    success_count += 1;
                }
            }
        }

        if success_count > 0 {
            Ok(())
        } else {
            Err(SignalError::ProcessNotFound)
        }
    }

    /// 发送IPI到指定核心
    fn send_ipi(&self, target_core: usize) -> Result<(), SignalError> {
        let hart_mask = 1usize << target_core;

        match crate::arch::sbi::sbi_send_ipi(hart_mask, 0) {
            Ok(()) => Ok(()),
            Err(_) => Err(SignalError::InternalError),
        }
    }

    /// 处理当前核心的待处理消息
    pub fn process_core_messages(&self) -> usize {
        let current_core = hart_id();
        if current_core < MAX_CORES {
            self.core_states[current_core].process_messages()
        } else {
            0
        }
    }
}

//=============================================================================
// 全局实例和接口函数
//=============================================================================

static MULTICORE_MANAGER: MultiCoreSignalManager = MultiCoreSignalManager::new();

/// 初始化多核信号支持
pub fn init() {
    MULTICORE_MANAGER.init();
}

/// 更新任务在核心上的运行状态
pub fn update_task_on_core(core_id: usize, pid: usize) {
    MULTICORE_MANAGER.update_task_on_core(core_id, pid);
}

/// 清除任务在核心上的运行状态
pub fn clear_task_on_core(core_id: usize, pid: usize) {
    MULTICORE_MANAGER.clear_task_on_core(core_id, pid);
}

/// 查找进程当前运行的核心
pub fn find_process_core(pid: usize) -> Option<usize> {
    MULTICORE_MANAGER.find_process_core(pid)
}

/// 向进程发送信号（跨核心支持）
pub fn send_signal_to_process(pid: usize, signal: Signal) -> Result<(), SignalError> {
    MULTICORE_MANAGER.send_signal_to_process(pid, signal)
}

/// 处理当前核心的待处理消息
pub fn process_core_messages() -> usize {
    MULTICORE_MANAGER.process_core_messages()
}
