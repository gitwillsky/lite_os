use alloc::{sync::Arc, collections::BTreeMap};
use core::sync::atomic::{AtomicBool, Ordering};
use crate::{
    thread::{ThreadId, ThreadControlBlock},
    task::{
        signal::{
            Signal, SignalSet, SignalAction, SignalDisposition,
            SignalDelivery, SignalFrame, SA_NODEFER,
        },
        TaskControlBlock,
    },
    trap::TrapContext,
};

/// 线程级别的信号状态
#[derive(Debug)]
pub struct ThreadSignalState {
    /// 线程私有的待处理信号
    pub pending: spin::Mutex<SignalSet>,
    /// 线程私有的信号屏蔽
    pub blocked: spin::Mutex<SignalSet>,
    /// 线程私有的信号处理器（覆盖进程级别的处理器）
    pub thread_handlers: spin::Mutex<BTreeMap<Signal, SignalDisposition>>,
    /// 是否正在处理信号
    pub in_signal_handler: spin::Mutex<bool>,
    /// 保存的信号掩码
    pub saved_mask: spin::Mutex<Option<SignalSet>>,
    /// 线程是否被信号暂停
    pub signal_suspended: AtomicBool,
    /// 当前正在处理的信号
    pub current_signal: spin::Mutex<Option<Signal>>,
}

impl ThreadSignalState {
    pub fn new() -> Self {
        Self {
            pending: spin::Mutex::new(SignalSet::new()),
            blocked: spin::Mutex::new(SignalSet::new()),
            thread_handlers: spin::Mutex::new(BTreeMap::new()),
            in_signal_handler: spin::Mutex::new(false),
            saved_mask: spin::Mutex::new(None),
            signal_suspended: AtomicBool::new(false),
            current_signal: spin::Mutex::new(None),
        }
    }

    /// 向线程发送信号
    pub fn send_signal(&self, signal: Signal) {
        let mut pending = self.pending.lock();
        pending.add(signal);
        
        // 如果是SIGCONT信号，唤醒被暂停的线程
        if signal.is_continue_signal() {
            self.signal_suspended.store(false, Ordering::Release);
        }
        
        debug!("Signal {} sent to thread", signal as u32);
    }

    /// 检查是否有可投递的信号
    pub fn has_deliverable_signals(&self) -> bool {
        let pending = self.pending.lock();
        let blocked = self.blocked.lock();
        
        !pending.difference(&blocked).is_empty()
    }

    /// 获取下一个可投递的信号
    pub fn next_deliverable_signal(&self) -> Option<Signal> {
        let mut pending = self.pending.lock();
        let blocked = self.blocked.lock();
        
        let deliverable = pending.difference(&blocked);
        if let Some(signal) = deliverable.first_signal() {
            // 检查是否是不可捕获的信号
            if signal.is_uncatchable() {
                pending.remove(signal);
                return Some(signal);
            }
            
            // 检查是否被阻塞
            if !blocked.contains(signal) {
                pending.remove(signal);
                return Some(signal);
            }
        }
        
        None
    }

    /// 设置线程级别的信号处理器
    pub fn set_thread_handler(&self, signal: Signal, disposition: SignalDisposition) {
        let mut handlers = self.thread_handlers.lock();
        handlers.insert(signal, disposition);
        debug!("Thread-level handler set for signal {}", signal as u32);
    }

    /// 获取线程级别的信号处理器
    pub fn get_thread_handler(&self, signal: Signal) -> Option<SignalDisposition> {
        let handlers = self.thread_handlers.lock();
        handlers.get(&signal).cloned()
    }

    /// 阻塞信号
    pub fn block_signals(&self, signals: SignalSet) {
        let mut blocked = self.blocked.lock();
        *blocked = blocked.union(&signals);
    }

    /// 解除阻塞信号
    pub fn unblock_signals(&self, signals: SignalSet) {
        let mut blocked = self.blocked.lock();
        *blocked = blocked.difference(&signals);
    }

    /// 设置信号掩码
    pub fn set_signal_mask(&self, mask: SignalSet) {
        let mut blocked = self.blocked.lock();
        *blocked = mask;
    }

    /// 获取信号掩码
    pub fn get_signal_mask(&self) -> SignalSet {
        *self.blocked.lock()
    }

