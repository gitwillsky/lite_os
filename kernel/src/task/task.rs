use core::cell::RefMut;

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use crate::{
    memory::{
        KERNEL_SPACE, TRAP_CONTEXT,
        address::{PhysicalPageNumber, VirtualAddress},
        mm::{self, MemorySet},
    },
    sync::UPSafeCell,
    task::{
        context::TaskContext,
        pid::{KernelStack, PidHandle, alloc_pid},
    },
    trap::{TrapContext, trap_handler},
};

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Exited,
    Zombie,
}

#[derive(Debug)]
pub struct TaskControlBlockInner {
    /// 进程状态
    pub task_status: TaskStatus,
    /// 用户态的 TaskContext
    pub task_cx: TaskContext,
    /// 用户态的内存空间
    pub memory_set: mm::MemorySet,
    /// 用户态的 TrapContext 的物理页号
    pub trap_cx_ppn: PhysicalPageNumber,
    /// 应用数据仅有可能出现在应用地址空间低于 base_size 字节的区域中。
    /// 借助它我们可以清楚的知道应用有多少数据驻留在内存中。
    pub base_size: usize,
    /// 父进程, 子进程有可能在父进程退出时还存活，因此需要弱引用
    pub parent: Option<Weak<TaskControlBlock>>,
    /// 子进程
    pub children: Vec<Arc<TaskControlBlock>>,
    /// 子进程退出时，父进程可以获取其退出码
    pub exit_code: i32,
}

/// Task Control block structure
#[derive(Debug)]
pub struct TaskControlBlock {
    pub pid: PidHandle,
    pub kernel_stack: KernelStack,

    inner: UPSafeCell<TaskControlBlockInner>,
}

impl TaskControlBlock {
    pub fn new(elf_data: &[u8]) -> Self {
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);

        let task_status = TaskStatus::Ready;

        let pid = alloc_pid();
        let kernel_stack = KernelStack::new(pid.0);
        let kernel_stack_top = kernel_stack.get_top();

        // 获取用户空间中TRAP_CONTEXT映射的物理页面
        let trap_cx_ppn = memory_set
            .translate(VirtualAddress::from(TRAP_CONTEXT).into())
            .expect("TRAP_CONTEXT should be mapped")
            .ppn();

        let tcb = Self {
            pid,
            kernel_stack,
            inner: UPSafeCell::new(TaskControlBlockInner {
                task_status,
                task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                memory_set,
                trap_cx_ppn,
                base_size: user_sp,
                parent: None,
                children: Vec::new(),
                exit_code: 0,
            }),
        };

        // prepare TrapContext in user space
        let trap_cx = tcb.inner_exclusive_access().get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        tcb
    }

    pub fn get_pid(&self) -> usize {
        self.pid.0
    }

    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }

    pub fn exec(&self, elf_data: &[u8]) {
        let (memory_set, user_stack_top, entrypoint) = MemorySet::from_elf(elf_data);
        let trap_cx_ppn = memory_set
            .translate(VirtualAddress::from(TRAP_CONTEXT).into())
            .unwrap()
            .ppn();

        let mut inner = self.inner_exclusive_access();
        inner.trap_cx_ppn = trap_cx_ppn;
        inner.memory_set = memory_set;
        let trap_cx = inner.get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entrypoint,
            user_stack_top,
            KERNEL_SPACE.wait().lock().token(),
            self.kernel_stack.get_top(),
            trap_handler as usize,
        );
    }

    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        let mut parent_inner = self.inner_exclusive_access();
        let memory_set = MemorySet::form_existed_user(&parent_inner.memory_set);
        let trap_cx_ppn = memory_set
            .translate(VirtualAddress::from(TRAP_CONTEXT).into())
            .unwrap()
            .ppn();

        // alloc a pid and a kernel stack in kernel space
        let pid = alloc_pid();
        let kernel_stack = KernelStack::new(pid.0);
        let kernel_stack_top = kernel_stack.get_top();

        let tcb = Arc::new(TaskControlBlock {
            pid,
            kernel_stack,
            inner: UPSafeCell::new(TaskControlBlockInner {
                task_status: TaskStatus::Ready,
                task_cx: TaskContext::goto_trap_return(kernel_stack_top),
                memory_set,
                trap_cx_ppn,
                base_size: parent_inner.base_size,
                parent: Some(Arc::downgrade(self)),
                children: Vec::new(),
                exit_code: 0,
            }),
        });

        parent_inner.children.push(tcb.clone());
        let trap_cx = tcb.inner_exclusive_access().get_trap_cx();
        trap_cx.kernel_sp = kernel_stack_top;
        tcb
    }
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }

    pub fn get_status(&self) -> TaskStatus {
        self.task_status
    }

    pub fn is_zombie(&self) -> bool {
        self.task_status == TaskStatus::Zombie
    }
}
