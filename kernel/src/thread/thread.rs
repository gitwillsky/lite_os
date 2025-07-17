use alloc::{sync::{Arc, Weak}, vec::Vec};
use crate::{
    task::TaskContext,
    memory::{
        address::{VirtualAddress, PhysicalPageNumber},
    },
    trap::TrapContext,
    thread::signal::ThreadSignalState,
};

/// 线程ID类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(pub usize);

/// 线程状态
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum ThreadStatus {
    Ready,
    Running,
    Blocked,
    Zombie,
    Exited,
}

/// 线程用户栈信息
#[derive(Debug)]
pub struct ThreadStack {
    /// 用户栈虚拟地址范围
    pub start_va: VirtualAddress,
    pub end_va: VirtualAddress,
    /// 栈大小
    pub size: usize,
    /// 当前栈指针
    pub sp: usize,
}

/// 线程控制块内部数据
#[derive(Debug)]
pub struct ThreadControlBlockInner {
    /// 线程状态
    pub status: ThreadStatus,
    /// 线程上下文 - 用于内核态上下文切换
    pub context: TaskContext,
    /// 陷入上下文物理页号 - 每个线程需要独立的陷入上下文
    pub trap_cx_ppn: PhysicalPageNumber,
    /// 用户栈信息
    pub user_stack: ThreadStack,
    /// 内核栈信息
    pub kernel_stack_base: usize,
    pub kernel_stack_top: usize,
    /// 退出码
    pub exit_code: i32,
    /// 是否可以被join
    pub joinable: bool,
    /// 等待join的线程列表
    pub waiting_threads: Vec<ThreadId>,
    /// 线程私有数据
    pub thread_local_data: Option<usize>,
    /// 线程入口点
    pub entry_point: usize,
    /// 线程参数
    pub thread_arg: usize,
    /// CPU亲和性
    pub cpu_affinity: Option<usize>,
    /// 线程信号状态
    pub signal_state: Option<ThreadSignalState>,
}

/// 线程控制块
#[derive(Debug)]
pub struct ThreadControlBlock {
    /// 线程ID
    pub thread_id: ThreadId,
    /// 所属进程的TaskControlBlock
    pub parent_process: Weak<crate::task::TaskControlBlock>,
    /// 内部数据
    inner: spin::Mutex<ThreadControlBlockInner>,
}

impl ThreadControlBlock {
    /// 创建主线程（复用进程的现有资源）
    pub fn new_main_thread(
        thread_id: ThreadId,
        parent_process: Weak<crate::task::TaskControlBlock>,
        trap_cx_ppn: PhysicalPageNumber,
    ) -> Self {
        // 主线程使用进程的现有栈和陷入上下文，不需要分配新的资源
        let inner = ThreadControlBlockInner {
            status: ThreadStatus::Running,
            context: crate::task::TaskContext::zero_init(),
            trap_cx_ppn, // 使用进程的实际陷入上下文页面
            user_stack: ThreadStack { start_va: VirtualAddress::from(0), end_va: VirtualAddress::from(0), size: 0, sp: 0 }, // 主线程使用进程的现有栈
            kernel_stack_base: 0, // 使用进程的内核栈
            kernel_stack_top: 0,
            exit_code: 0,
            joinable: false, // 主线程不能被join
            waiting_threads: Vec::new(),
            thread_local_data: None,
            entry_point: 0, // 主线程已经在运行中
            thread_arg: 0,
            cpu_affinity: None,
            signal_state: Some(crate::thread::signal::ThreadSignalState::new()),
        };

        Self {
            thread_id,
            parent_process,
            inner: spin::Mutex::new(inner),
        }
    }

    /// 创建新线程
    pub fn new(
        thread_id: ThreadId,
        parent_process: Weak<crate::task::TaskControlBlock>,
        entry_point: usize,
        user_stack: ThreadStack,
        kernel_stack_base: usize,
        kernel_stack_size: usize,
        trap_cx_ppn: PhysicalPageNumber,
        thread_arg: usize,
        joinable: bool,
        user_token: usize, // 添加用户空间页表token参数
    ) -> Self {
        let kernel_stack_top = kernel_stack_base + kernel_stack_size;

        let tcb = Self {
            thread_id,
            parent_process,
            inner: spin::Mutex::new(ThreadControlBlockInner {
                status: ThreadStatus::Ready,
                context: TaskContext::goto_trap_return(kernel_stack_top),
                trap_cx_ppn,
                user_stack,
                kernel_stack_base,
                kernel_stack_top,
                exit_code: 0,
                joinable,
                waiting_threads: Vec::new(),
                thread_local_data: None,
                entry_point,
                thread_arg,
                cpu_affinity: None,
                signal_state: Some(ThreadSignalState::new()),
            }),
        };

        // 初始化陷入上下文，传入用户页表token
        tcb.init_trap_context(user_token);
        tcb
    }