    /// 进入信号处理器
    pub fn enter_signal_handler(&self, signal: Signal, additional_mask: SignalSet) {
        let mut in_handler = self.in_signal_handler.lock();
        let mut saved_mask = self.saved_mask.lock();
        let mut blocked = self.blocked.lock();
        let mut current_signal = self.current_signal.lock();

        if !*in_handler {
            *saved_mask = Some(*blocked);
            *in_handler = true;
        }

        *blocked = blocked.union(&additional_mask);
        *current_signal = Some(signal);
    }

    /// 退出信号处理器
    pub fn exit_signal_handler(&self) {
        let mut in_handler = self.in_signal_handler.lock();
        let mut saved_mask = self.saved_mask.lock();
        let mut blocked = self.blocked.lock();
        let mut current_signal = self.current_signal.lock();

        if let Some(mask) = saved_mask.take() {
            *blocked = mask;
            *in_handler = false;
        }
        *current_signal = None;
    }

    /// 暂停线程（由信号引起）
    pub fn suspend_by_signal(&self) {
        self.signal_suspended.store(true, Ordering::Release);
    }

    /// 检查线程是否被信号暂停
    pub fn is_suspended_by_signal(&self) -> bool {
        self.signal_suspended.load(Ordering::Acquire)
    }

    /// 恢复被信号暂停的线程
    pub fn resume_from_signal(&self) {
        self.signal_suspended.store(false, Ordering::Release);
    }

    /// 继承父线程的信号状态
    pub fn inherit_from_parent(&self, parent_state: &ThreadSignalState) {
        // 继承信号掩码
        let parent_blocked = *parent_state.blocked.lock();
        *self.blocked.lock() = parent_blocked;
        
        // 继承线程级别的信号处理器
        let parent_handlers = parent_state.thread_handlers.lock().clone();
        *self.thread_handlers.lock() = parent_handlers;
        
        debug!("Thread signal state inherited from parent");
    }
}

impl Default for ThreadSignalState {
    fn default() -> Self {
        Self::new()
    }
}

/// 线程信号投递引擎
pub struct ThreadSignalDelivery;

impl ThreadSignalDelivery {
    /// 为线程处理信号
    pub fn handle_thread_signals(
        thread: &Arc<ThreadControlBlock>,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        let thread_inner = thread.inner_exclusive_access();
        
        // 检查线程级别的信号
        if let Some(signal_state) = thread_inner.signal_state.as_ref() {
            if let Some(signal) = signal_state.next_deliverable_signal() {
                drop(thread_inner);
                return Self::deliver_thread_signal(thread, signal, trap_cx);
            }
        }
        
        drop(thread_inner);
        
        // 如果没有线程级别的信号，检查进程级别的信号
        if let Some(parent) = thread.parent_process.upgrade() {
            let parent_inner = parent.inner_exclusive_access();
            if let Some(signal) = parent_inner.next_signal() {
                drop(parent_inner);
                return Self::deliver_process_signal_to_thread(thread, signal, trap_cx);
            }
        }
        
        (true, None)
    }

