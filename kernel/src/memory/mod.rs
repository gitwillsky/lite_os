use address::PhysicalAddress;
use spin::{Mutex, Once};

use crate::{
    board,
    memory::mm::{MapArea, MapPermission, MemorySet},
};

pub mod address;
mod config;
pub mod dynamic_linker;
pub mod frame_allocator;
pub mod heap_allocator;
pub mod kernel_stack;
pub mod mm;
pub mod page_table;
pub mod slab_allocator;

pub use config::*;
unsafe extern "C" {
    fn skernel();

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
    pub fn strampoline();
}

pub static KERNEL_SPACE: Once<Mutex<MemorySet>> = Once::new();

pub fn init() {
    let kernel_end_addr: PhysicalAddress = (ekernel as usize).into();
    let memory_end_addr: PhysicalAddress = board::board_info().mem.end.into();
    debug!("kernel_end_addr: {:#x}", kernel_end_addr.as_usize());
    debug!("memory_end_addr: {:#x}", memory_end_addr.as_usize());

    heap_allocator::init();
    frame_allocator::init(kernel_end_addr, memory_end_addr);

    // Initialize SLAB allocator after frame allocator is ready
    heap_allocator::init_slab();

    KERNEL_SPACE.call_once(|| Mutex::new(init_kernel_space(memory_end_addr)));
    KERNEL_SPACE.wait().lock().active();
    debug!("memory initialized");
}

fn init_kernel_space(memory_end_addr: PhysicalAddress) -> MemorySet {
    let mut memory_set = MemorySet::new();

    memory_set.map_trampoline();

    // VirtIO MMIO 设备映射 - 使用 BoardInfo 获取动态地址范围
    let board_info = crate::board::board_info();
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
    let stext_addr = stext as usize;
    let etext_addr = etext as usize;
    debug!(
        "[init_kernel_space] .text section: {:#x} - {:#x}",
        stext_addr, etext_addr
    );
    memory_set
        .push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                mm::MapType::Identical,
                MapPermission::R | MapPermission::X,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .text section");

    // kernel read only data
    let srodata_addr = srodata as usize;
    let erodata_addr = erodata as usize;
    debug!(
        "[init_kernel_space] .rodata section: {:#x} - {:#x}",
        srodata_addr, erodata_addr
    );
    memory_set
        .push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                mm::MapType::Identical,
                MapPermission::R,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .rodata section");

    // kernel data
    let sdata_addr = sdata as usize;
    let edata_addr = edata as usize;
    debug!(
        "[init_kernel_space] .data section: {:#x} - {:#x}",
        sdata_addr, edata_addr
    );
    memory_set
        .push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                mm::MapType::Identical,
                MapPermission::R | MapPermission::W,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .data section");

    // kernel bss section
    let sbss_addr = sbss as usize;
    let ebss_addr = ebss as usize;
    debug!(
        "[init_kernel_space] .bss section: {:#x} - {:#x}",
        sbss_addr, ebss_addr
    );
    memory_set
        .push(
            MapArea::new(
                (sbss as usize).into(),
                (ebss as usize).into(),
                mm::MapType::Identical,
                MapPermission::R | MapPermission::W,
            )
            .set_global(true),
            None,
        )
        .expect("Failed to map kernel .bss section");

    // kernel boot stacks with per-hart guard pages at the bottom of each stack
    // 这样当内核启动栈向下越界时会立即触发缺页，有助于定位随机返回地址被破坏的问题
    let boot_stack_bottom_addr = boot_stack_bottom as usize;
    let boot_stack_top_addr = boot_stack_top as usize;
    debug!(
        "[init_kernel_space] boot stack region: {:#x} - {:#x}",
        boot_stack_bottom_addr, boot_stack_top_addr
    );
    {
        use crate::arch::hart::MAX_CORES;
        for hart in 0..MAX_CORES {
            let stack_top = boot_stack_top_addr - hart * KERNEL_STACK_SIZE;
            let stack_bottom = stack_top - KERNEL_STACK_SIZE;
            let mapped_bottom = stack_bottom + PAGE_SIZE; // 保留 1 页守护页
            debug!(
                "[init_kernel_space] boot stack[{}]: {:#x} - {:#x} (guard @ {:#x})",
                hart, mapped_bottom, stack_top, stack_bottom
            );
            memory_set
                .push(
                    MapArea::new(
                        mapped_bottom.into(),
                        stack_top.into(),
                        mm::MapType::Identical,
                        MapPermission::R | MapPermission::W,
                    ),
                    None,
                )
                .expect("Failed to map per-hart kernel boot stack");
        }
    }

    // 后续可演进为受控的 physmap/kmap 方案，再逐步去除依赖。
    {
        let ekernel_addr = ekernel as usize;
        debug!(
            "[init_kernel_space] kernel physmap (RW, NX): {:#x} - {:#x}",
            ekernel_addr,
            memory_end_addr.as_usize()
        );
        memory_set
            .push(
                MapArea::new(
                    (ekernel as usize).into(),
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
