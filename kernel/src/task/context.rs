use crate::trap::trap_return;

#[repr(C)]
/// Task context structure containing some registers
pub struct TaskContext {
    /// return address
    ra: usize,
    /// kernel stack pointer of app
    kernel_sp: usize,
    /// callee saved registers: s 0..11
    s: [usize; 12],
}

impl TaskContext {
    pub fn zero_init() -> Self {
        Self {
            ra: 0,
            kernel_sp: 0,
            s: [0; 12],
        }
    }

    pub fn goto_trap_return(kernel_sp: usize) -> Self {
        Self {
            ra: trap_return as usize,
            kernel_sp,
            s: [0; 12],
        }
    }
}