    /// 投递线程级别的信号
    fn deliver_thread_signal(
        thread: &Arc<ThreadControlBlock>,
        signal: Signal,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        let thread_inner = thread.inner_exclusive_access();
        let signal_state = thread_inner.signal_state.as_ref().unwrap();
        
        // 首先检查线程级别的处理器
        let handler = if let Some(thread_handler) = signal_state.get_thread_handler(signal) {
            thread_handler
        } else {
            // 回退到进程级别的处理器
            drop(thread_inner);
            if let Some(parent) = thread.parent_process.upgrade() {
                let parent_inner = parent.inner_exclusive_access();
                let handler = parent_inner.get_signal_handler(signal);
                drop(parent_inner);
                handler
            } else {
                SignalDisposition {
                    action: signal.default_action(),
                    mask: SignalSet::new(),
                    flags: 0,
                }
            }
        };
        
        match handler.action {
            SignalAction::Ignore => {
                debug!("Thread signal {} ignored", signal as u32);
                (true, None)
            }
            SignalAction::Terminate => {
                info!("Thread signal {} terminates thread {}", signal as u32, thread.thread_id.0);
                (false, Some(signal as i32))
            }
            SignalAction::Stop => {
                let thread_inner = thread.inner_exclusive_access();
                if let Some(signal_state) = thread_inner.signal_state.as_ref() {
                    signal_state.suspend_by_signal();
                }
                drop(thread_inner);
                
                // 设置线程状态为阻塞
                thread.set_status(crate::thread::ThreadStatus::Blocked);
                info!("Thread signal {} stops thread {}", signal as u32, thread.thread_id.0);
                (true, None)
            }
            SignalAction::Continue => {
                let thread_inner = thread.inner_exclusive_access();
                if let Some(signal_state) = thread_inner.signal_state.as_ref() {
                    signal_state.resume_from_signal();
                }
                drop(thread_inner);
                
                // 设置线程状态为就绪
                thread.set_status(crate::thread::ThreadStatus::Ready);
                info!("Thread signal {} continues thread {}", signal as u32, thread.thread_id.0);
                (true, None)
            }
            SignalAction::Handler(handler_addr) => {
                info!("Thread signal {} executing handler at {:#x} for thread {}", 
                      signal as u32, handler_addr, thread.thread_id.0);
                
                Self::setup_thread_signal_handler(thread, signal, handler_addr, &handler, trap_cx);
                (true, None)
            }
        }
    }

    /// 投递进程级别的信号到特定线程
    fn deliver_process_signal_to_thread(
        thread: &Arc<ThreadControlBlock>,
        signal: Signal,
        trap_cx: &mut TrapContext,
    ) -> (bool, Option<i32>) {
        // 使用公开的信号处理逻辑
        if let Some(parent) = thread.parent_process.upgrade() {
            // 手动实现信号投递逻辑，而不是调用私有方法
            let parent_inner = parent.inner_exclusive_access();
            let handler = parent_inner.get_signal_handler(signal);
            drop(parent_inner);

            match handler.action {
                SignalAction::Ignore => {
                    debug!("Process signal {} ignored by thread", signal as u32);
                    (true, None)
                }
                SignalAction::Terminate => {
                    info!("Process signal {} terminates thread {}", signal as u32, thread.thread_id.0);
                    (false, Some(signal as i32))
                }
                SignalAction::Stop => {
                    let thread_inner = thread.inner_exclusive_access();
                    if let Some(signal_state) = thread_inner.signal_state.as_ref() {
                        signal_state.suspend_by_signal();
                    }
                    drop(thread_inner);
                    
                    thread.set_status(crate::thread::ThreadStatus::Blocked);
                    info!("Process signal {} stops thread {}", signal as u32, thread.thread_id.0);
                    (true, None)
                }
                SignalAction::Continue => {
                    let thread_inner = thread.inner_exclusive_access();
                    if let Some(signal_state) = thread_inner.signal_state.as_ref() {
                        signal_state.resume_from_signal();
                    }
                    drop(thread_inner);
                    
                    thread.set_status(crate::thread::ThreadStatus::Ready);
                    info!("Process signal {} continues thread {}", signal as u32, thread.thread_id.0);
                    (true, None)
                }
                SignalAction::Handler(handler_addr) => {
                    info!("Process signal {} executing handler at {:#x} for thread {}", 
                          signal as u32, handler_addr, thread.thread_id.0);
                    
                    Self::setup_thread_signal_handler(thread, signal, handler_addr, &handler, trap_cx);
                    (true, None)
                }
            }
        } else {
            (true, None)
        }
    }

