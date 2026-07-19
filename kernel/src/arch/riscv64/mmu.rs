use core::{
    arch::asm,
    sync::atomic::{AtomicUsize, Ordering},
};

const PHYSICAL_ADDRESS_WIDTH: usize = 56;
const VIRTUAL_ADDRESS_WIDTH: usize = 39;
const PAGE_SHIFT: usize = 12;
const SATP_ASID_SHIFT: usize = 44;
const SATP_ASID_BITS: usize = 16;
const SATP_ASID_MASK: usize = ((1usize << SATP_ASID_BITS) - 1) << SATP_ASID_SHIFT;
const MAX_ADDRESS_SPACE_IDS: usize = 1 << SATP_ASID_BITS;
const ASID_BITMAP_WORDS: usize = MAX_ADDRESS_SPACE_IDS / usize::BITS as usize;

// OWNER: this bitmap is the sole global ASID lifecycle owner. A set bit cannot be cleared until
// MemorySet has synchronously invalidated every online CPU; without that ordering, a reused ASID
// could select a stale translation belonging to a freed page-table root.
static ADDRESS_SPACE_ID_CAPACITY: AtomicUsize = AtomicUsize::new(0);
static ADDRESS_SPACE_IDS: [AtomicUsize; ASID_BITMAP_WORDS] =
    [const { AtomicUsize::new(0) }; ASID_BITMAP_WORDS];
// OWNER: one native CPU-seen bitmap per ASID. First activation on each CPU executes an ASID-scoped
// fence that orders page-table publication before implicit reads. Retirement clears all CPU TLBs
// before the allocator resets this word; omitting it would skip the ordering fence on a CPU that
// has never observed the page-table writes.
static ADDRESS_SPACE_ID_CPUS: [AtomicUsize; MAX_ADDRESS_SPACE_IDS] =
    [const { AtomicUsize::new(0) }; MAX_ADDRESS_SPACE_IDS];

pub(crate) const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub(crate) const DIRECT_MAP_BASE: usize = 0;
pub(crate) const USER_ADDRESS_END: usize = 1 << (VIRTUAL_ADDRESS_WIDTH - 1);
pub(crate) const TRAMPOLINE_ADDRESS: usize = usize::MAX - PAGE_SIZE + 1;
pub(crate) const TRAP_CONTEXT_ADDRESS: usize = TRAMPOLINE_ADDRESS - PAGE_SIZE;
pub(crate) const SIGNAL_TRAMPOLINE_ADDRESS: usize = TRAP_CONTEXT_ADDRESS - PAGE_SIZE;
/// 保留既有 Sv39 kernel-stack virtual layout。
pub(crate) const KERNEL_STACK_REGION_START: usize = 0;
pub(crate) const KERNEL_STACK_REGION_TOP: usize = TRAP_CONTEXT_ADDRESS;
/// 保持既有 Sv39 用户上界 guard 布局的初始用户栈 exclusive top。
pub(crate) const USER_STACK_TOP: usize = USER_ADDRESS_END - PAGE_SIZE;

/// @description 将 raw integer 限制到 RISC-V physical-address width。
/// @param address generic memory 传入的地址值。
/// @return backend 可表达的 physical address。
pub(crate) fn normalize_physical_address(address: usize) -> usize {
    address & ((1usize << PHYSICAL_ADDRESS_WIDTH) - 1)
}

/// @description 将 raw page number 限制到 RISC-V physical-page width。
/// @param page generic memory 传入的 page number。
/// @return backend 可表达的 physical page number。
pub(crate) fn normalize_physical_page(page: usize) -> usize {
    page & ((1usize << (PHYSICAL_ADDRESS_WIDTH - PAGE_SHIFT)) - 1)
}

/// @description 将 raw page number 限制到 Sv39 virtual-page width。
/// @param page generic memory 传入的 page number。
/// @return backend 可表达的 virtual page number。
pub(crate) fn normalize_virtual_page(page: usize) -> usize {
    page & ((1usize << (VIRTUAL_ADDRESS_WIDTH - PAGE_SHIFT)) - 1)
}

