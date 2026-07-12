use spin::{Mutex, Once};

use self::mm::MapArea;
use crate::arch::dtb;

mod address;
mod config;
mod executable;
mod frame_allocator;
mod heap_allocator;
mod kernel_stack;
mod mm;
mod page_table;
mod shared_file;

pub(crate) use address::{PhysicalAddress, VirtualAddress};
pub(crate) use config::*;
pub(crate) use executable::{
    ExecutableImage, ExecutableParseError, ExecutableSource, parse_interpreter_elf, parse_main_elf,
};
pub(crate) use frame_allocator::{FrameTracker, alloc_contiguous, statistics as frame_statistics};
pub(crate) use kernel_stack::KernelStack;
pub(crate) use mm::{
    ElfLoadError, MapPermission, MemoryError, MemorySet, PageFaultAccess, PageFaultOutcome,
    UserAccessError,
};
pub(crate) use shared_file::{
    SharedFileError, SharedFileId, SharedFileMapping, SharedFrame, SharedMappingInvalidator,
    SharedPage, invalidate_shared_file, register_shared_mapping_owner,
};
// SAFETY: every symbol is defined by the fixed kernel linker script; callers use them only as
// section boundary addresses and never dereference them as Rust values.
unsafe extern "C" {
    fn stext();
    fn etext();

    fn srodata();
    fn erodata();

    fn sdata();
    fn edata();

    fn sbss();
    fn ebss();

    fn boot_stack_bottom();
    fn boot_stack_top();

    fn ekernel();
    pub(crate) fn strampoline();
    pub(crate) fn __signal_trampoline();
}

pub(crate) fn signal_trampoline_entry() -> usize {
    SIGNAL_TRAMPOLINE
        + (__signal_trampoline as *const () as usize - strampoline as *const () as usize)
}

// OWNER: memory module owns the canonical kernel address space after initialization.
pub(crate) static KERNEL_SPACE: Once<Mutex<MemorySet>> = Once::new();

/// @description 初始化构造动态 hart topology 所需的 kernel allocator。
///
/// @return 无返回值。
/// @errors allocator 重复初始化或内存布局损坏时 fail-stop。
pub(crate) fn init_allocator() {
    heap_allocator::init();
}

pub(crate) fn init() {
    let kernel_end_addr: PhysicalAddress = (ekernel as *const () as usize).into();
    let memory_end_addr: PhysicalAddress = dtb::board_info().mem.end.into();
    debug!("kernel_end_addr: {:#x}", kernel_end_addr.as_usize());
    debug!("memory_end_addr: {:#x}", memory_end_addr.as_usize());

    frame_allocator::init(kernel_end_addr, memory_end_addr);

    KERNEL_SPACE.call_once(|| Mutex::new(init_kernel_space(memory_end_addr)));
    KERNEL_SPACE.wait().lock().active();
    debug!("memory initialized");
}

