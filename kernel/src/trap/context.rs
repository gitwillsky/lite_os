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

    pub fn set_tp(&mut self, tp: usize) {
        self.x[4] = tp;
    }

    pub fn set_gp(&mut self, gp: usize) {
        self.x[3] = gp; // gp is x3 register
    }

    pub fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        let mut sstatus = sstatus::read(); // CSR status
        sstatus.set_spp(SPP::User);
        // 启用用户态浮点支持
        sstatus.set_fs(sstatus::FS::Dirty);

        let mut cx = Self {
            x: [0; 32],
            sstatus,
            sepc: entry,
            kernel_satp,
            kernel_sp,
            trap_handler,
        };

        cx.set_sp(sp);
        cx
    }

    pub fn app_init_context_with_tp(
        entry: usize,
        sp: usize,
        tp: usize,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        let mut cx = Self::app_init_context(entry, sp, kernel_satp, kernel_sp, trap_handler);
        cx.set_tp(tp);
        cx
    }

    pub fn app_init_context_with_tp_gp(
        entry: usize,
        sp: usize,
        tp: usize,
        gp: Option<usize>,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        let mut cx = Self::app_init_context(entry, sp, kernel_satp, kernel_sp, trap_handler);
        cx.set_tp(tp);
        if let Some(gp_val) = gp {
            cx.set_gp(gp_val);
        }
        cx
    }
}
