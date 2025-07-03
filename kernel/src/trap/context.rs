use riscv::register::sstatus::{self, SPP, Sstatus};

#[repr(C)]
pub struct TrapContext {
    pub x: [usize; 32],   // 保存通用寄存器 x0-x31
    pub sstatus: Sstatus, // 保存状态寄存器 sstatus
    pub sepc: usize,      // 保存异常程序计数器 sepc

    pub kernel_satp: usize,  // 内核地址空间页表起始地址
    pub kernel_sp: usize,    // 当前应用在内核地址空间中的内核栈栈顶的虚拟地址
    pub trap_handler: usize, // 内核 trap handler 入口点的虚拟地址
}

impl TrapContext {
    pub fn set_sp(&mut self, sp: usize) {
        self.x[2] = sp;
    }

    pub fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        let mut sstatus = sstatus::read(); // CSR status
        println!("[app_init_context] Original sstatus: {:?}", sstatus);
        sstatus.set_spp(SPP::User);
        println!("[app_init_context] After setting SPP to User: {:?}", sstatus);

        let mut cx = Self {
            x: [0; 32],
            sstatus,
            sepc: entry,
            kernel_satp,
            kernel_sp,
            trap_handler,
        };

        cx.set_sp(sp);
        println!("[app_init_context] Final context: entry={:#x}, sp={:#x}", entry, sp);
        cx
    }
}
