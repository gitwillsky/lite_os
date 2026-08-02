use alloc::vec::Vec;
use core::{
    arch::asm,
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};
use spin::Once;

const PHYSICAL_ADDRESS_WIDTH: usize = 48;
const VIRTUAL_ADDRESS_WIDTH: usize = 39;
const PAGE_SHIFT: usize = 12;
const ASID_BITS_MAX: usize = 16;
const MAX_ADDRESS_SPACE_IDS: usize = 1 << ASID_BITS_MAX;
const ASID_BITMAP_WORDS: usize = MAX_ADDRESS_SPACE_IDS / usize::BITS as usize;

// OWNER: this bitmap is the sole ASID lifecycle owner. An ID is released only after the memory
// owner has completed the all-CPU invalidation; earlier reuse can expose a retired translation.
static ADDRESS_SPACE_ID_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static ADDRESS_SPACE_IDS: [AtomicUsize; ASID_BITMAP_WORDS] =
    [const { AtomicUsize::new(0) }; ASID_BITMAP_WORDS];

pub(crate) const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
/// TTBR1 管理的 39-bit 高半区；低侧保持连续物理 RAM direct-map。
pub(crate) const DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000;
const DIRECT_MAP_SIZE: usize = 120usize << 30;
const TTBR1_ADDRESS_TOP: usize = usize::MAX & !(PAGE_SIZE - 1);
/// 架构拥有的 high-MMIO 虚拟窗口；缺失它会让高地址 PCI ECAM 无法进入 TTBR1。
const KERNEL_MMIO_WINDOW_SIZE: usize = 1usize << 30;
/// high-MMIO window 的 inclusive lower bound。
pub(crate) const KERNEL_MMIO_REGION_START: usize = TTBR1_ADDRESS_TOP - KERNEL_MMIO_WINDOW_SIZE;
/// high-MMIO window 的 page-aligned exclusive top；最后一页保留以避免地址回绕。
pub(crate) const KERNEL_MMIO_REGION_TOP: usize = TTBR1_ADDRESS_TOP;
/// TTBR1 kernel stack window 的 inclusive lower bound；high-MMIO window 与 direct map 静态不相交。
pub(crate) const KERNEL_STACK_REGION_START: usize = DIRECT_MAP_BASE + DIRECT_MAP_SIZE;
/// TTBR1 kernel stack window 的 page-aligned exclusive top；其上方固定保留 high-MMIO window。
pub(crate) const KERNEL_STACK_REGION_TOP: usize = KERNEL_MMIO_REGION_START;
pub(crate) const USER_ADDRESS_END: usize = 1 << (VIRTUAL_ADDRESS_WIDTH - 1);
pub(crate) const TRAMPOLINE_ADDRESS: usize = USER_ADDRESS_END - PAGE_SIZE;
pub(crate) const TRAP_CONTEXT_ADDRESS: usize = TRAMPOLINE_ADDRESS - PAGE_SIZE;
pub(crate) const SIGNAL_TRAMPOLINE_ADDRESS: usize = TRAP_CONTEXT_ADDRESS - PAGE_SIZE;
/// signal trampoline 下方保留一页 guard 后的初始用户栈 exclusive top。
pub(crate) const USER_STACK_TOP: usize = SIGNAL_TRAMPOLINE_ADDRESS - PAGE_SIZE;

const _: () =
    assert!(KERNEL_STACK_REGION_TOP - KERNEL_STACK_REGION_START >= (135usize << 30) - PAGE_SIZE);

#[derive(Debug, Clone, Copy)]
struct KernelMmioMapping {
    physical_start: usize,
    physical_end: usize,
    virtual_start: usize,
}

// OWNER: this table is the sole AArch64 physical-MMIO to TTBR1-window projection. Boot memory
// initialization publishes all validated platform ranges before any device adapter can translate
// a high physical address; keeping it immutable prevents PCI/GIC/driver modules from maintaining
// separate aliases. Missing publication would make a high MMIO pointer either unmapped or stale.
static KERNEL_MMIO_MAPPINGS: Once<Vec<KernelMmioMapping>> = Once::new();

/// Mask a raw integer to the supported stage-1 physical-address width.
pub(crate) fn normalize_physical_address(address: usize) -> usize {
    address & ((1usize << PHYSICAL_ADDRESS_WIDTH) - 1)
}

/// Mask a raw page number to the supported physical-page width.
pub(crate) fn normalize_physical_page(page: usize) -> usize {
    page & ((1usize << (PHYSICAL_ADDRESS_WIDTH - PAGE_SHIFT)) - 1)
}

/// Mask a raw page number to the 39-bit virtual-page width.
pub(crate) fn normalize_virtual_page(page: usize) -> usize {
    page & ((1usize << (VIRTUAL_ADDRESS_WIDTH - PAGE_SHIFT)) - 1)
}

