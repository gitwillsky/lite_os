use core::{cell::UnsafeCell, mem::forget, ptr::NonNull};

use crate::{
    Supervisor, constants, fast_handler,
    fast_trap::{self, FlowContext, reuse_stack_for_trap},
    hart::hart_id,
    hsm_cell,
};

#[repr(transparent)]
struct StackCell(UnsafeCell<Stack>);

impl StackCell {
    const fn new() -> Self {
        Self(UnsafeCell::new(Stack::ZERO))
    }
}

// SAFETY: 每个 StackCell 只由 ID 等于其数组索引的 hart 可变访问；远端 HSM 状态已拆到独立原子对象。
unsafe impl Sync for StackCell {}

/// 每 hart 独占的 M-mode trap stack，不参与 cold-boot BSS 清零。
#[unsafe(link_section = ".bss.uninit")]
// OWNER: trap-stack module owns the firmware stack backing every representable hart.
static ROOT_STACK: [StackCell; constants::HART_MASK_BITS] =
    [const { StackCell::new() }; constants::HART_MASK_BITS];

/// HSM 使用原子状态和受状态保护的 payload，可安全地由 local/remote hart 共享。
// OWNER: trap-stack module owns the HSM state cell for every representable hart.
static HSM_CELLS: [hsm_cell::HsmCell<Supervisor>; constants::HART_MASK_BITS] =
    [const { hsm_cell::HsmCell::new() }; constants::HART_MASK_BITS];

/// @description 在访问任何固定数组前验证 `mhartid`，然后定位当前 hart 的 trap stack。
///
/// @return 正常路径返回调用者；非法 hart 永久 fail-stop。
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn locate() {
    core::arch::naked_asm!(
        "   csrr t1, mhartid
            li   t2, {max_harts}
            bgeu t1, t2, 2f
            la   sp, {stack}
            li   t0, {per_hart_stack_size}
            addi t1, t1,  1
         1: add  sp, sp, t0
            addi t1, t1, -1
            bnez t1, 1b
            call t1, {move_stack}
            ret
         2:
            csrci mstatus, 8
            csrw mie, zero
         3:
            wfi
            j 3b
        ",
        per_hart_stack_size = const constants::STACK_SIZE_PER_HART,
        max_harts = const constants::HART_MASK_BITS,
        stack = sym ROOT_STACK,
        move_stack = sym reuse_stack_for_trap,
    )
}

fn local_stack() -> &'static mut Stack {
    let hart = hart_id();
    // SAFETY: 入口已验证索引，且该 cell 只由 hart `hart` 访问。
    unsafe { &mut *ROOT_STACK[hart].0.get() }
}

/// @description 为当前 hart 安装 fast-trap stack。
///
/// @return 无返回值。
pub(crate) fn prepare_for_trap() {
    local_stack().load_as_stack();
}

/// @description 获取当前 hart 的 local HSM handle。
///
/// @return 仅当前 hart 可调用的状态转换 handle。
pub(crate) fn local_hsm() -> hsm_cell::LocalHsmCell<'static, Supervisor> {
    // SAFETY: 数组索引来自当前 hart，handle 不会传给远端。
    unsafe { HSM_CELLS[hart_id()].local() }
}

/// @description 获取当前 hart 的 remote HSM handle，用于 cold-boot start 发布。
///
/// @return 当前 hart 对应的共享 remote handle。
pub(crate) fn local_remote_hsm() -> hsm_cell::RemoteHsmCell<'static, Supervisor> {
    HSM_CELLS[hart_id()].remote()
}

/// @description 获取指定合法 hart 的 remote HSM handle。
///
/// @param hart_id 目标 hart ID。
/// @return 越界返回 `None`，合法索引返回共享 handle。
pub(crate) fn remote_hsm(hart_id: usize) -> Option<hsm_cell::RemoteHsmCell<'static, Supervisor>> {
    HSM_CELLS.get(hart_id).map(hsm_cell::HsmCell::remote)
}

/// 每个 hart 独占的对齐 trap stack。
#[repr(C, align(128))]
struct Stack([u8; constants::STACK_SIZE_PER_HART]);

impl Stack {
    const ZERO: Self = Self([0; constants::STACK_SIZE_PER_HART]);

    fn context_ptr(&mut self) -> NonNull<FlowContext> {
        // SAFETY: Stack 至少 128-byte 对齐且容量大于 FlowContext；底部区域由 fast-trap 独占。
        unsafe { NonNull::new_unchecked(self.0.as_mut_ptr().cast()) }
    }

    fn load_as_stack(&'static mut self) {
        let context_ptr = self.context_ptr();
        let range = self.0.as_ptr_range();
        forget(
            fast_trap::FreeTrapStack::new(
                range.start as usize..range.end as usize,
                |_| {},
                context_ptr,
                fast_handler,
            )
            .expect("per-hart trap stack is too small")
            .load(),
        );
    }
}
