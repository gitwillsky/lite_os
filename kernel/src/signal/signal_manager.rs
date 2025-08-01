use alloc::{collections::BTreeMap, vec::Vec, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    ops::Deref,
};
use spin::{Mutex, RwLock};

use crate::{
    arch::hart::hart_id,
    sync::UPSafeCell,
    task::{TaskControlBlock, TaskStatus},
    trap::TrapContext,
};

use super::signal::{Signal, SignalAction, SignalDisposition, SignalError, SignalSet, SignalState};
use super::signal_state::{AtomicSignalState, DEFAULT_BATCH_PROCESSOR};
use super::signal_delivery::{SafeSignalDelivery, UserStackValidator};
use super::multicore_signal::{MULTICORE_SIGNAL_MANAGER, InterCoreSignalMessage};

/// 信号相关事件类型
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SignalEvent {
    /// 任务状态改变事件
    TaskStatusChanged {
        pid: usize,
        old_status: TaskStatus,
        new_status: TaskStatus,
    },
    /// 信号投递事件
    SignalDelivered {
        pid: usize,
        signal: Signal,
    },
    /// 任务退出事件
    TaskExited {
        pid: usize,
        exit_code: i32,
    },
    /// 任务创建事件
    TaskCreated {
        pid: usize,
        parent_pid: Option<usize>,
    },
}

/// 事件监听器回调函数类型
pub type EventCallback = fn(event: SignalEvent);

/// 信号系统事件总线
pub struct SignalEventBus {
    /// 注册的事件监听器
    listeners: RwLock<Vec<EventCallback>>,
}

impl SignalEventBus {
    pub const fn new() -> Self {
        Self {
            listeners: RwLock::new(Vec::new()),
        }
    }

    /// 注册事件监听器
    pub fn register_listener(&self, callback: EventCallback) {
        self.listeners.write().push(callback);
    }

    /// 发布事件到所有监听器
    pub fn publish(&self, event: SignalEvent) {
        let listeners = self.listeners.read();
        for callback in listeners.iter() {
            callback(event);
        }
    }
}

/// 全局信号事件总线
pub static SIGNAL_EVENT_BUS: SignalEventBus = SignalEventBus::new();

/// 任务状态改变通知函数
/// 替代直接调用 task 模块函数，使用事件通知
pub fn notify_task_status_change(pid: usize, old_status: TaskStatus, new_status: TaskStatus) {
    SIGNAL_EVENT_BUS.publish(SignalEvent::TaskStatusChanged {
        pid,
        old_status,
        new_status,
    });
}

/// 信号投递通知函数
pub fn notify_signal_delivered(pid: usize, signal: Signal) {
    SIGNAL_EVENT_BUS.publish(SignalEvent::SignalDelivered { pid, signal });
}

/// 任务退出通知函数
pub fn notify_task_exited(pid: usize, exit_code: i32) {
    SIGNAL_EVENT_BUS.publish(SignalEvent::TaskExited { pid, exit_code });
}

/// 任务创建通知函数
pub fn notify_task_created(pid: usize, parent_pid: Option<usize>) {
    SIGNAL_EVENT_BUS.publish(SignalEvent::TaskCreated { pid, parent_pid });
}

/// 多核信号管理信息
#[derive(Debug)]
struct CoreSignalInfo {
    /// 当前运行的任务 PID
    current_task_pid: AtomicUsize,
    /// 是否需要检查信号
    needs_signal_check: AtomicBool,
}

impl CoreSignalInfo {
    const fn new() -> Self {
        Self {
            current_task_pid: AtomicUsize::new(0),
            needs_signal_check: AtomicBool::new(false),
        }
    }
}

/// 统一的信号管理器
/// 负责所有信号相关操作，解除模块间直接依赖
pub struct SignalManager {
    /// 每个核心的信号管理信息
    core_info: [CoreSignalInfo; crate::arch::hart::MAX_CORES],
    /// 进程到核心的映射缓存
    process_core_map: RwLock<BTreeMap<usize, usize>>,
}

impl SignalManager {
    pub const fn new() -> Self {
        const CORE_INFO: CoreSignalInfo = CoreSignalInfo::new();
        Self {
            core_info: [CORE_INFO; crate::arch::hart::MAX_CORES],
            process_core_map: RwLock::new(BTreeMap::new()),
        }
    }

    /// 更新进程在核心上的运行状态
    pub fn update_task_on_core(&self, core_id: usize, pid: usize) {
        // 同时更新本地和多核管理器
        if core_id < self.core_info.len() {
            self.core_info[core_id].current_task_pid.store(pid, Ordering::Relaxed);
            if pid != 0 {
                self.process_core_map.write().insert(pid, core_id);
            }
        }
        MULTICORE_SIGNAL_MANAGER.update_task_on_core(core_id, pid);
    }