/// Canonicalize a 39-bit virtual address by sign extension.
pub(crate) fn canonicalize_virtual_address(address: usize) -> usize {
    let mask = (1usize << VIRTUAL_ADDRESS_WIDTH) - 1;
    let sign = 1usize << (VIRTUAL_ADDRESS_WIDTH - 1);
    let raw = address & mask;
    if raw & sign == 0 { raw } else { raw | !mask }
}

/// @description 发布平台 MMIO 的唯一 TTBR1 映射投影。
/// @param regions platform 已验证的 MMIO physical ranges。
/// @return 无返回值；发布后 `physical_to_virtual` 与 `virtual_to_physical` 共享同一映射表。
/// @errors high MMIO 总跨度超过架构窗口、range 跨越 direct-map 边界或重复发布时 fail-stop。
pub(crate) fn initialize_kernel_mmio<I>(regions: I)
where
    I: IntoIterator<Item = Range<usize>>,
{
    assert!(
        KERNEL_MMIO_MAPPINGS.get().is_none(),
        "AArch64 kernel MMIO mappings initialized twice"
    );
    let mut mappings: Vec<KernelMmioMapping> = Vec::new();
    let mut virtual_cursor = KERNEL_MMIO_REGION_START;
    for region in regions {
        assert!(
            region.start < region.end,
            "AArch64 kernel MMIO range is empty or reversed"
        );
        let in_direct_map = region.end <= DIRECT_MAP_SIZE;
        assert_eq!(
            region.start < DIRECT_MAP_SIZE,
            in_direct_map,
            "AArch64 kernel MMIO range crosses the RAM direct-map boundary"
        );
        if in_direct_map {
            continue;
        }

        let physical_start = region.start & !(PAGE_SIZE - 1);
        let physical_end = region
            .end
            .checked_add(PAGE_SIZE - 1)
            .expect("AArch64 kernel MMIO range alignment overflow")
            & !(PAGE_SIZE - 1);
        let size = physical_end
            .checked_sub(physical_start)
            .expect("AArch64 kernel MMIO range is reversed after alignment");
        let virtual_start = virtual_cursor;
        virtual_cursor = virtual_cursor
            .checked_add(size)
            .expect("AArch64 high-MMIO virtual window overflow");
        assert!(
            virtual_cursor <= KERNEL_MMIO_REGION_TOP,
            "AArch64 high-MMIO ranges exceed the fixed TTBR1 window"
        );
        assert!(
            mappings.iter().all(|mapping| {
                physical_end <= mapping.physical_start || physical_start >= mapping.physical_end
            }),
            "AArch64 high-MMIO ranges overlap"
        );
        mappings
            .try_reserve(1)
            .expect("AArch64 kernel MMIO mapping table allocation failed");
        mappings.push(KernelMmioMapping {
            physical_start,
            physical_end,
            virtual_start,
        });
    }
    KERNEL_MMIO_MAPPINGS.call_once(|| mappings);
}

/// @description 将受支持的物理地址转换为 TTBR1 kernel mapping 地址。
/// @param address 已经按 platform memory/MMIO fact 验证的物理地址。
/// @return 可供 kernel 解引用的 canonical virtual address。
pub(crate) fn physical_to_virtual(address: usize) -> usize {
    if address < DIRECT_MAP_SIZE {
        return DIRECT_MAP_BASE
            .checked_add(address)
            .expect("AArch64 direct-map address overflow");
    }
    if let Some(mapping) = KERNEL_MMIO_MAPPINGS.get().and_then(|mappings| {
        mappings
            .iter()
            .find(|mapping| (mapping.physical_start..mapping.physical_end).contains(&address))
    }) {
        return mapping
            .virtual_start
            .checked_add(address - mapping.physical_start)
            .expect("AArch64 high-MMIO virtual address overflow");
    }
    panic!("AArch64 physical address has no kernel mapping");
}