/// @description 将 raw Sv39 value 转换为 pointer 可用的 canonical virtual address。
/// @param address generic memory 保存的 virtual-address bits。
/// @return 按 Sv39 sign-extension 规则 canonicalize 的地址。
pub(crate) fn canonicalize_virtual_address(address: usize) -> usize {
    let mask = (1usize << VIRTUAL_ADDRESS_WIDTH) - 1;
    let sign_bit = 1usize << (VIRTUAL_ADDRESS_WIDTH - 1);
    let raw = address & mask;
    if raw & sign_bit == 0 {
        raw
    } else {
        raw | !mask
    }
}

/// @description RISC-V 当前保持恒等 direct map，把 physical fact 转为 kernel address。
pub(crate) fn physical_to_virtual(address: usize) -> usize {
    assert_eq!(
        address,
        normalize_physical_address(address),
        "RISC-V direct-map physical address exceeds supported width"
    );
    DIRECT_MAP_BASE + normalize_physical_address(address)
}

/// @description RISC-V 当前恒等 direct map 的逆变换。
pub(crate) fn virtual_to_physical(address: usize) -> Option<usize> {
    let normalized = normalize_physical_address(address);
    (normalized == address).then_some(normalized)
}

/// @description RISC-V Sv39 address-space token；raw `satp` encoding 不跨越 arch seam。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddressSpaceToken(usize);

/// RISC-V user trap 仍需切换回包含 kernel mapping 的 Sv39 root。
pub(crate) type KernelTrapToken = AddressSpaceToken;

impl AddressSpaceToken {
    /// @description 从 Sv39 root physical page number 构造 address-space token。
    pub(crate) fn from_root_page(root_page: usize, address_space_id: usize) -> Self {
        assert!(
            root_page < 1usize << 44,
            "Sv39 root PPN exceeds satp encoding"
        );
        assert!(
            address_space_id > 0
                && address_space_id < ADDRESS_SPACE_ID_CAPACITY.load(Ordering::Acquire),
            "Sv39 address-space identifier is not allocated"
        );
        Self(root_page | address_space_id << SATP_ASID_SHIFT | 8usize << 60)
    }

    pub(super) fn encoded(self) -> usize {
        self.0
    }
}

/// @description 探测并验证当前 hart 的 `satp.ASID` 宽度。
///
/// 每个 CPU 在进入共享 execution 前调用；boot CPU 发布全局容量，secondary 必须得到相同
/// 结果。恢复原 `satp` 后执行一次全量 fence，缺失它会让探测期间的临时 ASID translation
/// 泄漏到后续执行。
pub(crate) fn initialize_address_space_identifiers() {
    let original: usize;
    let observed: usize;
    // SAFETY: startup owns this hart's satp; only ASID WARL bits are probed with the same MODE/PPN,
    // then the original value is restored before any shared kernel state is used.
    unsafe {
        asm!("csrr {value}, satp", value = out(reg) original, options(nomem, nostack));
        asm!("csrw satp, {value}", value = in(reg) (original | SATP_ASID_MASK), options(nostack));
        asm!("csrr {value}, satp", value = out(reg) observed, options(nomem, nostack));
        asm!("csrw satp, {value}", value = in(reg) original, options(nostack));
        asm!("sfence.vma", options(nostack));
    }
    let mask = (observed & SATP_ASID_MASK) >> SATP_ASID_SHIFT;
    assert_ne!(mask, 0, "RISC-V platform provides no usable ASID");
    assert!(
        (mask + 1).is_power_of_two(),
        "satp.ASID implements a non-contiguous WARL mask"
    );
    let capacity = mask + 1;
    match ADDRESS_SPACE_ID_CAPACITY.compare_exchange(
        0,
        capacity,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(published) => assert_eq!(
            published, capacity,
            "CPUs report inconsistent satp.ASID widths"
        ),
    }
}

