use alloc::{collections::BTreeMap, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use spin::{Mutex, RwLock};

use crate::arch::hart::{hart_id, MAX_CORES};
use super::signal::{Signal, SignalError};
use super::signal_manager::{SIGNAL_EVENT_BUS, SignalEvent};

/// 核心间信号通信类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InterCoreSignalMessage {
    /// 信号投递通知
    SignalDelivery { target_pid: usize, signal: Signal },
    /// 要求检查待处理信号
    CheckSignals { target_pid: usize },
    /// 强制任务调度
    ForceSchedule,
    /// 核心状态同步
    SyncCoreState,
}

/// 每个核心的信号处理状态
#[derive(Debug)]
struct CoreSignalHandler {
    /// 核心ID
    core_id: usize,
    /// 是否活跃
    active: AtomicBool,
    /// 当前运行的任务PID
    current_task_pid: AtomicUsize,
    /// 待处理的核心间消息队列
    message_queue: Mutex<Vec<InterCoreSignalMessage>>,
    /// 信号处理统计
    signal_stats: SignalProcessorStats,
}

impl CoreSignalHandler {
    fn new(core_id: usize) -> Self {
        Self {
            core_id,
            active: AtomicBool::new(false),
            current_task_pid: AtomicUsize::new(0),
            message_queue: Mutex::new(Vec::new()),
            signal_stats: SignalProcessorStats::new(),
        }
    }

    /// 激活核心信号处理
    fn activate(&self) {
        self.active.store(true, Ordering::Release);
        debug!("Core {} signal handler activated", self.core_id);
    }

    /// 停用核心信号处理
    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
        debug!("Core {} signal handler deactivated", self.core_id);
    }

    /// 更新当前任务
    fn update_current_task(&self, pid: usize) {
        self.current_task_pid.store(pid, Ordering::Relaxed);
    }

    /// 添加消息到队列
    fn enqueue_message(&self, message: InterCoreSignalMessage) {
        self.message_queue.lock().push(message);
        self.signal_stats.increment_messages_received();
    }

    /// 处理所有待处理消息
    fn process_messages(&self) -> usize {
        let mut queue = self.message_queue.lock();
        let message_count = queue.len();
        
        for message in queue.drain(..) {
            self.handle_message(message);
        }
        
        if message_count > 0 {
            self.signal_stats.increment_messages_processed(message_count);
        }
        
        message_count
    }

    /// 处理单个消息
    fn handle_message(&self, message: InterCoreSignalMessage) {
        match message {
            InterCoreSignalMessage::SignalDelivery { target_pid, signal } => {
                debug!("Core {} received signal delivery request: PID {} signal {}", 
                       self.core_id, target_pid, signal as u32);
                
                // 通知信号管理器处理
                SIGNAL_EVENT_BUS.publish(SignalEvent::SignalDelivered { 
                    pid: target_pid, 
                    signal 
                });
            }
            InterCoreSignalMessage::CheckSignals { target_pid } => {
                debug!("Core {} received signal check request for PID {}", 
                       self.core_id, target_pid);
                
                // 标记需要检查信号
                super::signal_manager::SIGNAL_MANAGER.mark_core_needs_signal_check(self.core_id);
            }
            InterCoreSignalMessage::ForceSchedule => {
                debug!("Core {} received force schedule request", self.core_id);
                // 这里可以触发调度器重新调度
            }
            InterCoreSignalMessage::SyncCoreState => {
                debug!("Core {} received state sync request", self.core_id);
                // 同步核心状态
            }
        }
    }
}

/// 信号处理统计
#[derive(Debug)]
struct SignalProcessorStats {
    /// 收到的消息数量
    messages_received: AtomicU32,
    /// 处理的消息数量
    messages_processed: AtomicU32,
    /// 发送的IPI数量
    ipis_sent: AtomicU32,
    /// 失败的IPI数量
    ipis_failed: AtomicU32,
}

impl SignalProcessorStats {
    fn new() -> Self {
        Self {
            messages_received: AtomicU32::new(0),
            messages_processed: AtomicU32::new(0),
            ipis_sent: AtomicU32::new(0),
            ipis_failed: AtomicU32::new(0),
        }
    }

