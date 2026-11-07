//! restricted 状态绑定：固定状态 VMO 的页，进出 restricted mode 经 direct map 拷贝寄存器。

use alloc::sync::Arc;

use native_abi::{RestrictedState, Status};

use crate::object::vm_status;
use crate::vm::Vmo;

/// 绑定的 restricted 状态：VMO 页在绑定时固定，进入/退出经内核直接映射拷贝，不再每次取 VMO 树锁、查页。
///
/// 固定的页不会被撤销或替换（用户对该 VMO 的 decommit、快照都会被拒绝），所以物理页号在绑定期间有效；
/// 缺少固定时，用户 decommit 之后内核会写入已释放的页框。
pub(crate) struct RestrictedStateBinding {
    // OWNER: 保活状态 VMO；`pin` 的解除固定发生在它自己的 Drop 内，必须先于 VMO 释放，字段顺序保证这一点。
    pin: crate::vm::PinnedRange,
    _vmo: Arc<Vmo>,
}

impl RestrictedStateBinding {
    /// 固定 `vmo` 的全部状态页并建立绑定。
    ///
    /// # Errors
    /// 固定失败（分配失败）。
    pub(super) fn new(vmo: Arc<Vmo>) -> Result<Self, Status> {
        let size = core::mem::size_of::<RestrictedState>().next_multiple_of(native_abi::PAGE_SIZE);
        let pin = vmo.pin(0, size as u64).map_err(vm_status)?;
        Ok(Self { pin, _vmo: vmo })
    }

    fn copy(&self, mut visit: impl FnMut(*mut u8, core::ops::Range<usize>)) {
        let total = core::mem::size_of::<RestrictedState>();
        for (index, page) in self.pin.pages().iter().enumerate() {
            let start = index * native_abi::PAGE_SIZE;
            if start >= total {
                break;
            }
            visit(
                page.as_page_mut_ptr(),
                start..(start + native_abi::PAGE_SIZE).min(total),
            );
        }
    }

    /// 读出状态。
    pub(crate) fn load(&self) -> RestrictedState {
        let mut state = RestrictedState::default();
        let bytes = (&raw mut state).cast::<u8>();
        self.copy(|page, range| {
            // SAFETY: `page` 是固定页的 direct-map 指针，`range` 在状态结构内；RestrictedState 是 Plain，
            // 任意字节都是合法值。
            unsafe { core::ptr::copy_nonoverlapping(page, bytes.add(range.start), range.len()) };
        });
        state
    }

    /// 写入状态。
    pub(crate) fn store(&self, state: &RestrictedState) {
        let bytes = (state as *const RestrictedState).cast::<u8>();
        self.copy(|page, range| {
            // SAFETY: 同 `load`；写入只改动固定页内的状态字节。
            unsafe { core::ptr::copy_nonoverlapping(bytes.add(range.start), page, range.len()) };
        });
    }
}
