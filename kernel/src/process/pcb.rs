use crate::{memory::address::VirtualAddress, trap::TrapContext};

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ProcessState {
    Ready,
    Running,
    Blocked,
    Zombie,
}

pub struct ProcessControlBlock {
    pub pid: usize, // 进程 ID
    pub state: ProcessState,
    pub trap_ctx_ptr: VirtualAddress,
    pub kernel_stack_top: VirtualAddress,
}

impl ProcessControlBlock {
    pub fn trap_context_mut(&self) -> &mut TrapContext {
        unsafe {
            (self.trap_ctx_ptr.as_usize() as *mut TrapContext)
                .as_mut()
                .unwrap()
        }
    }
}
