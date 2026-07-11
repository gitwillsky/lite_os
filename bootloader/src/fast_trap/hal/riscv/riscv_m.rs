use core::arch::asm;

macro_rules! exchange {
    () => {
        exchange!(sp)
    };

    ($reg:ident) => {
        concat!("csrrw ", stringify!($reg), ", mscratch, ", stringify!($reg))
    };
}

macro_rules! r#return {
    () => {
        "mret"
    };
}

use super::FlowContext;

pub(super) use {exchange, r#return};

impl FlowContext {
    /// 从上下文向硬件加载非调用规范约定的寄存器。
    #[inline]
    pub(crate) unsafe fn load_others(&self) {
        unsafe {
            asm!(
                "   mv         gp, {gp}
                mv         tp, {tp}
                csrw mscratch, {sp}
                csrw     mepc, {pc}
            ",
                gp = in(reg) self.gp,
                tp = in(reg) self.tp,
                sp = in(reg) self.sp,
                pc = in(reg) self.pc,
            );
        }
    }
}

/// 交换突发寄存器。
#[inline]
pub(crate) fn exchange_scratch(mut val: usize) -> usize {
    unsafe { asm!("csrrw {0}, mscratch, {0}", inlateout(reg) val) };
    val
}