    fn increment_messages_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }

    fn increment_messages_processed(&self, count: usize) {
        self.messages_processed.fetch_add(count as u32, Ordering::Relaxed);
    }

    fn increment_ipis_sent(&self) {
        self.ipis_sent.fetch_add(1, Ordering::Relaxed);
    }

    fn increment_ipis_failed(&self) {
        self.ipis_failed.fetch_add(1, Ordering::Relaxed);
    }
}

/// 多核信号管理器
pub struct MultiCoreSignalManager {
    /// 每个核心的信号处理器
    core_handlers: [CoreSignalHandler; MAX_CORES],
    /// 进程到核心的映射
    process_core_map: RwLock<BTreeMap<usize, usize>>,
    /// IPI发送历史记录（用于调试）
    ipi_history: Mutex<Vec<IpiRecord>>,
}

#[derive(Debug, Clone)]
struct IpiRecord {
    timestamp: u64,
    source_core: usize,
    target_core: usize,
    message: InterCoreSignalMessage,
    success: bool,
}

impl MultiCoreSignalManager {
    pub const fn new() -> Self {
        const CORE_HANDLER: CoreSignalHandler = CoreSignalHandler {
            core_id: 0,
            active: AtomicBool::new(false),
            current_task_pid: AtomicUsize::new(0),
            message_queue: Mutex::new(Vec::new()),
            signal_stats: SignalProcessorStats {
                messages_received: AtomicU32::new(0),
                messages_processed: AtomicU32::new(0),
                ipis_sent: AtomicU32::new(0),
                ipis_failed: AtomicU32::new(0),
            },
        };
        
        Self {
            core_handlers: [CORE_HANDLER; MAX_CORES],
            process_core_map: RwLock::new(BTreeMap::new()),
            ipi_history: Mutex::new(Vec::new()),
        }
    }

    /// 初始化所有核心处理器（在运行时调用）
    fn init_core_handlers(&self) {
        for i in 0..MAX_CORES {
            // 运行时初始化每个核心的ID
            // 注意：这里我们无法在const中设置不同的core_id，所以在运行时处理
        }
    }

    /// 初始化多核信号支持
    pub fn init(&self) {
        let current_core = hart_id();
        self.activate_core(current_core);
        
        info!("MultiCoreSignalManager initialized on core {}", current_core);
    }

    /// 激活指定核心的信号处理
    pub fn activate_core(&self, core_id: usize) {
        if core_id < MAX_CORES {
            self.core_handlers[core_id].activate();
        }
    }

    /// 停用指定核心的信号处理
    pub fn deactivate_core(&self, core_id: usize) {
        if core_id < MAX_CORES {
            self.core_handlers[core_id].deactivate();
        }
    }

    /// 更新任务在核心上的运行状态
    pub fn update_task_on_core(&self, core_id: usize, pid: usize) {
        if core_id < MAX_CORES {
            self.core_handlers[core_id].update_current_task(pid);
            if pid != 0 {
                self.process_core_map.write().insert(pid, core_id);
            }
        }
    }

    /// 清除任务在核心上的运行状态
    pub fn clear_task_on_core(&self, core_id: usize, pid: usize) {
        if core_id < MAX_CORES {
            let current_pid = self.core_handlers[core_id].current_task_pid.load(Ordering::Relaxed);
            if current_pid == pid {
                self.core_handlers[core_id].update_current_task(0);
            }
            self.process_core_map.write().remove(&pid);
        }
    }

    /// 查找进程当前运行的核心
    pub fn find_process_core(&self, pid: usize) -> Option<usize> {
        self.process_core_map.read().get(&pid).copied()
    }

    /// 向指定核心发送信号消息
    pub fn send_signal_to_core(
        &self, 
        target_core: usize, 
        message: InterCoreSignalMessage
    ) -> Result<(), SignalError> {
        if target_core >= MAX_CORES {
            return Err(SignalError::InvalidProcess);
        }

        let source_core = hart_id();
        
        // 如果是同一个核心，直接处理
        if source_core == target_core {
            self.core_handlers[target_core].handle_message(message);
            return Ok(());
        }

        // 检查目标核心是否活跃
        if !self.core_handlers[target_core].active.load(Ordering::Acquire) {
            return Err(SignalError::ProcessNotFound);
        }

        // 将消息加入目标核心队列
        self.core_handlers[target_core].enqueue_message(message);

        // 发送IPI唤醒目标核心
        let result = self.send_ipi(target_core);
        
        // 记录IPI历史
        let record = IpiRecord {
            timestamp: crate::timer::get_time_us(),
            source_core,
            target_core,
            message,
            success: result.is_ok(),
        };
        
        self.ipi_history.lock().push(record);
        
        // 更新统计
        if result.is_ok() {
            self.core_handlers[source_core].signal_stats.increment_ipis_sent();
        } else {
            self.core_handlers[source_core].signal_stats.increment_ipis_failed();
        }

        result
    }