    /// 为线程设置信号处理器
    fn setup_thread_signal_handler(
        thread: &Arc<ThreadControlBlock>,
        signal: Signal,
        handler_addr: usize,
        handler_info: &SignalDisposition,
        trap_cx: &mut TrapContext,
    ) {
        // 获取线程的用户栈
        let thread_inner = thread.inner_exclusive_access();
        let user_stack_top = thread_inner.user_stack.sp;
        
        // 创建信号帧
        let signal_frame = SignalFrame {
            regs: trap_cx.x,
            pc: trap_cx.sepc,
            status: trap_cx.sstatus.bits(),
            signal: signal as u32,
            return_addr: Self::get_thread_sigreturn_addr(),
        };

        // 在线程栈上分配信号帧空间
        let signal_frame_size = core::mem::size_of::<SignalFrame>();
        let aligned_size = (signal_frame_size + 15) & !15; // 16字节对齐
        let signal_frame_addr = user_stack_top - aligned_size;
        
        // 获取用户token进行地址转换
        let user_token = if let Some(parent) = thread.parent_process.upgrade() {
            let parent_inner = parent.inner_exclusive_access();
            parent_inner.get_user_token()
        } else {
            drop(thread_inner);
            return;
        };
        
        // 检查地址有效性
        if signal_frame_addr < 0x10000 || signal_frame_addr >= 0x80000000 {
            warn!("Thread signal frame address out of range: {:#x}", signal_frame_addr);
            drop(thread_inner);
            return;
        }
        
        // 写入信号帧
        let frame_ptr = signal_frame_addr as *mut SignalFrame;
        let frame_ref = crate::memory::page_table::translated_ref_mut(user_token, frame_ptr);
        *frame_ref = signal_frame;
        
        // 设置线程信号状态
        if let Some(signal_state) = thread_inner.signal_state.as_ref() {
            signal_state.enter_signal_handler(signal, handler_info.mask);
            
            // 检查SA_NODEFER标志
            if (handler_info.flags & SA_NODEFER) == 0 {
                let mut current_signal_mask = SignalSet::new();
                current_signal_mask.add(signal);
                signal_state.block_signals(current_signal_mask);
            }
        }
        
        drop(thread_inner);
        
        // 修改trap context
        trap_cx.sepc = handler_addr;
        trap_cx.x[2] = signal_frame_addr; // 更新栈指针
        trap_cx.x[10] = signal as usize;  // a0寄存器传递信号号码
        trap_cx.x[1] = Self::get_thread_sigreturn_addr(); // 返回地址
        
        debug!("Thread signal {} handler setup: pc={:#x}, sp={:#x}, frame={:#x}", 
               signal as u32, handler_addr, signal_frame_addr, signal_frame_addr);
    }

    /// 获取线程sigreturn的地址
    fn get_thread_sigreturn_addr() -> usize {
        // 返回一个特殊值，用于标识线程信号返回
        0x1000 // 特殊的线程信号返回地址
    }

    /// 线程信号返回处理
    pub fn thread_sigreturn(
        thread: &Arc<ThreadControlBlock>,
        trap_cx: &mut TrapContext,
    ) -> bool {
        let user_sp = trap_cx.x[2];
        
        // 计算信号帧地址
        let signal_frame_size = core::mem::size_of::<SignalFrame>();
        let aligned_size = (signal_frame_size + 15) & !15;
        let signal_frame_addr = user_sp;
        
        debug!("Thread sigreturn: sp={:#x}, frame_addr={:#x}", user_sp, signal_frame_addr);
        
        // 检查地址有效性
        if signal_frame_addr < 0x10000 || signal_frame_addr >= 0x80000000 {
            error!("Invalid thread signal frame address: {:#x}", signal_frame_addr);
            return false;
        }
        
        // 获取用户token
        let user_token = if let Some(parent) = thread.parent_process.upgrade() {
            let parent_inner = parent.inner_exclusive_access();
            parent_inner.get_user_token()
        } else {
            return false;
        };
        
        // 读取信号帧
        let signal_frame = {
            let frame_ptr = signal_frame_addr as *const SignalFrame;
            let frame_ref = crate::memory::page_table::translated_ref_mut(user_token, frame_ptr as *mut SignalFrame);
            *frame_ref
        };
        
        // 验证信号帧
        if signal_frame.signal == 0 || signal_frame.signal > 31 {
            error!("Invalid signal number in thread frame: {}", signal_frame.signal);
            return false;
        }
        
        // 恢复寄存器状态
        trap_cx.x = signal_frame.regs;
        trap_cx.sepc = signal_frame.pc;
        
        // 恢复状态寄存器
        let mut current_sstatus = riscv::register::sstatus::read();
        let saved_bits = signal_frame.status;
        
        if (saved_bits & (1 << 8)) != 0 {
            current_sstatus.set_spp(riscv::register::sstatus::SPP::Supervisor);
        } else {
            current_sstatus.set_spp(riscv::register::sstatus::SPP::User);
        }
        
        if (saved_bits & (1 << 5)) != 0 {
            current_sstatus.set_spie(true);
        } else {
            current_sstatus.set_spie(false);
        }
        
        trap_cx.sstatus = current_sstatus;
        
        // 恢复线程信号状态
        let thread_inner = thread.inner_exclusive_access();
        if let Some(signal_state) = thread_inner.signal_state.as_ref() {
            signal_state.exit_signal_handler();
        }
        drop(thread_inner);
        
        // 恢复栈指针
        trap_cx.x[2] = signal_frame.regs[2];
        
        debug!("Thread signal {} sigreturn completed: pc={:#x}, sp={:#x}", 
               signal_frame.signal, trap_cx.sepc, trap_cx.x[2]);
        
        true
    }
}