    /// 初始化陷入上下文
    fn init_trap_context(&self, user_token: usize) {
        let inner = self.inner.lock();

        debug!("Initializing trap context for thread {}: ppn={:#x}", self.thread_id.0, inner.trap_cx_ppn.as_usize());

        // 获取线程的陷入上下文页面
        let trap_cx = inner.trap_cx_ppn.get_mut::<TrapContext>();

        debug!("Got trap context pointer: {:#x}", trap_cx as *mut _ as usize);

        // 初始化陷入上下文
        *trap_cx = TrapContext::app_init_context(
            inner.entry_point,
            inner.user_stack.sp,
            user_token, // 使用传入的用户页表token
            inner.kernel_stack_top,
            crate::trap::trap_handler as usize,
        );

        debug!("Initialized trap context for thread {}", self.thread_id.0);

        // 设置线程函数参数
        // 线程函数地址需要保存到不会被系统调用覆盖的寄存器中
        // 我们使用 s0 寄存器 (x[8]) 来保存线程函数地址，因为它是被调用者保存的寄存器
        // 在用户空间的 thread_wrapper 中需要相应地从 s0 寄存器获取函数地址
        trap_cx.x[8] = inner.thread_arg; // s0 register - 线程函数地址
        trap_cx.x[10] = 0; // a0 register - 初始化为0
        trap_cx.x[11] = 0; // a1 register - 初始化为0

        debug!("Set thread function address {:#x} in s0 for thread {}", inner.thread_arg, self.thread_id.0);
    }