/// @description 将同一已发布映射内的半开物理区间转换为半开 TTBR1 kernel 区间。
/// @param range 已经按 platform fact 验证且不为空的 physical range。
/// @return 与输入等长、可供 `MapArea` 使用的 canonical virtual range。
/// @errors range 跨 direct-map 与 high-MMIO seam、未发布或溢出时 fail-stop。
pub(crate) fn physical_range_to_virtual(range: Range<usize>) -> Range<usize> {
    assert!(
        range.start < range.end,
        "AArch64 physical range is empty or reversed"
    );
    if range.end <= DIRECT_MAP_SIZE {
        return DIRECT_MAP_BASE
            .checked_add(range.start)
            .expect("AArch64 direct-map range start overflow")
            ..DIRECT_MAP_BASE
                .checked_add(range.end)
                .expect("AArch64 direct-map range end overflow");
    }
    assert!(
        range.start >= DIRECT_MAP_SIZE,
        "AArch64 physical range crosses the RAM direct-map boundary"
    );
    let mapping = KERNEL_MMIO_MAPPINGS
        .get()
        .and_then(|mappings| {
            mappings.iter().find(|mapping| {
                range.start >= mapping.physical_start && range.end <= mapping.physical_end
            })
        })
        .expect("AArch64 physical range has no kernel mapping");
    let start_offset = range.start - mapping.physical_start;
    let end_offset = range.end - mapping.physical_start;
    let virtual_start = mapping
        .virtual_start
        .checked_add(start_offset)
        .expect("AArch64 high-MMIO range start overflow");
    let virtual_end = mapping
        .virtual_start
        .checked_add(end_offset)
        .expect("AArch64 high-MMIO range end overflow");
    virtual_start..virtual_end
}

/// @description 尝试把 TTBR1 kernel mapping 地址还原为物理地址。
/// @param address kernel virtual address。
/// @return 地址属于 direct-map 或已发布 high-MMIO window 时返回物理地址，否则返回 `None`。
pub(crate) fn virtual_to_physical(address: usize) -> Option<usize> {
    let address = canonicalize_virtual_address(address);
    let offset = address.checked_sub(DIRECT_MAP_BASE)?;
    if offset < DIRECT_MAP_SIZE {
        return Some(offset);
    }
    KERNEL_MMIO_MAPPINGS.get().and_then(|mappings| {
        mappings.iter().find_map(|mapping| {
            let size = mapping.physical_end - mapping.physical_start;
            let virtual_end = mapping.virtual_start.checked_add(size)?;
            (mapping.virtual_start..virtual_end)
                .contains(&address)
                .then_some(mapping.physical_start + address - mapping.virtual_start)
        })
    })
}

/// Opaque AArch64 TTBR address-space token.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddressSpaceToken(u64);

/// TTBR1 常驻后，user trap 不再携带或切换 kernel TTBR0 root。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KernelTrapToken;

impl AddressSpaceToken {
    /// Construct a token from an aligned root physical page and allocated nonzero ASID.
    pub(crate) fn from_root_page(root_page: usize, address_space_id: usize) -> Self {
        let capacity = ADDRESS_SPACE_ID_CAPACITY.load(Ordering::Acquire);
        assert!(
            root_page < 1usize << 36,
            "AArch64 root page exceeds 48-bit TTBR encoding"
        );
        assert!(
            address_space_id > 0 && address_space_id < capacity,
            "AArch64 ASID is not allocated"
        );
        Self(((address_space_id as u64) << 48) | ((root_page as u64) << PAGE_SHIFT))
    }

    pub(super) fn encoded(self) -> u64 {
        self.0
    }
}

/// Probe and publish the hardware ASID capacity.
pub(crate) fn initialize_address_space_identifiers() -> bool {
    let mmfr0: u64;
    // SAFETY: ID_AA64MMFR0_EL1 is a read-only CPU identification register at EL1.
    unsafe {
        asm!("mrs {value}, id_aa64mmfr0_el1", value = out(reg) mmfr0, options(nomem, nostack, preserves_flags))
    };
    let bits = match (mmfr0 >> 4) & 0xf {
        0 => 8,
        2 => 16,
        value => panic!("unsupported ID_AA64MMFR0_EL1.ASIDBits encoding {value}"),
    };
    let capacity = 1usize << bits;
    match ADDRESS_SPACE_ID_CAPACITY.compare_exchange(
        0,
        capacity,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(published) => assert_eq!(
            published, capacity,
            "CPUs report inconsistent AArch64 ASID widths"
        ),
    }
    bits == ASID_BITS_MAX
}

pub(super) fn allocate_address_space_id() -> Option<usize> {
    let capacity = ADDRESS_SPACE_ID_CAPACITY.load(Ordering::Acquire);
    assert_ne!(capacity, 0, "ASID allocator used before CPU initialization");
    for identifier in 1..capacity {
        let word = &ADDRESS_SPACE_IDS[identifier / usize::BITS as usize];
        let bit = 1usize << (identifier % usize::BITS as usize);
        let mut current = word.load(Ordering::Acquire);
        while current & bit == 0 {
            match word.compare_exchange_weak(
                current,
                current | bit,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(identifier),
                Err(observed) => current = observed,
            }
        }
    }
    None
}

pub(super) fn release_address_space_id_after_global_fence(identifier: usize) {
    let word = &ADDRESS_SPACE_IDS[identifier / usize::BITS as usize];
    let bit = 1usize << (identifier % usize::BITS as usize);
    let previous = word.fetch_and(!bit, Ordering::Release);
    assert_ne!(previous & bit, 0, "address-space identifier released twice");
}

