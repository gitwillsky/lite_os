#[repr(C)]
pub struct TrapContext {
    pub x: [usize; 32], // 保存通用寄存器 x0-x31
    pub sstatus: usize, // 保存状态寄存器 sstatus
    pub sepc: usize,    // 保存异常程序计数器 sepc
}

impl Default for TrapContext {
    fn default() -> Self {
        Self {
            x: [0; 32],
            sstatus: 0,
            sepc: 0,
        }
    }
}
