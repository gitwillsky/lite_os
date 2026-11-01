//! 内核地址空间：内核映像、direct map、平台 MMIO、trampoline 与 kernel stack。
//!
//! # 锁与 fence
//!
//! deferred safe point 的等待者会关中断自旋，无法应答 TLB shootdown IPI；因此持有
//! [`KERNEL_SPACE`] 时不得等待远端 fence。撤销映射先在锁内摘除 PTE 并取得
//! [`KernelRetirement`]，释放锁后再 `synchronize`，fence 完成前 frame 一直由 retirement 保活。

use alloc::vec::Vec;
use core::ops::Range;

use spin::{Mutex, Once};

use super::page_table::{PagePermissions, PageTable};
use super::translation::TranslationCommit;
use crate::arch::mmu::AddressSpaceKind;
use crate::memory::{
    self, FrameTracker, PAGE_SIZE, PhysicalAddress, VirtualAddress, VirtualPageNumber,
};
use fallible_tree::FallibleMap;

/// 由 frame 支撑的内核区间（kernel stack）。
struct FramedRegion {
    end: VirtualPageNumber,
    frames: Vec<FrameTracker>,
}

/// 内核页表与 frame 支撑区间的唯一 owner。
pub(crate) struct KernelSpace {
    page_table: PageTable,
    // OWNER: 只记录 frame 支撑的区间；内核映像、direct map 与 MMIO 是启动期固定映射，不需要退休。
    framed: FallibleMap<VirtualPageNumber, FramedRegion>,
}

/// 已摘除 PTE、等待 fence 后才能释放的内核区间。
#[must_use = "kernel unmapping must synchronize before frames are released"]
pub(crate) struct KernelRetirement {
    commit: TranslationCommit,
    _frames: Vec<FrameTracker>,
}

impl KernelRetirement {
    /// 完成全部 CPU 的 fence 后释放 frame。
    ///
    /// # Panics
    ///
    /// fence 失败时 fail-stop：frame 可能仍被其他 CPU 的旧翻译访问，不能复用。
    pub(crate) fn synchronize(mut self) {
        self.commit
            .synchronize()
            .unwrap_or_else(|error| panic!("kernel translation fence failed: {error}"));
    }
}

// OWNER: vm 领域在初始化后拥有唯一内核地址空间；锁内禁止等待远端 fence，见模块文档。
pub(crate) static KERNEL_SPACE: Once<Mutex<KernelSpace>> = Once::new();

impl KernelSpace {
    fn map_kernel_range(&mut self, range: Range<usize>, permissions: PagePermissions) {
        let start = VirtualAddress::from(range.start).floor();
        let end = VirtualAddress::from(range.end).ceil();
        if end <= start {
            return;
        }
        let physical =
            crate::arch::mmu::virtual_to_physical(usize::from(VirtualAddress::from(start)))
                .expect("kernel range is outside the architecture direct map");
        let mut commit = TranslationCommit::new();
        self.page_table
            .map_contiguous_range(
                start,
                PhysicalAddress::from(physical).floor(),
                end.as_usize() - start.as_usize(),
                permissions | PagePermissions::GLOBAL,
                &mut commit,
            )
            .expect("boot kernel mapping failed");
        commit.finish_unpublished();
    }

    /// 映射 frame 支撑的内核区间。
    ///
    /// # Errors
    ///
    /// frame、页表页或索引节点分配失败时返回 `Err(())`，已映射的部分全部回滚。
    pub(crate) fn insert_framed(
        &mut self,
        range: Range<usize>,
        permissions: PagePermissions,
    ) -> Result<(), ()> {
        let start = VirtualAddress::from(range.start).floor();
        let end = VirtualAddress::from(range.end).ceil();
        let pages = end.as_usize() - start.as_usize();
        let mut frames = Vec::new();
        frames.try_reserve_exact(pages).map_err(|_| ())?;
        for _ in 0..pages {
            frames.push(memory::frame_allocator::alloc().ok_or(())?);
        }
        let entry =
            FallibleMap::try_prepare(start, FramedRegion { end, frames }).map_err(|_| ())?;
        let mut commit = TranslationCommit::new();
        for (index, frame) in entry.value().frames.iter().enumerate() {
            let page = VirtualPageNumber::from_vpn(start.as_usize() + index);
            if self
                .page_table
                .map(
                    page,
                    frame.ppn,
                    permissions | PagePermissions::GLOBAL,
                    &mut commit,
                )
                .is_err()
            {
                for mapped in 0..index {
                    let page = VirtualPageNumber::from_vpn(start.as_usize() + mapped);
                    self.page_table
                        .unmap(page, &mut commit)
                        .expect("rollback of a freshly mapped kernel page failed");
                }
                // 回滚的页从未返回给调用者，任何 CPU 都不可能访问过它们；不需要 fence，
                // 也避免在持锁时等待远端 CPU（见模块文档）。
                commit.finish_unpublished();
                return Err(());
            }
        }
        // invalid → valid 只需本地 fence；新区间尚未发布给任何其他 CPU 使用。
        commit
            .synchronize()
            .unwrap_or_else(|error| panic!("kernel translation fence failed: {error}"));
        self.framed.commit_vacant(entry);
        Ok(())
    }

    /// 摘除以 `start` 开头的 frame 支撑区间；调用者在释放锁后 `synchronize`。
    ///
    /// # Panics
    ///
    /// `start` 不是已登记区间的起点时 fail-stop。
    pub(crate) fn retire_framed(&mut self, start: usize) -> KernelRetirement {
        let start = VirtualAddress::from(start).floor();
        let region = self
            .framed
            .remove(&start)
            .expect("retired kernel region is not mapped");
        let mut commit = TranslationCommit::new();
        for page in start.as_usize()..region.end.as_usize() {
            self.page_table
                .unmap(VirtualPageNumber::from_vpn(page), &mut commit)
                .expect("kernel region page table lost a mapping");
        }
        KernelRetirement {
            commit,
            _frames: region.frames,
        }
    }