/// 向指定线程发送信号
pub fn send_signal_to_thread(
    parent_process: &Arc<TaskControlBlock>,
    thread_id: ThreadId,
    signal: Signal,
) -> Result<(), &'static str> {
    let mut parent_inner = parent_process.inner_exclusive_access();
    
    if let Some(thread_manager) = parent_inner.thread_manager.as_ref() {
        if let Some(thread) = thread_manager.find_thread(thread_id) {
            let thread_inner = thread.inner_exclusive_access();
            
            if let Some(signal_state) = thread_inner.signal_state.as_ref() {
                signal_state.send_signal(signal);
                
                // 如果是不可捕获的信号，直接处理
                if signal.is_uncatchable() {
                    match signal {
                        Signal::SIGKILL => {
                            thread.set_status(crate::thread::ThreadStatus::Exited);
                        }
                        Signal::SIGSTOP => {
                            signal_state.suspend_by_signal();
                            thread.set_status(crate::thread::ThreadStatus::Blocked);
                        }
                        _ => unreachable!(),
                    }
                }
                
                drop(thread_inner);
                
                debug!("Signal {} sent to thread {}", signal as u32, thread_id.0);
                return Ok(());
            }
        }
    }
    
    Err("Thread not found or signal state not initialized")
}

/// 检查并处理线程信号
pub fn check_and_handle_thread_signals(
    thread: &Arc<ThreadControlBlock>,
) -> (bool, Option<i32>) {
    let thread_inner = thread.inner_exclusive_access();
    
    // 检查是否有待处理的线程信号
    let has_signals = if let Some(signal_state) = thread_inner.signal_state.as_ref() {
        signal_state.has_deliverable_signals()
    } else {
        false
    };
    
    drop(thread_inner);
    
    if has_signals {
        // 获取trap context并处理信号
        let trap_cx = thread.get_trap_cx();
        ThreadSignalDelivery::handle_thread_signals(thread, trap_cx)
    } else {
        (true, None)
    }
}

/// 创建线程时的信号状态继承
pub fn inherit_signal_state_for_thread(
    thread: &Arc<ThreadControlBlock>,
    parent_thread: Option<&Arc<ThreadControlBlock>>,
) {
    let mut thread_inner = thread.inner_exclusive_access();
    
    if thread_inner.signal_state.is_none() {
        let signal_state = ThreadSignalState::new();
        
        // 如果有父线程，继承其信号状态
        if let Some(parent) = parent_thread {
            let parent_inner = parent.inner_exclusive_access();
            if let Some(parent_signal_state) = parent_inner.signal_state.as_ref() {
                signal_state.inherit_from_parent(parent_signal_state);
            }
        }
        
        thread_inner.signal_state = Some(signal_state);
        info!("Thread signal state initialized for thread {}", thread.thread_id.0);
    }
}

/// 线程退出时的信号清理
pub fn cleanup_thread_signals(thread: &Arc<ThreadControlBlock>) {
    let mut thread_inner = thread.inner_exclusive_access();
    
    if let Some(signal_state) = thread_inner.signal_state.take() {
        // 清理待处理的信号
        let mut pending = signal_state.pending.lock();
        pending.clear();
        
        // 清理线程处理器
        let mut handlers = signal_state.thread_handlers.lock();
        handlers.clear();
        
        debug!("Thread signal state cleaned up for thread {}", thread.thread_id.0);
    }
}