    /// 获取线程ID
    pub fn get_thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// 获取内部数据的独占访问
    pub fn inner_exclusive_access(&self) -> spin::MutexGuard<'_, ThreadControlBlockInner> {
        self.inner.lock()
    }

    /// 获取线程状态
    pub fn get_status(&self) -> ThreadStatus {
        self.inner.lock().status
    }

    /// 设置线程状态
    pub fn set_status(&self, status: ThreadStatus) {
        self.inner.lock().status = status;
    }

    /// 获取陷入上下文
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.inner.lock().trap_cx_ppn.get_mut()
    }

    /// 获取用户token（页表）
    pub fn get_user_token(&self) -> usize {
        if let Some(parent) = self.parent_process.upgrade() {
            parent.inner_exclusive_access().get_user_token()
        } else {
            0
        }
    }

    /// 线程退出
    pub fn exit(&self, exit_code: i32) {
        let mut inner = self.inner.lock();
        inner.status = ThreadStatus::Exited;
        inner.exit_code = exit_code;

        // 记录等待join的线程，用于后续唤醒
        let waiting_threads = inner.waiting_threads.clone();
        inner.waiting_threads.clear();
        drop(inner);

        // 唤醒等待join的线程
        for waiting_thread_id in waiting_threads {
            if let Some(parent) = self.parent_process.upgrade() {
                let mut parent_inner = parent.inner_exclusive_access();
                if let Some(thread_manager) = parent_inner.thread_manager.as_mut() {
                    thread_manager.wakeup_thread(waiting_thread_id);
                }
            }
        }
    }

    /// 加入等待join的线程
    pub fn add_waiting_thread(&self, thread_id: ThreadId) {
        self.inner.lock().waiting_threads.push(thread_id);
    }

    /// 获取等待join的线程列表
    pub fn get_waiting_threads(&self) -> Vec<ThreadId> {
        self.inner.lock().waiting_threads.clone()
    }

    /// 检查是否可以被join
    pub fn is_joinable(&self) -> bool {
        self.inner.lock().joinable
    }

    /// 获取退出码
    pub fn get_exit_code(&self) -> i32 {
        self.inner.lock().exit_code
    }

    /// 获取任务上下文指针（需要在持有锁的情况下使用）
    pub fn get_task_cx_ptr(&self) -> *const TaskContext {
        // 这个方法不安全，因为返回的指针可能在锁释放后失效
        // 我们需要重新设计这个方法
        panic!("get_task_cx_ptr is unsafe - use get_task_cx_mut instead");
    }

    /// 获取任务上下文的可变引用（需要在持有锁的情况下使用）
    pub fn get_task_cx_mut(&self) -> &mut TaskContext {
        // 这个方法也不安全，因为需要静态生命周期
        panic!("get_task_cx_mut needs redesign");
    }

    /// 设置线程私有数据
    pub fn set_thread_local_data(&self, data: usize) {
        self.inner.lock().thread_local_data = Some(data);
    }

    /// 获取线程私有数据
    pub fn get_thread_local_data(&self) -> Option<usize> {
        self.inner.lock().thread_local_data
    }

    /// 设置CPU亲和性
    pub fn set_cpu_affinity(&self, cpu_id: usize) {
        self.inner.lock().cpu_affinity = Some(cpu_id);
    }

    /// 获取CPU亲和性
    pub fn get_cpu_affinity(&self) -> Option<usize> {
        self.inner.lock().cpu_affinity
    }

    /// 准备线程切换
    pub fn prepare_context_switch(&self) {
        let _inner = self.inner.lock();
        // 这里可以添加上下文切换前的准备工作
        // 例如保存浮点寄存器状态等
    }

    /// 完成线程切换后的清理
    pub fn finish_context_switch(&self) {
        let _inner = self.inner.lock();
        // 这里可以添加上下文切换后的清理工作
        // 例如恢复浮点寄存器状态等
    }

    /// 获取线程的陷入上下文
    pub fn get_trap_context(&self) -> *mut TrapContext {
        let inner = self.inner.lock();
        inner.trap_cx_ppn.get_mut::<TrapContext>()
    }

    /// 保存trap context到线程的私有trap context中
    pub fn save_trap_context(&self, process_trap_cx: &TrapContext) {
        let inner = self.inner.lock();
        let thread_trap_cx = inner.trap_cx_ppn.get_mut::<TrapContext>();
        // 手动复制TrapContext的字段
        thread_trap_cx.x = process_trap_cx.x;
        thread_trap_cx.sstatus = process_trap_cx.sstatus;
        thread_trap_cx.sepc = process_trap_cx.sepc;
        thread_trap_cx.kernel_satp = process_trap_cx.kernel_satp;
        thread_trap_cx.kernel_sp = process_trap_cx.kernel_sp;
        thread_trap_cx.trap_handler = process_trap_cx.trap_handler;
    }

    /// 从线程的私有trap context加载到进程的trap context中
    pub fn load_trap_context(&self, process_trap_cx: &mut TrapContext) {
        let inner = self.inner.lock();
        let thread_trap_cx = inner.trap_cx_ppn.get_mut::<TrapContext>();
        // 手动复制TrapContext的字段
        process_trap_cx.x = thread_trap_cx.x;
        process_trap_cx.sstatus = thread_trap_cx.sstatus;
        process_trap_cx.sepc = thread_trap_cx.sepc;
        process_trap_cx.kernel_satp = thread_trap_cx.kernel_satp;
        process_trap_cx.kernel_sp = thread_trap_cx.kernel_sp;
        process_trap_cx.trap_handler = thread_trap_cx.trap_handler;
    }
}

impl ThreadControlBlockInner {
    /// 获取线程上下文指针
    pub fn get_context_ptr(&mut self) -> *mut TaskContext {
        &mut self.context as *mut TaskContext
    }

    /// 获取陷入上下文
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    /// 检查线程是否处于运行状态
    pub fn is_running(&self) -> bool {
        self.status == ThreadStatus::Running
    }

    /// 检查线程是否已退出
    pub fn is_exited(&self) -> bool {
        matches!(self.status, ThreadStatus::Exited | ThreadStatus::Zombie)
    }

    /// 获取用户栈指针
    pub fn get_user_sp(&self) -> usize {
        self.user_stack.sp
    }

    /// 设置用户栈指针
    pub fn set_user_sp(&mut self, sp: usize) {
        self.user_stack.sp = sp;
    }
}

impl ThreadStack {
    /// 创建新的线程栈
    pub fn new(start_va: VirtualAddress, size: usize) -> Self {
        let end_va = VirtualAddress::from(start_va.as_usize() + size);
        Self {
            start_va,
            end_va,
            size,
            sp: end_va.as_usize(), // 栈从高地址向低地址增长
        }
    }

    /// 检查栈指针是否在有效范围内
    pub fn is_valid_sp(&self, sp: usize) -> bool {
        sp >= self.start_va.as_usize() && sp <= self.end_va.as_usize()
    }

    /// 获取栈的剩余空间
    pub fn remaining_space(&self) -> usize {
        if self.sp > self.start_va.as_usize() {
            self.sp - self.start_va.as_usize()
        } else {
            0
        }
    }

    /// 栈溢出检查
    pub fn check_overflow(&self, sp: usize) -> bool {
        sp < self.start_va.as_usize()
    }
}