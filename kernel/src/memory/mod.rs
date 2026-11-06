//! 物理内存：buddy frame allocator、内核堆与物理/虚拟地址类型。
//!
//! 虚拟地址空间（页表、VMO、VMAR、kernel stack）由 `vm` 拥有；本领域只提供物理页与堆。

use crate::platform;

mod address;
mod config;
pub(crate) mod frame_allocator;
mod frame_metadata_layout;
mod heap_allocator;
pub(crate) use heap_allocator::heap_pages;

// OWNER: memory subsystem reserves one minimum buddy growth extent from single-frame consumers.
// Without this low watermark, a user fault can consume the final frame before the kernel records
// or tears down that mapping, turning recoverable process OOM into a global alloc-error panic.
const KERNEL_HEAP_GROWTH_PAGES: usize = 64;
// OWNER: frame allocator preserves this final reserve from ordinary kernel-heap growth.
// Without a distinct lower watermark, a fallible userspace-driven Vec/Arc allocation can consume
// the same last frames needed to tear down the faulting thread after an out-of-memory failure.
const KERNEL_PROGRESS_RESERVE_PAGES: usize = 16;

pub(crate) use crate::config::KERNEL_STACK_SIZE;
pub(crate) use address::{PhysicalAddress, PhysicalPageNumber, VirtualAddress, VirtualPageNumber};
pub(crate) use config::*;
pub(crate) use frame_allocator::{
    FrameAllocationClass, FrameTracker, alloc_contiguous, statistics as frame_statistics,
};

// SAFETY: every symbol is defined by the fixed kernel linker script; callers use them only as
// section boundary addresses and never dereference them as Rust values.
unsafe extern "C" {
    pub(crate) fn stext();
    pub(crate) fn etext();

    pub(crate) fn srodata();
    pub(crate) fn erodata();

    pub(crate) fn sdata();
    pub(crate) fn edata();

    pub(crate) fn sbss();
    pub(crate) fn ebss();

    pub(crate) fn boot_stack_bottom();
    pub(crate) fn boot_stack_top();

    pub(crate) fn ekernel();
    pub(crate) fn strampoline();
}

/// 初始化构造动态 logical CPU topology 所需的 kernel allocator。
///
/// # Errors
///
/// allocator 重复初始化或内存布局损坏时 fail-stop。
pub(crate) fn init_allocator() {
    heap_allocator::init();
}

/// 把 `[kernel end, allocator_end)` 的 RAM 交给 frame allocator，并允许堆从中增长。
///
/// # Parameters
///
/// - `allocator_end`: boot 装配排除崩溃记录等保留区后的可分配上界。
/// - `reserved`: 启动期必须保留到显式归还的物理区间（initrd 与 DTB）。
pub(crate) fn init(
    allocator_end: PhysicalAddress,
    reserved: [Option<core::ops::Range<PhysicalAddress>>; frame_allocator::BOOT_RESERVED_CAPACITY],
) {
    let kernel_end_addr: PhysicalAddress =
        crate::arch::mmu::virtual_to_physical(ekernel as *const () as usize)
            .expect("kernel end is outside architecture direct map")
            .into();
    debug!("kernel_end_addr: {:#x}", kernel_end_addr.as_usize());
    debug!("memory_end_addr: {:#x}", platform::physical_memory_end());
    frame_allocator::init(kernel_end_addr, allocator_end, reserved);
    heap_allocator::enable_frame_backed_growth();
    heap_allocator::init_cpu_caches();
}

/// boot CPU early stack 的可读范围（下界含、上界不含）。
pub(crate) fn boot_stack_range() -> (usize, usize) {
    (
        boot_stack_bottom as *const () as usize,
        boot_stack_top as *const () as usize,
    )
}
