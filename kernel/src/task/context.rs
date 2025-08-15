use crate::trap::trap_return;

/// Task context structure containing some registers
#[repr(C)]
#[derive(Debug)]
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
    
    /// 设置返回地址
    pub fn set_ra(&mut self, ra: usize) {
        self.ra = ra;
    }
    
    /// 设置栈指针
    pub fn set_sp(&mut self, sp: usize) {
        self.kernel_sp = sp;
    }
}
