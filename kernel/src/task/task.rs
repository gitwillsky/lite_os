use core::{cell::RefMut, error::Error};

use alloc::{
    boxed::Box, string::{String, ToString}, sync::{Arc, Weak}, vec::Vec, collections::BTreeMap
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
    fs::inode::Inode,
};

pub struct FileDescriptor {
    pub inode: Arc<dyn Inode>,
    pub offset: UPSafeCell<u64>,
    pub flags: u32,
    pub mode: u32,
}

impl core::fmt::Debug for FileDescriptor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FileDescriptor")
            .field("offset", &self.offset)
            .field("flags", &self.flags)
            .field("mode", &self.mode)
            .finish()
    }
}

impl FileDescriptor {
    pub fn new(inode: Arc<dyn Inode>, flags: u32) -> Self {
        Self {
            inode,
            offset: UPSafeCell::new(0),
            flags,
            mode: 0o644, // Default file mode
        }
    }
    
    pub fn read_at(&self, buf: &mut [u8]) -> Result<usize, crate::fs::FileSystemError> {
        let mut offset = self.offset.exclusive_access();
        let result = self.inode.read_at(*offset, buf);
        if let Ok(bytes_read) = result {
            *offset += bytes_read as u64;
        }
        result
    }
    
    pub fn write_at(&self, buf: &[u8]) -> Result<usize, crate::fs::FileSystemError> {
        let mut offset = self.offset.exclusive_access();
        let result = self.inode.write_at(*offset, buf);
        if let Ok(bytes_written) = result {
            *offset += bytes_written as u64;
        }
        result
    }
}

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
    /// 当前工作目录
    pub cwd: String,
    /// 文件描述符表
    pub fd_table: BTreeMap<usize, Arc<FileDescriptor>>,
    /// 下一个可分配的文件描述符
    pub next_fd: usize,
}

/// Task Control block structure
#[derive(Debug)]
pub struct TaskControlBlock {
    pub pid: PidHandle,
    pub kernel_stack: KernelStack,

    inner: UPSafeCell<TaskControlBlockInner>,
}

impl TaskControlBlock {
    pub fn new(elf_data: &[u8]) -> Result<Self, Box<dyn Error>> {
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data)?;

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
                cwd: "/".to_string(),  // 新进程默认工作目录为根目录
                fd_table: BTreeMap::new(),
                next_fd: 3, // 0, 1, 2 reserved for stdin, stdout, stderr
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
        Ok(tcb)
    }

    pub fn get_pid(&self) -> usize {
        self.pid.0
    }

    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }

    pub fn exec(&self, elf_data: &[u8]) -> Result<(), Box<dyn Error>> {
        let (memory_set, user_stack_top, entrypoint) = MemorySet::from_elf(elf_data)?;
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
        Ok(())
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
                cwd: parent_inner.cwd.clone(),  // 复制父进程的工作目录
                fd_table: parent_inner.fd_table.clone(), // 复制父进程的文件描述符表
                next_fd: parent_inner.next_fd,
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

    /// 分配新的文件描述符
    pub fn alloc_fd(&mut self, file_desc: Arc<FileDescriptor>) -> usize {
        let fd = self.next_fd;
        self.fd_table.insert(fd, file_desc);
        self.next_fd += 1;
        fd
    }

    /// 根据文件描述符获取FileDescriptor
    pub fn get_fd(&self, fd: usize) -> Option<Arc<FileDescriptor>> {
        self.fd_table.get(&fd).cloned()
    }

    /// 关闭文件描述符
    pub fn close_fd(&mut self, fd: usize) -> bool {
        self.fd_table.remove(&fd).is_some()
    }

    /// 关闭所有文件描述符（进程退出时调用）
    pub fn close_all_fds(&mut self) {
        self.fd_table.clear();
    }
}