    /// 发送IPI到指定核心
    fn send_ipi(&self, target_core: usize) -> Result<(), SignalError> {
        let hart_mask = 1usize << target_core;
        
        match crate::arch::sbi::sbi_send_ipi(hart_mask, 0) {
            Ok(()) => {
                debug!("IPI sent successfully to core {}", target_core);
                Ok(())
            }
            Err(error) => {
                warn!("Failed to send IPI to core {}: error {}", target_core, error);
                Err(SignalError::InvalidProcess)
            }
        }
    }

    /// 处理当前核心的待处理消息
    pub fn process_core_messages(&self) -> usize {
        let current_core = hart_id();
        if current_core < MAX_CORES {
            self.core_handlers[current_core].process_messages()
        } else {
            0
        }
    }

    /// 向进程发送信号（跨核心支持）
    pub fn send_signal_to_process(&self, pid: usize, signal: Signal) -> Result<(), SignalError> {
        // 查找进程所在的核心
        if let Some(target_core) = self.find_process_core(pid) {
            let message = InterCoreSignalMessage::SignalDelivery { 
                target_pid: pid, 
                signal 
            };
            
            self.send_signal_to_core(target_core, message)
        } else {
            // 进程不在任何核心上运行，可能在等待队列中
            debug!("Process PID {} not found on any core, may be in wait queue", pid);
            
            // 广播到所有活跃核心
            self.broadcast_signal_check(pid)
        }
    }

    /// 广播信号检查请求到所有活跃核心
    fn broadcast_signal_check(&self, pid: usize) -> Result<(), SignalError> {
        let mut success_count = 0;
        let mut error_count = 0;
        
        for core_id in 0..MAX_CORES {
            if self.core_handlers[core_id].active.load(Ordering::Acquire) {
                let message = InterCoreSignalMessage::CheckSignals { target_pid: pid };
                match self.send_signal_to_core(core_id, message) {
                    Ok(()) => success_count += 1,
                    Err(_) => error_count += 1,
                }
            }
        }
        
        if success_count > 0 {
            Ok(())
        } else {
            Err(SignalError::ProcessNotFound)
        }
    }

    /// 获取核心状态统计
    pub fn get_core_stats(&self, core_id: usize) -> Option<CoreSignalStats> {
        if core_id < MAX_CORES {
            let handler = &self.core_handlers[core_id];
            Some(CoreSignalStats {
                core_id,
                active: handler.active.load(Ordering::Acquire),
                current_task_pid: handler.current_task_pid.load(Ordering::Relaxed),
                messages_received: handler.signal_stats.messages_received.load(Ordering::Relaxed),
                messages_processed: handler.signal_stats.messages_processed.load(Ordering::Relaxed),
                ipis_sent: handler.signal_stats.ipis_sent.load(Ordering::Relaxed),
                ipis_failed: handler.signal_stats.ipis_failed.load(Ordering::Relaxed),
                queue_length: handler.message_queue.lock().len(),
            })
        } else {
            None
        }
    }

    /// 获取所有核心的统计信息
    pub fn get_all_core_stats(&self) -> Vec<CoreSignalStats> {
        (0..MAX_CORES)
            .filter_map(|core_id| self.get_core_stats(core_id))
            .collect()
    }

    /// 清理IPI历史记录（防止内存泄漏）
    pub fn cleanup_ipi_history(&self, max_records: usize) {
        let mut history = self.ipi_history.lock();
        if history.len() > max_records {
            let excess = history.len() - max_records;
            history.drain(0..excess);
        }
    }
}

/// 核心信号处理统计信息
#[derive(Debug, Clone)]
pub struct CoreSignalStats {
    pub core_id: usize,
    pub active: bool,
    pub current_task_pid: usize,
    pub messages_received: u32,
    pub messages_processed: u32,
    pub ipis_sent: u32,
    pub ipis_failed: u32,
    pub queue_length: usize,
}

/// 全局多核信号管理器
pub static MULTICORE_SIGNAL_MANAGER: MultiCoreSignalManager = MultiCoreSignalManager::new();