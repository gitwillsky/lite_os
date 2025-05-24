use core::{mem::forget, ptr::NonNull};

use crate::{
    Supervisor, constants, fast_handler,
    fast_trap::{self, FlowContext, reuse_stack_for_trap},
    hart::hart_id,
    hsm_cell,
};

/// 栈空间。
#[unsafe(link_section = ".bss.uninit")]
static mut ROOT_STACK: [Stack; constants::MAX_HART_NUM] = [Stack::ZERO; constants::MAX_HART_NUM];

/// 定位每个 hart 的栈。
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn locate() {
    core::arch::naked_asm!(
        "   la   sp, {stack}
            li   t0, {per_hart_stack_size}
            csrr t1, mhartid
            addi t1, t1,  1
         1: add  sp, sp, t0
            addi t1, t1, -1
            bnez t1, 1b
            call t1, {move_stack}
            ret
        ",
        per_hart_stack_size = const constants::STACK_SIZE_PER_HART,
        stack               =   sym ROOT_STACK,
        move_stack          =   sym reuse_stack_for_trap,
    )
}

/// 预备陷入栈。
pub(crate) fn prepare_for_trap() {
    unsafe {
        (*core::ptr::addr_of_mut!(ROOT_STACK))
            .get_unchecked_mut(hart_id())
            .load_as_stack()
    };
}

/// 获取此 hart 的 local hsm 对象。
pub(crate) fn local_hsm() -> hsm_cell::LocalHsmCell<'static, Supervisor> {
    unsafe {
        (*core::ptr::addr_of_mut!(ROOT_STACK))
            .get_unchecked_mut(hart_id())
            .hart_context()
            .hsm
            .local()
    }
}

/// 获取此 hart 的 remote hsm 对象。
pub(crate) fn local_remote_hsm() -> hsm_cell::RemoteHsmCell<'static, Supervisor> {
    unsafe {
        (*core::ptr::addr_of_mut!(ROOT_STACK))
            .get_unchecked_mut(hart_id())
            .hart_context()
            .hsm
            .remote()
    }
}

/// 获取任意 hart 的 remote hsm 对象。
pub(crate) fn remote_hsm(hart_id: usize) -> Option<hsm_cell::RemoteHsmCell<'static, Supervisor>> {
    unsafe {
        (*core::ptr::addr_of_mut!(ROOT_STACK))
            .get_mut(hart_id)
            .map(|x| x.hart_context().hsm.remote())
    }
}

/// 类型化栈。
///
/// 每个硬件线程拥有一个满足这样条件的内存块。
/// 这个内存块的底部放着硬件线程状态 [`HartContext`]，顶部用于陷入处理，中间是这个硬件线程的栈空间。
/// 不需要 M 态线程，每个硬件线程只有这一个栈。
#[repr(C, align(128))]
struct Stack([u8; constants::STACK_SIZE_PER_HART]);

impl Stack {
    /// 零初始化以避免加载。
    const ZERO: Self = Self([0; constants::STACK_SIZE_PER_HART]);

    /// 从栈上取出硬件线程状态。
    #[inline]
    fn hart_context(&mut self) -> &mut HartContext {
        unsafe { &mut *self.0.as_mut_ptr().cast() }
    }

    fn load_as_stack(&'static mut self) {
        let hart = self.hart_context();
        let context_ptr = hart.context_ptr();
        hart.init();
        let range = self.0.as_ptr_range();
        forget(
            fast_trap::FreeTrapStack::new(
                range.start as usize..range.end as usize,
                |_| {},
                context_ptr,
                fast_handler,
            )
            .unwrap()
            .load(),
        );
    }
}

/// 硬件线程上下文。
struct HartContext {
    /// 陷入上下文。
    trap: FlowContext,
    hsm: hsm_cell::HsmCell<Supervisor>,
}

impl HartContext {
    #[inline]
    fn init(&mut self) {
        self.hsm = hsm_cell::HsmCell::new();
    }

    #[inline]
    fn context_ptr(&mut self) -> NonNull<fast_trap::FlowContext> {
        unsafe { NonNull::new_unchecked(&mut self.trap) }
    }
}
