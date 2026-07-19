use super::{mmu::KernelTrapToken, trap::UserTrapEntry};

/// 每任务 kernel stack 顶部由 AArch64 UserContext 与 trap metadata 独占的页数。
pub(crate) const KERNEL_STACK_CONTEXT_RESERVE: usize = super::mmu::PAGE_SIZE;
pub(super) const KERNEL_STACK_CONTEXT_OFFSET: usize = 16;

/// EL0 integer/system state shared by trap entry and user return assembly.
#[repr(C, align(16))]
#[derive(Debug, Clone)]
pub(crate) struct UserContext {
    pub(super) x: [usize; 31],
    pub(super) sp: usize,
    pub(super) pc: usize,
    pub(super) pstate: usize,
    pub(super) kernel_ttbr: u64,
    pub(super) kernel_sp: usize,
    pub(super) trap_handler: usize,
    pub(super) kernel_cpu_id: usize,
    pub(super) thread_pointer: usize,
    pub(super) _reserved: usize,
}

const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(offset_of!(UserContext, sp) == 248);
    assert!(offset_of!(UserContext, kernel_ttbr) == 272);
    assert!(size_of::<UserContext>() == 320);
    assert!(KERNEL_STACK_CONTEXT_OFFSET + size_of::<UserContext>() <= KERNEL_STACK_CONTEXT_RESERVE);
};

/// @description 从 kernel stack mapped top 投影 TTBR1 常驻的 UserContext 地址。
/// @param mapped_top 含保留页的 kernel stack mapping exclusive top。
/// @return 保留页起点后固定 16-byte offset 的唯一 UserContext 地址。
pub(crate) fn kernel_stack_user_context(mapped_top: usize) -> Option<usize> {
    let reserved = mapped_top.checked_sub(KERNEL_STACK_CONTEXT_RESERVE)?;
    Some(reserved + KERNEL_STACK_CONTEXT_OFFSET)
}

/// @description 判断 UserContext 是否由 AArch64 TTBR1 kernel-stack window 保活。
/// @param address context virtual address。
/// @return 地址位于 kernel stack window 时为 true。
pub(crate) fn is_kernel_stack_user_context(address: usize) -> bool {
    (super::mmu::KERNEL_STACK_REGION_START..super::mmu::KERNEL_STACK_REGION_TOP).contains(&address)
}

impl UserContext {
    /// Set the EL0 stack pointer.
    pub(crate) fn set_sp(&mut self, sp: usize) {
        self.sp = sp;
    }

    /// Construct the first EL0 entry context.
    pub(crate) fn app_init_context(
        entry: usize,
        sp: usize,
        _kernel_root: KernelTrapToken,
        kernel_sp: usize,
        trap_handler: UserTrapEntry,
    ) -> Self {
        let mut context = Self {
            x: [0; 31],
            sp,
            pc: entry,
            pstate: 0,
            kernel_ttbr: 0,
            kernel_sp,
            trap_handler: trap_handler.encoded(),
            kernel_cpu_id: 0,
            thread_pointer: 0,
            _reserved: 0,
        };
        context.set_sp(sp);
        context
    }

    /// FP/ASIMD is eagerly enabled on AArch64, so no lazy first-use transition exists.
    pub(crate) fn activate_floating_point(&mut self) -> bool {
        false
    }
}