    /// 清除进程在核心上的运行状态
    pub fn clear_task_on_core(&self, core_id: usize, pid: usize) {
        // 同时更新本地和多核管理器
        if core_id < self.core_info.len() {
            let current_pid = self.core_info[core_id].current_task_pid.load(Ordering::Relaxed);
            if current_pid == pid {
                self.core_info[core_id].current_task_pid.store(0, Ordering::Relaxed);
            }
            self.process_core_map.write().remove(&pid);
        }
        MULTICORE_SIGNAL_MANAGER.clear_task_on_core(core_id, pid);
    }

    /// 查找进程当前运行的核心
    pub fn find_process_core(&self, pid: usize) -> Option<usize> {
        // 优先使用多核管理器的查找
        MULTICORE_SIGNAL_MANAGER.find_process_core(pid)
            .or_else(|| self.process_core_map.read().get(&pid).copied())
    }

    /// 标记核心需要检查信号
    pub fn mark_core_needs_signal_check(&self, core_id: usize) {
        if core_id < self.core_info.len() {
            self.core_info[core_id].needs_signal_check.store(true, Ordering::Relaxed);
        }
    }

    /// 检查并清除核心的信号检查标记
    pub fn check_and_clear_signal_flag(&self, core_id: usize) -> bool {
        if core_id < self.core_info.len() {
            self.core_info[core_id].needs_signal_check.swap(false, Ordering::Relaxed)
        } else {
            false
        }
    }

    /// 发送信号到指定进程（使用改进的多核支持）
    pub fn send_signal(&self, target_pid: usize, signal: Signal) -> Result<(), SignalError> {
        debug!("SignalManager: Sending signal {} to PID {}", signal as u32, target_pid);

        // 查找目标任务
        let task = crate::task::find_task_by_pid(target_pid)
            .ok_or(SignalError::ProcessNotFound)?;

        // 处理不可捕获的信号
        if signal.is_uncatchable() {
            return self.handle_uncatchable_signal(&task, signal);
        }

        // 添加信号到待处理队列
        task.signal_state.lock().add_pending_signal(signal);

        // 通知信号投递事件
        notify_signal_delivered(target_pid, signal);

        // 使用多核信号管理器发送信号
        let current_status = *task.task_status.lock();
        match current_status {
            TaskStatus::Running => {
                // 使用多核管理器发送信号
                MULTICORE_SIGNAL_MANAGER.send_signal_to_process(target_pid, signal)?;
            }
            TaskStatus::Sleeping => {
                // 根据信号类型决定是否唤醒
                if !signal.is_stop_signal() {
                    self.wakeup_task(&task);
                    // 也发送IPI确保及时处理
                    let _ = MULTICORE_SIGNAL_MANAGER.send_signal_to_process(target_pid, signal);
                }
            }
            TaskStatus::Stopped => {
                // 处理停止状态下的信号
                self.handle_signal_for_stopped_task(&task, signal);
                // 发送IPI通知其他核心
                let _ = MULTICORE_SIGNAL_MANAGER.send_signal_to_process(target_pid, signal);
            }
            _ => {
                // 对于其他状态，也发送IPI确保消息传递
                let _ = MULTICORE_SIGNAL_MANAGER.send_signal_to_process(target_pid, signal);
            }
        }

        Ok(())
    }

