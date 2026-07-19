use core::{
    arch::asm,
    sync::atomic::{AtomicUsize, Ordering},
};

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
/// TTBR1 管理的 39-bit 高半区；`DIRECT_MAP_BASE + PA` 是内核访问物理内存的唯一地址。
pub(crate) const DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000;
const DIRECT_MAP_SIZE: usize = 120usize << 30;
/// TTBR1 kernel stack window 的 inclusive lower bound；高侧 136 GiB 与 direct map 静态不相交。
pub(crate) const KERNEL_STACK_REGION_START: usize = DIRECT_MAP_BASE + DIRECT_MAP_SIZE;
/// TTBR1 kernel stack window 的 page-aligned exclusive top；最后一页保留以避免地址回绕。
pub(crate) const KERNEL_STACK_REGION_TOP: usize = usize::MAX & !(PAGE_SIZE - 1);
pub(crate) const USER_ADDRESS_END: usize = 1 << (VIRTUAL_ADDRESS_WIDTH - 1);
pub(crate) const TRAMPOLINE_ADDRESS: usize = USER_ADDRESS_END - PAGE_SIZE;
pub(crate) const TRAP_CONTEXT_ADDRESS: usize = TRAMPOLINE_ADDRESS - PAGE_SIZE;
pub(crate) const SIGNAL_TRAMPOLINE_ADDRESS: usize = TRAP_CONTEXT_ADDRESS - PAGE_SIZE;
/// signal trampoline 下方保留一页 guard 后的初始用户栈 exclusive top。
pub(crate) const USER_STACK_TOP: usize = SIGNAL_TRAMPOLINE_ADDRESS - PAGE_SIZE;

const _: () =
    assert!(KERNEL_STACK_REGION_TOP - KERNEL_STACK_REGION_START >= (136usize << 30) - PAGE_SIZE);

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

/// @description 将受支持的物理地址转换为 TTBR1 高半区 direct-map 地址。
/// @param address 已经按 platform memory/MMIO fact 验证的物理地址。
/// @return 可供 kernel 解引用的 canonical virtual address。
pub(crate) fn physical_to_virtual(address: usize) -> usize {
    assert!(
        address < DIRECT_MAP_SIZE,
        "AArch64 direct-map physical address exceeds the current 120 GiB TTBR1 window"
    );
    DIRECT_MAP_BASE
        .checked_add(address)
        .expect("AArch64 direct-map address overflow")
}

/// @description 尝试把 TTBR1 direct-map 地址还原为物理地址。
/// @param address kernel virtual address。
/// @return 地址属于 39-bit direct-map window 时返回物理地址，否则返回 `None`。
pub(crate) fn virtual_to_physical(address: usize) -> Option<usize> {
    let address = canonicalize_virtual_address(address);
    let offset = address.checked_sub(DIRECT_MAP_BASE)?;
    (offset < DIRECT_MAP_SIZE).then_some(offset)
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

/// Broadcast an all-ASID EL1 stage-1 invalidation to the inner-shareable domain.
///
/// `(0, 0)` or `size == usize::MAX` selects a full invalidation; otherwise the range must be
/// nonempty and page aligned. `usize::MAX` is the generic sparse-span normalization sentinel.
pub(crate) fn broadcast_tlb(start: usize, size: usize) {
    if start == 0 && size == 0 || size == usize::MAX {
        // SAFETY: VMALLE1IS affects the inner-shareable EL1 translation domain; barriers complete
        // break-before-reuse before the generic owner reclaims retired frames.
        unsafe {
            asm!(
                "dsb ishst",
                "tlbi vmalle1is",
                "dsb ish",
                "isb",
                options(nostack)
            )
        };
        return;
    }
    assert!(
        start.is_multiple_of(PAGE_SIZE) && size != 0 && size.is_multiple_of(PAGE_SIZE),
        "invalid AArch64 broadcast TLB range"
    );
    let end = start
        .checked_add(size)
        .expect("broadcast TLB range overflow");
    // SAFETY: operands are validated VA page numbers; VAAE1IS targets every ASID in the
    // inner-shareable translation domain.
    unsafe { asm!("dsb ishst", options(nostack)) };
    for address in (start..end).step_by(PAGE_SIZE) {
        let operand = super::va39::tlbi_all_asid_operand(address);
        // SAFETY: this iteration's operand is a validated page number in the bounded range.
        unsafe { asm!("tlbi vaae1is, {operand}", operand = in(reg) operand, options(nostack)) };
    }
    // SAFETY: the completion barriers close the preceding broadcast invalidation sequence.
    unsafe { asm!("dsb ish", "isb", options(nostack)) };
}