/// @description 激活全局 TTBR1 kernel page-table root，不修改当前 TTBR0 user root。
/// @param root_page 由 live kernel page-table owner 持有的物理根页号。
pub(crate) fn activate_kernel(root_page: usize) {
    let root = (root_page as u64) << PAGE_SHIFT;
    assert_eq!(root & (PAGE_SIZE as u64 - 1), 0);
    // SAFETY: root owner remains live for the kernel lifetime. TTBR1 uses ASID 0 and every kernel
    // leaf is global; DSB/ISB publish table writes before instruction/data fetch can use them.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr1_el1, {root}",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            root = in(reg) root,
            options(nostack)
        )
    };
}

/// Invalidate all EL1 stage-1 translations on the calling CPU.
pub(crate) fn flush_local() {
    // SAFETY: VMALLE1 invalidates only local EL1 stage-1 TLB state.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack)
        )
    };
}

/// Invalidate all-ASID translations covering an aligned local virtual-address range.
pub(crate) fn flush_local_range(start: usize, size: usize) {
    debug_assert_eq!(start % PAGE_SIZE, 0);
    debug_assert_ne!(size, 0);
    debug_assert_eq!(size % PAGE_SIZE, 0);
    if requires_full_local_flush(start) {
        // Apple HVF can retain a retired global TTBR1 translation after local VAAE1 even though
        // the page-table owner has removed the leaf. Kernel-stack VA reuse would then write into
        // the released physical frame. A full local VMALLE1 is one operation and establishes the
        // required completion point before the stack backing returns to the frame allocator.
        flush_local();
        return;
    }
    let end = start.checked_add(size).expect("local TLB range overflow");
    // SAFETY: caller supplies canonical page addresses; VAAE1 takes VA[55:12] and all ASIDs.
    unsafe { asm!("dsb ishst", options(nostack)) };
    for address in (start..end).step_by(PAGE_SIZE) {
        let operand = super::va39::tlbi_all_asid_operand(address);
        // SAFETY: operand contains the architected VAAE1 VA field and affects local TLB state only.
        unsafe { asm!("tlbi vaae1, {operand}", operand = in(reg) operand, options(nostack)) };
    }
    // SAFETY: complete invalidation before later memory access or instruction fetch.
    unsafe { asm!("dsb ish", "isb", options(nostack)) };
}

const fn requires_full_local_flush(start: usize) -> bool {
    start >= KERNEL_STACK_REGION_START
}

/// Broadcast a full EL1 stage-1 invalidation to the inner-shareable domain.
pub(crate) fn broadcast_tlb() {
    // SAFETY: VMALLE1IS affects the complete inner-shareable EL1 translation domain. Platform
    // code adds a per-vCPU rendezvous before reclaiming retired owners because Apple HVF may
    // otherwise return before every vCPU has crossed its flush point.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack)
        )
    };
}

/// Complete the target-vCPU side of an inner-shareable TLB broadcast rendezvous.
pub(crate) fn acknowledge_broadcast_tlb() {
    // SAFETY: the SGI exception has forced this vCPU through the hypervisor after the source
    // VMALLE1IS. Barriers prevent publishing the software ack before that observation completes.
    unsafe { asm!("dsb ish", "isb", options(nostack)) };
}

#[cfg(test)]
mod tests {
    use super::{
        DIRECT_MAP_BASE, KERNEL_MMIO_REGION_START, KERNEL_MMIO_REGION_TOP,
        KERNEL_STACK_REGION_START, KERNEL_STACK_REGION_TOP, initialize_kernel_mmio,
        physical_to_virtual, requires_full_local_flush, virtual_to_physical,
    };

    #[test]
    fn dynamic_ttbr1_stack_range_uses_full_local_flush() {
        assert!(!requires_full_local_flush(DIRECT_MAP_BASE));
        assert!(requires_full_local_flush(KERNEL_STACK_REGION_START));
    }

    #[test]
    fn high_mmio_projection_round_trips_without_overlapping_stacks() {
        initialize_kernel_mmio([0x4010_0000_00..0x4020_0000_00]);
        let physical = 0x4010_1234_5000;
        let virtual_address = physical_to_virtual(physical);
        assert!((KERNEL_MMIO_REGION_START..KERNEL_MMIO_REGION_TOP).contains(&virtual_address));
        assert_eq!(virtual_to_physical(virtual_address), Some(physical));
        let virtual_range = physical_range_to_virtual(0x4010_0000_00..0x4020_0000_00);
        assert_eq!(virtual_range.start, KERNEL_MMIO_REGION_START);
        assert_eq!(virtual_range.end - virtual_range.start, 0x1000_0000);
        assert!(KERNEL_STACK_REGION_TOP <= KERNEL_MMIO_REGION_START);
    }
}