    /// 处理不可捕获的信号
    fn handle_uncatchable_signal(&self, task: &Arc<TaskControlBlock>, signal: Signal) -> Result<(), SignalError> {
        match signal {
            Signal::SIGKILL => {
                info!("SignalManager: Killing process PID {} with SIGKILL", task.pid());
                task.signal_state.lock().add_pending_signal(signal);
                if *task.task_status.lock() == TaskStatus::Sleeping {
                    self.wakeup_task(task);
                }
            }
            Signal::SIGSTOP => {
                info!("SignalManager: Stopping process PID {} with SIGSTOP", task.pid());
                self.stop_task(task);
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    /// 停止任务
    fn stop_task(&self, task: &TaskControlBlock) {
        // 保存停止前的状态
        let old_status = *task.task_status.lock();
        *task.prev_status_before_stop.lock() = Some(old_status);

        // 使用统一的状态管理
        if let Some(task_arc) = crate::task::find_task_by_pid(task.pid()) {
            crate::task::set_task_status(&task_arc, TaskStatus::Stopped);
        }
    }

    /// 唤醒任务
    fn wakeup_task(&self, task: &TaskControlBlock) {
        // Find the task in the task manager and call wakeup on the Arc
        if let Some(task_arc) = crate::task::find_task_by_pid(task.pid()) {
            task_arc.wakeup();
        }
    }

    /// 处理停止状态任务的信号
    fn handle_signal_for_stopped_task(&self, task: &TaskControlBlock, signal: Signal) {
        if signal.is_continue_signal() {
            debug!("SignalManager: SIGCONT will resume stopped process PID {}", task.pid());
            // 对于SIGCONT信号，也需要恢复进程
            if let Some(task_arc) = crate::task::find_task_by_pid(task.pid()) {
                crate::task::set_task_status(&task_arc, TaskStatus::Ready);
            }
        } else if signal.is_uncatchable() || signal.default_action() == SignalAction::Terminate {
            debug!("SignalManager: Fatal signal {} for stopped PID {} - resuming for termination",
                   signal as u32, task.pid());
            // 恢复进程以处理致命信号，使用统一的状态管理机制
            if let Some(task_arc) = crate::task::find_task_by_pid(task.pid()) {
                crate::task::set_task_status(&task_arc, TaskStatus::Ready);
            }
        }
    }

    /// 发送 IPI 到指定核心
    fn send_ipi_to_core(&self, core_id: usize, target_pid: usize) -> Result<(), SignalError> {
        self.mark_core_needs_signal_check(core_id);

        let hart_mask = 1usize << core_id;
        match crate::arch::sbi::sbi_send_ipi(hart_mask, 0) {
            Ok(()) => {
                debug!("SignalManager: Sent IPI to core {} for PID {}", core_id, target_pid);
                Ok(())
            }
            Err(error) => {
                warn!("SignalManager: Failed to send IPI to core {} for PID {}: error {}",
                      core_id, target_pid, error);
                Err(SignalError::InvalidProcess)
            }
        }
    }

    /// 处理当前任务的信号（安全版本，无 trap context）
    pub fn handle_signals_safe(&self, task: &TaskControlBlock) -> (bool, Option<i32>) {
        loop {
            let signal = task.signal_state.lock().next_deliverable_signal();
            let Some(signal) = signal else {
                return (true, None);
            };

            debug!("SignalManager: Processing signal {} for PID {} (safe context)",
                   signal as u32, task.pid());

            let handler = task.signal_state.lock().get_handler(signal);

            match handler.action {
                SignalAction::Ignore => {
                    debug!("SignalManager: Signal {} ignored", signal as u32);
                    continue;
                }
                SignalAction::Terminate => {
                    debug!("SignalManager: Signal {} terminates process", signal as u32);
                    notify_task_exited(task.pid(), signal.default_exit_code());
                    return (false, Some(signal.default_exit_code()));
                }
                SignalAction::Stop => {
                    debug!("SignalManager: Signal {} stops process", signal as u32);
                    self.stop_task(task);
                    return (false, None);
                }
                SignalAction::Continue => {
                    debug!("SignalManager: Signal {} continues process", signal as u32);
                    self.continue_task(task);
                    continue;
                }
                SignalAction::Handler(_) => {
                    // 需要 trap context，标记延后处理
                    debug!("SignalManager: Signal {} needs trap context handling", signal as u32);
                    task.signal_state.lock().add_pending_signal(signal);
                    task.signal_state.lock().set_needs_trap_context_handling(true);
                    return (true, None);
                }
            }
        }
    }

    /// 处理当前任务的信号（带 trap context）
    pub fn handle_signals_with_context(&self, task: &TaskControlBlock, trap_cx: &mut TrapContext) -> (bool, Option<i32>) {
        // 检查是否有需要 trap context 处理的信号
        if task.signal_state.lock().needs_trap_context_handling() {
            task.signal_state.lock().set_needs_trap_context_handling(false);
        }

        // 处理信号
        let signal_and_handler = {
            let mut signal_state = task.signal_state.lock();
            if let Some(signal) = signal_state.next_deliverable_signal() {
                let handler = signal_state.get_handler(signal);
                Some((signal, handler))
            } else {
                None
            }
        };

        if let Some((signal, handler)) = signal_and_handler {
            self.deliver_signal_with_handler(task, signal, handler, trap_cx)
        } else {
            (true, None)
        }
    }

    /// 继续被停止的任务
    fn continue_task(&self, task: &TaskControlBlock) {
        // 获取停止前的状态
        let restored_status = task.prev_status_before_stop.lock()
            .take()
            .unwrap_or(TaskStatus::Ready);

        // 使用统一的状态管理恢复任务
        if let Some(task_arc) = crate::task::find_task_by_pid(task.pid()) {
            crate::task::set_task_status(&task_arc, restored_status);
        }
    }

    /// 投递信号处理器
    fn deliver_signal_with_handler(
        &self,
        task: &TaskControlBlock,
        signal: Signal,
        handler: SignalDisposition,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        match handler.action {
            SignalAction::Ignore => {
                debug!("SignalManager: Signal {} ignored", signal as u32);
                (true, None)
            }
            SignalAction::Terminate => {
                info!("SignalManager: Signal {} terminates process PID {}",
                      signal as u32, task.pid());
                notify_task_exited(task.pid(), signal as i32);
                (false, Some(signal as i32))
            }
            SignalAction::Stop => {
                info!("SignalManager: Signal {} stops process PID {}",
                      signal as u32, task.pid());
                self.stop_task(task);
                (false, None)
            }
            SignalAction::Continue => {
                info!("SignalManager: Signal {} continues process PID {}",
                      signal as u32, task.pid());
                self.continue_task(task);
                (true, None)
            }
            SignalAction::Handler(handler_addr) => {
                debug!("SignalManager: Signal {} executing handler at {:#x} for PID {}",
                       signal as u32, handler_addr, task.pid());

                self.setup_signal_handler(task, signal, handler_addr, &handler, trap_cx);

                // 处理 SA_RESETHAND 标志
                if (handler.flags & super::signal::SA_RESETHAND) != 0 {
                    let default_disposition = SignalDisposition {
                        action: signal.default_action(),
                        mask: SignalSet::new(),
                        flags: 0,
                    };
                    task.signal_state.lock().set_handler(signal, default_disposition);
                }
                (true, None)
            }
        }
    }

    /// 设置信号处理器执行环境（使用安全投递机制）
    fn setup_signal_handler(
        &self,
        task: &TaskControlBlock,
        signal: Signal,
        handler_addr: usize,
        handler_info: &SignalDisposition,
        trap_cx: &mut TrapContext,
    ) {
        // 使用新的安全信号投递机制
        if let Err(e) = SafeSignalDelivery::setup_signal_handler(
            task, signal, handler_addr, handler_info, trap_cx
        ) {
            error!("SignalManager: Failed to setup signal handler for PID {}: {}",
                   task.pid(), e);

            // 如果设置失败，发送 SIGSEGV
            task.signal_state.lock().add_pending_signal(Signal::SIGSEGV);
        }
    }

    /// 从信号处理函数返回（使用安全机制）
    pub fn sigreturn(&self, task: &TaskControlBlock, trap_cx: &mut TrapContext) -> bool {
        match SafeSignalDelivery::safe_sigreturn(task, trap_cx) {
            Ok(()) => {
                debug!("SignalManager: Safe sigreturn completed for PID {}", task.pid());
                true
            }
            Err(e) => {
                error!("SignalManager: Safe sigreturn failed for PID {}: {}", task.pid(), e);
                // 发送 SIGSEGV 信号表示错误
                task.signal_state.lock().add_pending_signal(Signal::SIGSEGV);
                false
            }
        }
    }
}

/// 全局信号管理器实例
pub static SIGNAL_MANAGER: SignalManager = SignalManager::new();

/// 初始化信号系统
pub fn init() {
    // 初始化多核信号管理器
    MULTICORE_SIGNAL_MANAGER.init();

    // 注册事件监听器处理任务状态变化
    SIGNAL_EVENT_BUS.register_listener(|event| {
        match event {
            SignalEvent::TaskStatusChanged { pid, old_status, new_status } => {
                debug!("Task status changed: PID {} from {:?} to {:?}", pid, old_status, new_status);
                // 处理多核环境下的状态同步
                if new_status == TaskStatus::Running {
                    let current_core = crate::arch::hart::hart_id();
                    SIGNAL_MANAGER.update_task_on_core(current_core, pid);
                }
            }
            SignalEvent::SignalDelivered { pid, signal } => {
                debug!("Signal {} delivered to PID {}", signal as u32, pid);
                // 处理核心间消息
                let _ = MULTICORE_SIGNAL_MANAGER.process_core_messages();
            }
            SignalEvent::TaskExited { pid, exit_code } => {
                debug!("Task exited: PID {} with code {}", pid, exit_code);
                // 清理多核状态
                if let Some(core_id) = SIGNAL_MANAGER.find_process_core(pid) {
                    SIGNAL_MANAGER.clear_task_on_core(core_id, pid);
                }
            }
            SignalEvent::TaskCreated { pid, parent_pid } => {
                debug!("Task created: PID {} (parent: {:?})", pid, parent_pid);
            }
        }
    });

    info!("Signal system initialization complete with multi-core support");
}

/// 处理当前核心的待处理信号消息
/// 应该在调度循环中定期调用
pub fn process_multicore_signals() -> usize {
    MULTICORE_SIGNAL_MANAGER.process_core_messages()
}

/// 获取多核信号统计信息
pub fn get_multicore_signal_stats() -> Vec<super::multicore_signal::CoreSignalStats> {
    MULTICORE_SIGNAL_MANAGER.get_all_core_stats()
}