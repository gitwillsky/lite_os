use super::mmu::AddressSpaceToken;
use super::trap::UserTrapEntry;
use riscv::register::sstatus::{self, SPP, Sstatus};

/// @description U-mode 与 S-mode trap 路径之间共享的完整用户执行上下文。
#[repr(C)]
#[derive(Clone)]
pub(crate) struct UserContext {
    /// 用户通用寄存器 x0..x31，包括 psABI 的 gp(x3) 与 tp(x4)。
    pub(super) x: [usize; 32],
    /// trap 发生时的 supervisor status。
    pub(super) sstatus: Sstatus,
    /// trap 返回的用户程序计数器。
    pub(super) sepc: usize,
    /// trampoline 进入内核时切换的 kernel satp。
    pub(super) kernel_satp: usize,
    /// 当前任务的内核栈顶。
    pub(super) kernel_sp: usize,
    /// S-mode Rust trap handler 入口。
    pub(super) trap_handler: usize,
    /// 当前执行该任务的 hart ID，用于在保存用户 tp 后恢复 kernel tp。
    pub(super) kernel_cpu_id: usize,
    /// kernel psABI global pointer，用于在保存用户 gp 后恢复 kernel gp。
    pub(super) kernel_gp: usize,
    /// 用户浮点寄存器 f0..f31 的原始 64-bit 内容。
    pub(super) f: [u64; 32],
    /// 用户 floating-point control/status register。
    pub(super) fcsr: usize,
}

const _: () = {
    use core::mem::{offset_of, size_of};
    const WORD: usize = size_of::<usize>();
    assert!(offset_of!(UserContext, sstatus) == 32 * WORD);
    assert!(offset_of!(UserContext, kernel_satp) == 34 * WORD);
    assert!(offset_of!(UserContext, kernel_cpu_id) == 37 * WORD);
    assert!(offset_of!(UserContext, kernel_gp) == 38 * WORD);
    assert!(offset_of!(UserContext, f) == 39 * WORD);
    assert!(offset_of!(UserContext, fcsr) == 71 * WORD);
    assert!(size_of::<UserContext>() == 72 * WORD);
};

impl UserContext {
    /// @description 设置用户栈指针。
    ///
    /// @param sp 满足用户 ABI 对齐要求的栈顶。
    /// @return 无返回值。
    pub(crate) fn set_sp(&mut self, sp: usize) {
        self.x[2] = sp;
    }

    /// @description 构造首次进入用户程序所需的 trap context。
    ///
    /// @param entry ELF 入口虚拟地址。
    /// @param sp 用户初始栈指针。
    /// @param kernel_satp kernel 页表 token。
    /// @param kernel_sp 当前任务内核栈顶。
    /// @param trap_handler S-mode trap handler 地址。
    /// @return 通用寄存器、浮点寄存器和 fcsr 均为零的初始上下文。
    pub(crate) fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: AddressSpaceToken,
        kernel_sp: usize,
        trap_handler: UserTrapEntry,
    ) -> Self {
        let mut sstatus = sstatus::read(); // CSR status
        sstatus.set_spp(SPP::User);
        // SIE 必须在 S 模式恢复阶段保持关闭，否则切换到用户页表后会在内核栈失去映射时被中断。
        sstatus.set_sie(false);
        // SPIE 控制 sret 后的中断状态，确保进入用户态后可以正常响应中断。
        sstatus.set_spie(true);
        sstatus.set_sum(false);
        sstatus.set_mxr(false);
        // 当前采用 eager FP save/restore，因此用户上下文始终以 Dirty 状态进入。
        sstatus.set_fs(sstatus::FS::Dirty);

        let mut cx = Self {
            x: [0; 32],
            sstatus,
            sepc: entry,
            kernel_satp: kernel_satp.encoded(),
            kernel_sp,
            trap_handler: trap_handler.encoded(),
            kernel_cpu_id: 0,
            kernel_gp: 0,
            f: [0; 32],
            fcsr: 0,
        };

        cx.set_sp(sp);
        cx
    }
}