fn init_kernel_space(memory_end_addr: PhysicalAddress) -> MemorySet {
    let mut memory_set = MemorySet::new();

    memory_set
        .map_trampoline()
        .expect("Failed to map kernel trampoline");

    // VirtIO MMIO 设备映射 - 使用 BoardInfo 获取动态地址范围
    let board_info = dtb::board_info();
    if !board_info.uart.is_empty() {
        debug!("[init_kernel_space] UART MMIO: {:#x?}", board_info.uart);
        memory_set
            .push(
                MapArea::new(
                    board_info.uart.start.into(),
                    board_info.uart.end.into(),
                    mm::MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            )
            .expect("Failed to map UART MMIO memory");
    }
    if board_info.virtio_count > 0 {
        let mut min_addr = usize::MAX;
        let mut max_addr = 0;

        for i in 0..board_info.virtio_count {
            if let Some(dev) = &board_info.virtio_devices[i] {
                min_addr = min_addr.min(dev.base_addr);
                max_addr = max_addr.max(dev.base_addr + dev.size);
            }
        }

        debug!(
            "[init_kernel_space] VirtIO MMIO: {:#x} - {:#x}",
            min_addr, max_addr
        );
        memory_set
            .push(
                MapArea::new(
                    min_addr.into(),
                    max_addr.into(),
                    mm::MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            )
            .expect("Failed to map VirtIO MMIO memory");
    }

    // RTC 设备映射
    if let Some(rtc_dev) = &board_info.rtc_device {
        debug!(
            "[init_kernel_space] RTC MMIO: {:#x} - {:#x}",
            rtc_dev.base_addr,
            rtc_dev.base_addr + rtc_dev.size
        );
        memory_set
            .push(
                MapArea::new(
                    rtc_dev.base_addr.into(),
                    (rtc_dev.base_addr + rtc_dev.size).into(),
                    mm::MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            )
            .expect("Failed to map RTC MMIO memory");
    }

    // PLIC 中断控制器映射
    if let Some(plic_dev) = &board_info.plic_device {
        debug!(
            "[init_kernel_space] PLIC MMIO: {:#x} - {:#x}",
            plic_dev.base_addr,
            plic_dev.base_addr + plic_dev.size
        );
        memory_set
            .push(
                MapArea::new(
                    plic_dev.base_addr.into(),
                    (plic_dev.base_addr + plic_dev.size).into(),
                    mm::MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            )
            .expect("Failed to map PLIC MMIO memory");
    }

    // kernel text section
    let stext_addr = stext as *const () as usize;
    let etext_addr = etext as *const () as usize;
    debug!(
        "[init_kernel_space] .text section: {:#x} - {:#x}",
        stext_addr, etext_addr
    );
    memory_set
        .push(
            MapArea::new(
                (stext as *const () as usize).into(),
                (etext as *const () as usize).into(),
                mm::MapType::Identical,
                MapPermission::R | MapPermission::X,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .text section");

    // kernel read only data
    let srodata_addr = srodata as *const () as usize;
    let erodata_addr = erodata as *const () as usize;
    debug!(
        "[init_kernel_space] .rodata section: {:#x} - {:#x}",
        srodata_addr, erodata_addr
    );
    memory_set
        .push(
            MapArea::new(
                (srodata as *const () as usize).into(),
                (erodata as *const () as usize).into(),
                mm::MapType::Identical,
                MapPermission::R,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .rodata section");

    // kernel data
    let sdata_addr = sdata as *const () as usize;
    let edata_addr = edata as *const () as usize;
    debug!(
        "[init_kernel_space] .data section: {:#x} - {:#x}",
        sdata_addr, edata_addr
    );
    memory_set
        .push(
            MapArea::new(
                (sdata as *const () as usize).into(),
                (edata as *const () as usize).into(),
                mm::MapType::Identical,
                MapPermission::R | MapPermission::W,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .data section");

    // kernel bss section
    let sbss_addr = sbss as *const () as usize;
    let ebss_addr = ebss as *const () as usize;
    debug!(
        "[init_kernel_space] .bss section: {:#x} - {:#x}",
        sbss_addr, ebss_addr
    );
    memory_set
        .push(
            MapArea::new(
                (sbss as *const () as usize).into(),
                (ebss as *const () as usize).into(),
                mm::MapType::Identical,
                MapPermission::R | MapPermission::W,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .bss section");

    // boot hart 独占的 early stack，底部保留一页 guard。
    // 这样当内核启动栈向下越界时会立即触发缺页，有助于定位随机返回地址被破坏的问题。
    let boot_stack_bottom_addr = boot_stack_bottom as *const () as usize;
    let boot_stack_top_addr = boot_stack_top as *const () as usize;
    let mapped_bottom = boot_stack_bottom_addr + PAGE_SIZE;
    debug!(
        "[init_kernel_space] boot stack: {:#x} - {:#x} (guard @ {:#x})",
        mapped_bottom, boot_stack_top_addr, boot_stack_bottom_addr
    );
    memory_set
        .push(
            MapArea::new(
                mapped_bottom.into(),
                boot_stack_top_addr.into(),
                mm::MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        )
        .expect("Failed to map kernel boot stack");

    // 后续可演进为受控的 physmap/kmap 方案，再逐步去除依赖。
    {
        let ekernel_addr = ekernel as *const () as usize;
        debug!(
            "[init_kernel_space] kernel physmap (RW, NX): {:#x} - {:#x}",
            ekernel_addr,
            memory_end_addr.as_usize()
        );
        memory_set
            .push(
                MapArea::new(
                    (ekernel as *const () as usize).into(),
                    memory_end_addr.as_usize().into(),
                    mm::MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            )
            .expect("Failed to map kernel phys memory area");
    }

    memory_set
}
