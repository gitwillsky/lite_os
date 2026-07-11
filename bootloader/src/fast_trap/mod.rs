mod fast;
mod hal;

pub(crate) use fast::*;
pub(crate) use hal::*;

use core::{alloc::Layout, marker::PhantomPinned, mem::forget, ops::Range, ptr::NonNull};

/// 游离的陷入栈。
pub(crate) struct FreeTrapStack(NonNull<TrapHandler>);

/// 已加载的陷入栈。
pub(crate) struct LoadedTrapStack(usize);

/// 构造陷入栈失败。
#[derive(Debug)]
pub(crate) struct IllegalStack;

impl FreeTrapStack {
    /// 在内存块上构造游离的陷入栈。
    pub(crate) fn new(
        range: Range<usize>,
        drop: fn(Range<usize>),

        context_ptr: NonNull<hal::FlowContext>,
        fast_handler: FastHandler,
    ) -> Result<Self, IllegalStack> {
        const LAYOUT: Layout = Layout::new::<TrapHandler>();
        let bottom = range.start;
        let top = range.end;
        let ptr = (top - LAYOUT.size()) & !(LAYOUT.align() - 1);
        if ptr >= bottom {
            // SAFETY: aligned placement calculation proves a complete TrapHandler fits inside
            // caller-owned stack memory, which remains allocated for the returned owner.
            let handler = unsafe { &mut *(ptr as *mut TrapHandler) };
            handler.range = range;
            handler.drop = drop;
            handler.context = context_ptr;
            handler.fast_handler = fast_handler;
            // SAFETY: handler was just created from a non-null in-range address.
            Ok(Self(unsafe { NonNull::new_unchecked(handler) }))
        } else {
            Err(IllegalStack)
        }
    }

    /// 将这个陷入栈加载为预备陷入栈。
    #[inline]
    pub(crate) fn load(self) -> LoadedTrapStack {
        let scratch = hal::exchange_scratch(self.0.as_ptr() as _);
        forget(self);
        LoadedTrapStack(scratch)
    }
}

impl Drop for FreeTrapStack {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: FreeTrapStack uniquely owns this initialized handler and calls its paired
        // allocator callback exactly once with the original range.
        unsafe {
            let handler = self.0.as_ref();
            (handler.drop)(handler.range.clone());
        }
    }
}

impl LoadedTrapStack {
    /// 卸载但不消费所有权。
    ///
    /// # Safety
    ///
    /// 间接复制了所有权。用于 `Drop`。
    #[inline]
    // SAFETY: caller must ensure the returned FreeTrapStack is consumed exactly once and the
    // currently loaded mscratch value still names this TrapHandler.
    unsafe fn unload_unchecked(&self) -> FreeTrapStack {
        let ptr = hal::exchange_scratch(self.0) as *mut TrapHandler;
        // SAFETY: load stored this non-null TrapHandler address in mscratch; exchange returns it.
        let handler = unsafe { NonNull::new_unchecked(ptr) };
        FreeTrapStack(handler)
    }
}

impl Drop for LoadedTrapStack {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: Drop has unique access and invokes unload once, immediately consuming the
        // reconstructed owner so the backing stack is released exactly once.
        drop(unsafe { self.unload_unchecked() })
    }
}

/// 陷入处理器上下文。
#[repr(C)]
struct TrapHandler {
    /// 指向一个陷入上下文的指针。
    ///
    /// `TrapHandler` 与 trap stack 位于同一 owned range；`context` 指向该 range 内由汇编
    /// 保存的 `FlowContext`，只在 `LoadedTrapStack` 拥有该 range 期间访问。
    /// - 发生陷入时，将寄存器保存到此对象。
    /// - 离开陷入处理时，按此对象的内容设置寄存器。
    context: NonNull<hal::FlowContext>,
    /// 快速路径函数。
    ///
    /// 必须在初始化陷入时设置好。
    fast_handler: FastHandler,
    /// 可在汇编使用的临时存储。
    ///
    /// - 在快速路径开始时暂存 a0。
    /// - 在快速路径结束时保存完整路径函数。
    scratch: usize,

    range: Range<usize>,
    drop: fn(Range<usize>),

    /// 禁止移动标记。
    ///
    /// `TrapHandler` 是放在其内部定义的 `block` 块里的，这是一种自引用结构，不能移动。
    pinned: PhantomPinned,
}