    /// 把一段设备寄存器（不得与 RAM 重叠）映射进内核，返回其虚拟地址；已映射的页保持不变。
    ///
    /// # Errors
    /// 区间与 RAM 重叠或页表页分配失败返回 `Err(())`。
    pub(crate) fn map_device(&mut self, range: Range<usize>) -> Result<usize, ()> {
        let ram = crate::platform::physical_memory_range();
        if range.start >= range.end || (range.start < ram.end && range.end > ram.start) {
            return Err(());
        }
        let start = PhysicalAddress::from(range.start).floor();
        let end = PhysicalAddress::from(range.end).ceil();
        let virtual_start =
            crate::arch::mmu::physical_to_virtual(usize::from(PhysicalAddress::from(start)));
        let mut commit = TranslationCommit::new();
        for (index, page) in (start.as_usize()..end.as_usize()).enumerate() {
            let vpn = VirtualAddress::from(virtual_start).floor();
            let vpn = VirtualPageNumber::from_vpn(vpn.as_usize() + index);
            if self.page_table.translate(vpn).is_some() {
                continue;
            }
            self.page_table
                .map(
                    vpn,
                    page.into(),
                    PagePermissions::READ
                        | PagePermissions::WRITE
                        | PagePermissions::DEVICE
                        | PagePermissions::GLOBAL,
                    &mut commit,
                )
                .map_err(|_| ())?;
        }
        // invalid → valid 只需本地 fence。
        commit
            .synchronize()
            .unwrap_or_else(|error| panic!("kernel translation fence failed: {error}"));
        Ok(virtual_start + range.start % PAGE_SIZE)
    }

    /// 内核页表的 trap token（trampoline 进入内核时切换到它）。
    pub(crate) fn kernel_trap_token(&self) -> crate::arch::mmu::KernelTrapToken {
        self.page_table.kernel_trap_token()
    }

    /// 在当前 CPU 激活内核页表。
    pub(crate) fn activate(&self) {
        self.page_table.activate_kernel();
    }
}

/// 建立内核页表并在当前 CPU 激活。
///
/// # Panics
///
/// 启动期映射失败或重复初始化时 fail-stop。
pub(crate) fn initialize() {
    assert!(
        KERNEL_SPACE.get().is_none(),
        "kernel space initialized twice"
    );
    crate::arch::mmu::initialize_kernel_mmio(crate::platform::kernel_mmio_regions());
    let mut space = KernelSpace {
        page_table: PageTable::try_new(AddressSpaceKind::Kernel)
            .expect("kernel root page table allocation failed"),
        framed: FallibleMap::new(),
    };
    let symbol = |symbol: unsafe extern "C" fn()| symbol as *const () as usize;
    space.map_trampoline_into_kernel();
    for region in crate::platform::kernel_mmio_regions() {
        let virtual_range = crate::arch::mmu::physical_range_to_virtual(region.clone());
        let start = VirtualAddress::from(virtual_range.start).floor();
        let end = VirtualAddress::from(virtual_range.end).ceil();
        let mut commit = TranslationCommit::new();
        space
            .page_table
            .map_contiguous_range(
                start,
                PhysicalAddress::from(region.start).floor(),
                end.as_usize() - start.as_usize(),
                PagePermissions::READ
                    | PagePermissions::WRITE
                    | PagePermissions::DEVICE
                    | PagePermissions::GLOBAL,
                &mut commit,
            )
            .expect("platform MMIO mapping failed");
        commit.finish_unpublished();
    }
    space.map_kernel_range(
        symbol(memory::stext)..symbol(memory::etext),
        PagePermissions::READ | PagePermissions::EXECUTE,
    );
    space.map_kernel_range(
        symbol(memory::srodata)..symbol(memory::erodata),
        PagePermissions::READ,
    );
    space.map_kernel_range(
        symbol(memory::sdata)..symbol(memory::edata),
        PagePermissions::READ | PagePermissions::WRITE,
    );
    space.map_kernel_range(
        symbol(memory::sbss)..symbol(memory::ebss),
        PagePermissions::READ | PagePermissions::WRITE,
    );
    // boot CPU early stack 底部保留一页 guard，越界立即缺页。
    let (boot_bottom, boot_top) = memory::boot_stack_range();
    space.map_kernel_range(
        boot_bottom + PAGE_SIZE..boot_top,
        PagePermissions::READ | PagePermissions::WRITE,
    );
    // direct map：内核镜像之后的全部 RAM，可读写、不可执行。
    let kernel_end = crate::arch::mmu::virtual_to_physical(symbol(memory::ekernel))
        .expect("kernel end is outside architecture direct map");
    space.map_kernel_range(
        crate::arch::mmu::physical_to_virtual(kernel_end)
            ..crate::arch::mmu::physical_to_virtual(crate::platform::physical_memory_end()),
        PagePermissions::READ | PagePermissions::WRITE,
    );
    KERNEL_SPACE.call_once(|| Mutex::new(space));
    KERNEL_SPACE.wait().lock().activate();
}

impl KernelSpace {
    fn map_trampoline_into_kernel(&mut self) {
        let mut commit = TranslationCommit::new();
        super::map_trampoline(&mut self.page_table, &mut commit)
            .expect("kernel trampoline mapping failed");
        commit.finish_unpublished();
    }
}
