use alloc::{sync::{Arc, Weak}, vec::Vec};
use crate::{
    sync::UPSafeCell,
    task::TaskContext,
    memory::{
        address::{VirtualAddress, PhysicalPageNumber},
    },
    trap::TrapContext,
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
}

/// 线程控制块
#[derive(Debug)]
pub struct ThreadControlBlock {
    /// 线程ID
    pub thread_id: ThreadId,
    /// 所属进程的TaskControlBlock
    pub parent_process: Weak<crate::task::TaskControlBlock>,
    /// 内部数据
    inner: UPSafeCell<ThreadControlBlockInner>,
}

impl ThreadControlBlock {
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
    ) -> Self {
        let kernel_stack_top = kernel_stack_base + kernel_stack_size;
        
        let tcb = Self {
            thread_id,
            parent_process,
            inner: UPSafeCell::new(ThreadControlBlockInner {
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
            }),
        };

        // 初始化陷入上下文
        tcb.init_trap_context();
        tcb
    }

    /// 初始化陷入上下文
    fn init_trap_context(&self) {
        if let Some(parent) = self.parent_process.upgrade() {
            let parent_inner = parent.inner_exclusive_access();
            let inner = self.inner.exclusive_access();
            
            // 获取线程的陷入上下文页面
            let trap_cx = inner.trap_cx_ppn.get_mut::<TrapContext>();
            
            // 初始化陷入上下文
            *trap_cx = TrapContext::app_init_context(
                inner.entry_point,
                inner.user_stack.sp,
                parent_inner.get_user_token(),
                inner.kernel_stack_top,
                crate::trap::trap_handler as usize,
            );
            
            // 设置线程参数 (通过a0寄存器传递)
            trap_cx.x[10] = inner.thread_arg; // a0 register
        }
    }

    /// 获取线程ID
    pub fn get_thread_id(&self) -> ThreadId {
        self.thread_id
    }

    /// 获取内部数据的独占访问
    pub fn inner_exclusive_access(&self) -> core::cell::RefMut<'_, ThreadControlBlockInner> {
        self.inner.exclusive_access()
    }

    /// 获取线程状态
    pub fn get_status(&self) -> ThreadStatus {
        self.inner.exclusive_access().status
    }

    /// 设置线程状态
    pub fn set_status(&self, status: ThreadStatus) {
        self.inner.exclusive_access().status = status;
    }

    /// 获取陷入上下文
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.inner.exclusive_access().trap_cx_ppn.get_mut()
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
        let mut inner = self.inner.exclusive_access();
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
        self.inner.exclusive_access().waiting_threads.push(thread_id);
    }

    /// 检查是否可以被join
    pub fn is_joinable(&self) -> bool {
        self.inner.exclusive_access().joinable
    }

    /// 获取退出码
    pub fn get_exit_code(&self) -> i32 {
        self.inner.exclusive_access().exit_code
    }

    /// 设置线程私有数据
    pub fn set_thread_local_data(&self, data: usize) {
        self.inner.exclusive_access().thread_local_data = Some(data);
    }

    /// 获取线程私有数据
    pub fn get_thread_local_data(&self) -> Option<usize> {
        self.inner.exclusive_access().thread_local_data
    }

    /// 设置CPU亲和性
    pub fn set_cpu_affinity(&self, cpu_id: usize) {
        self.inner.exclusive_access().cpu_affinity = Some(cpu_id);
    }

    /// 获取CPU亲和性
    pub fn get_cpu_affinity(&self) -> Option<usize> {
        self.inner.exclusive_access().cpu_affinity
    }

    /// 准备线程切换
    pub fn prepare_context_switch(&self) {
        let _inner = self.inner.exclusive_access();
        // 这里可以添加上下文切换前的准备工作
        // 例如保存浮点寄存器状态等
    }

    /// 完成线程切换后的清理
    pub fn finish_context_switch(&self) {
        let _inner = self.inner.exclusive_access();
        // 这里可以添加上下文切换后的清理工作
        // 例如恢复浮点寄存器状态等
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