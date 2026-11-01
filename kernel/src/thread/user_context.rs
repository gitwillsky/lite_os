//! 线程用户态寄存器上下文的唯一 owner。
//!
//! 上下文存放在架构选定的位置：AArch64 在线程内核栈的保留顶页，RISC-V 在所属地址空间中
//! 为该线程分配的 supervisor-only 页（trampoline 在用户页表下访问它）。

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// trampoline 可见地址与 Rust 访问指针的唯一绑定。
pub(super) struct ContextSlot<T> {
    address: usize,
    // OWNER: retire 前始终指向唯一 live `T`；retire 后为 null，任何访问 fail-stop。
    pointer: AtomicPtr<T>,
    // OWNER: 把“同一线程不会在两个 CPU 上同时运行”变成可执行检查，保证 `&mut T` 永不跨 CPU 别名。
    claimed: AtomicBool,
}

// SAFETY: 访问只经 `with`，claimed flag 保证同一时刻至多一个 `&mut T`。
unsafe impl<T: Send> Sync for ContextSlot<T> {}
// SAFETY: slot 只保存指向由内核栈或地址空间保活的 `T` 的指针，转移所有权不复制 `T`。
unsafe impl<T: Send> Send for ContextSlot<T> {}

impl<T> ContextSlot<T> {
    /// 绑定上下文存储。
    ///
    /// # Safety
    ///
    /// `pointer` 必须对齐、可写，并在 [`Self::retire`] 前始终由调用者保活且不被其他代码访问。
    pub(super) unsafe fn bind(address: usize, pointer: NonNull<T>) -> Self {
        Self {
            address,
            pointer: AtomicPtr::new(pointer.as_ptr()),
            claimed: AtomicBool::new(false),
        }
    }

    /// trampoline 使用的上下文地址。
    pub(super) fn address(&self) -> usize {
        self.address
    }

    /// 在唯一 transaction 内访问上下文。
    ///
    /// # Panics
    ///
    /// 并发访问或已退休时 fail-stop。
    pub(super) fn with<R>(&self, operation: impl FnOnce(&mut T) -> R) -> R {
        assert!(
            !self.claimed.swap(true, Ordering::Acquire),
            "concurrent user-context transaction"
        );
        let pointer = NonNull::new(self.pointer.load(Ordering::Acquire))
            .unwrap_or_else(|| panic!("retired user context accessed"));
        // SAFETY: bind 保证指针有效；claimed 保证这是唯一的 `&mut T`。
        let result = operation(unsafe { &mut *pointer.as_ptr() });
        self.claimed.store(false, Ordering::Release);
        result
    }

    /// 退休绑定；之后的访问 fail-stop。必须在支撑存储释放前调用。
    pub(super) fn retire(&self) {
        assert!(
            !self
                .pointer
                .swap(core::ptr::null_mut(), Ordering::AcqRel)
                .is_null(),
            "user context retired twice"
        );
    }

    /// 是否仍绑定。
    pub(super) fn is_live(&self) -> bool {
        !self.pointer.load(Ordering::Acquire).is_null()
    }
}
