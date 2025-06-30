use crate::{
    memory::{
        KERNEL_SPACE, TRAP_CONTEXT,
        address::{PhysicalPageNumber, VirtualAddress, VirtualPageNumber},
        kernel_stack_position,
        mm::{self, MapPermission, MemorySet},
    },
    task::context::TaskContext,
    trap::{TrapContext, trap_handler},
};

#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Exited,
}

/// Task Control block structure
pub struct TaskControlBlock {
    pub task_status: TaskStatus,
    pub task_cx: TaskContext,
    pub memory_set: mm::MemorySet,
    pub trap_cx_ppn: PhysicalPageNumber,

    pub base_size: usize,
    pub heap_bottom: usize,
    pub program_brk: usize,
}

impl TaskControlBlock {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }

    pub fn new(elf_data: &[u8], app_id: usize) -> Self {
        let (memory_set, user_sp, entry_point) = MemorySet::from_elf(elf_data);

        println!("[TaskControlBlock::new] app_id={}, entry_point={:#x}, user_sp={:#x}", app_id, entry_point, user_sp);

        // 为TRAP_CONTEXT分配一个物理页面
        let trap_cx_ppn = crate::memory::frame_allocator::alloc().unwrap().ppn;

        let task_status = TaskStatus::Ready;

        let (kernel_stack_bottom, kernel_stack_top) = kernel_stack_position(app_id);

        KERNEL_SPACE.wait().lock().insert_framed_area(
            kernel_stack_bottom.into(),
            kernel_stack_top.into(),
            MapPermission::R | MapPermission::W,
        );

        let mut tcb = Self {
            task_status,
            task_cx: TaskContext::goto_trap_return(kernel_stack_top),
            memory_set,
            trap_cx_ppn,
            base_size: user_sp,
            heap_bottom: user_sp,
            program_brk: user_sp,
        };

        // 修复TRAP_CONTEXT的映射：需要将虚拟地址映射到实际的TrapContext物理页面
        let trap_context_vpn: VirtualPageNumber = VirtualAddress::from(TRAP_CONTEXT).into();
        // 映射到正确的物理页面，注意需要添加用户权限U标志
        tcb.memory_set.map_one(
            trap_context_vpn,
            trap_cx_ppn,
            crate::memory::page_table::PTEFlags::R | crate::memory::page_table::PTEFlags::W | crate::memory::page_table::PTEFlags::U,
        );
        println!(
            "[TaskControlBlock::new] Mapped TRAP_CONTEXT: vpn={:#x} -> ppn={:#x}",
            trap_context_vpn.as_usize(), trap_cx_ppn.as_usize()
        );

        // prepare TrapContext in user space
        let trap_cx = tcb.get_trap_cx();
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.wait().lock().token(),
            kernel_stack_top,
            trap_handler as usize,
        );
        println!("[TaskControlBlock::new] TrapContext initialized: sepc={:#x}, sp={:#x}", trap_cx.sepc, trap_cx.x[2]);
        tcb
    }

    pub fn change_program_brk(&mut self, size: i32) -> Option<usize> {
        let old_brk = self.program_brk;
        let new_brk = self.program_brk as isize + size as isize;

        if new_brk < self.heap_bottom as isize {
            return None;
        }
        let result = if size < 0 {
            self.memory_set
                .shrink_to(self.heap_bottom.into(), (new_brk as usize).into())
        } else {
            self.memory_set
                .append_to(self.heap_bottom.into(), (new_brk as usize).into())
        };
        if result {
            self.program_brk = new_brk as usize;
            Some(old_brk)
        } else {
            None
        }
    }
}