/// @description 分配一个跨全部 CPU 保持同一 address-space identity 的非零 ASID。
/// @return 可用 ASID；容量耗尽时返回 None。
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
                Ok(_) => {
                    ADDRESS_SPACE_ID_CPUS[identifier].store(0, Ordering::Release);
                    return Some(identifier);
                }
                Err(observed) => current = observed,
            }
        }
    }
    None
}

fn prepare_local_activation(token: AddressSpaceToken, logical_cpu: usize) {
    let identifier = (token.encoded() & SATP_ASID_MASK) >> SATP_ASID_SHIFT;
    let cpu_bit = 1usize
        .checked_shl(logical_cpu as u32)
        .expect("logical CPU exceeds ASID activation bitmap");
    let seen = &ADDRESS_SPACE_ID_CPUS[identifier];
    if seen.load(Ordering::Acquire) & cpu_bit != 0 {
        return;
    }
    // SAFETY: rs1=x0 selects every virtual address for this nonzero ASID; the current CPU owns its
    // activation bit and executes the fence before publishing that the ASID has been observed.
    unsafe { asm!("sfence.vma x0, {asid}", asid = in(reg) identifier, options(nostack)) };
    seen.fetch_or(cpu_bit, Ordering::Release);
}

/// @description 为 trampoline 的下一次 user `satp` 切换准备 CPU-local ASID state。
pub(crate) fn prepare_user_activation(token: AddressSpaceToken, logical_cpu: usize) {
    prepare_local_activation(token, logical_cpu);
}

/// @description 在 caller 已完成全 CPU translation invalidation 后释放 ASID。
/// @param identifier 待复用的非零、当前已分配 ID。
pub(super) fn release_address_space_id_after_global_fence(identifier: usize) {
    let word = &ADDRESS_SPACE_IDS[identifier / usize::BITS as usize];
    let bit = 1usize << (identifier % usize::BITS as usize);
    let previous = word.fetch_and(!bit, Ordering::Release);
    assert_ne!(previous & bit, 0, "address-space identifier released twice");
}

/// @description 激活当前 CPU 的 tagged Sv39 address space。
///
/// @param token 由 live page-table root 构造的 token。
pub(crate) fn activate(token: AddressSpaceToken) {
    prepare_local_activation(token, super::startup::current_logical_id());
    // SAFETY: token encodes a live Sv39 root and globally unique ASID. Page-table mutation and
    // ASID retirement owners execute the required fences, so an address-space switch needs none.
    unsafe {
        riscv::register::satp::write(riscv::register::satp::Satp::from_bits(token.encoded()));
    }
}

/// @description RISC-V kernel/user 共享同一 Sv39 root，激活 kernel root 等价于普通激活。
pub(crate) fn activate_kernel(token: AddressSpaceToken) {
    activate(token);
}

/// @description 失效当前 CPU 的全部 S-stage translations。
pub(crate) fn flush_local() {
    // SAFETY: kernel executes in S-mode and the instruction only affects local TLB state.
    unsafe { asm!("sfence.vma") };
}

/// @description 失效当前 CPU 上覆盖 `[start, start + size)` 的 S-stage translations。
/// @param start page-aligned canonical virtual address。
/// @param size 非零、page-aligned 字节数。
pub(crate) fn flush_local_range(start: usize, size: usize) {
    debug_assert_eq!(start % PAGE_SIZE, 0);
    debug_assert_ne!(size, 0);
    debug_assert_eq!(size % PAGE_SIZE, 0);
    let end = start
        .checked_add(size)
        .expect("local translation-fence range overflow");
    for address in (start..end).step_by(PAGE_SIZE) {
        // SAFETY: address 是 canonical page address；rs2=x0 同步所有 ASID/global translation。
        unsafe { asm!("sfence.vma {address}, x0", address = in(reg) address) };
    }
}
