pub(super) use crate::arch::address_space_id::Context as AddressSpaceContext;
use crate::arch::address_space_id::{Allocator, Flush};
use alloc::sync::Arc;
use core::{
    arch::asm,
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};
use spin::Once;

const PHYSICAL_ADDRESS_WIDTH: usize = 56;
const VIRTUAL_ADDRESS_WIDTH: usize = 39;
const PAGE_SHIFT: usize = 12;
const SATP_ASID_SHIFT: usize = 44;
const SATP_ASID_BITS: usize = 16;
const SATP_ASID_MASK: usize = ((1usize << SATP_ASID_BITS) - 1) << SATP_ASID_SHIFT;
// OWNER: hardware capacity is an immutable probe fact, not an allocation bitmap.
static ADDRESS_SPACE_ID_CAPACITY: AtomicUsize = AtomicUsize::new(0);
// OWNER: one generation allocator owns all user hardware IDs, reservations and local flush debt.
static ADDRESS_SPACE_IDS: Once<Allocator> = Once::new();

pub(crate) const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub(crate) const DIRECT_MAP_BASE: usize = 0;
pub(crate) const USER_ADDRESS_END: usize = 1 << (VIRTUAL_ADDRESS_WIDTH - 1);
pub(crate) const TRAMPOLINE_ADDRESS: usize = usize::MAX - PAGE_SIZE + 1;
pub(crate) const TRAP_CONTEXT_ADDRESS: usize = TRAMPOLINE_ADDRESS - PAGE_SIZE;
pub(crate) const SIGNAL_TRAMPOLINE_ADDRESS: usize = TRAP_CONTEXT_ADDRESS - PAGE_SIZE;
/// user page table 中专供 Thread supervisor context 的 canonical high-half 区间；不含 trampoline。
pub(crate) const THREAD_TRAP_CONTEXT_RANGE: Option<core::ops::Range<usize>> =
    Some((usize::MAX - USER_ADDRESS_END + 1)..SIGNAL_TRAMPOLINE_ADDRESS);

/// 保留既有 Sv39 kernel-stack virtual layout。
pub(crate) const KERNEL_STACK_REGION_START: usize = 0;
pub(crate) const KERNEL_STACK_REGION_TOP: usize = TRAP_CONTEXT_ADDRESS;
/// 保持既有 Sv39 用户上界 guard 布局的初始用户栈 exclusive top。
pub(crate) const USER_STACK_TOP: usize = USER_ADDRESS_END - PAGE_SIZE;

/// 将 raw integer 限制到 RISC-V physical-address width。
///
/// # Parameters
///
/// - `address`: generic memory 传入的地址值。
///
/// # Returns
///
/// backend 可表达的 physical address。
pub(crate) fn normalize_physical_address(address: usize) -> usize {
    address & ((1usize << PHYSICAL_ADDRESS_WIDTH) - 1)
}

/// 将 raw page number 限制到 RISC-V physical-page width。
///
/// # Parameters
///
/// - `page`: generic memory 传入的 page number。
///
/// # Returns
///
/// backend 可表达的 physical page number。
pub(crate) fn normalize_physical_page(page: usize) -> usize {
    page & ((1usize << (PHYSICAL_ADDRESS_WIDTH - PAGE_SHIFT)) - 1)
}

/// 将 raw page number 限制到 Sv39 virtual-page width。
///
/// # Parameters
///
/// - `page`: generic memory 传入的 page number。
///
/// # Returns
///
/// backend 可表达的 virtual page number。
pub(crate) fn normalize_virtual_page(page: usize) -> usize {
    page & ((1usize << (VIRTUAL_ADDRESS_WIDTH - PAGE_SHIFT)) - 1)
}

/// 将 raw Sv39 value 转换为 pointer 可用的 canonical virtual address。
///
/// # Parameters
///
/// - `address`: generic memory 保存的 virtual-address bits。
///
/// # Returns
///
/// 按 Sv39 sign-extension 规则 canonicalize 的地址。
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

/// 发布平台 MMIO 映射；Sv39 当前使用物理地址恒等映射，因此无需额外窗口。
///
/// # Parameters
///
/// - `regions`: platform 已验证的 MMIO physical ranges。
///
/// # Errors
///
/// 当前 backend 不产生额外映射失败；超出 physical width 仍由地址 façade fail-stop。
pub(crate) fn initialize_kernel_mmio<I>(_regions: I)
where
    I: IntoIterator<Item = Range<usize>>,
{
}

/// RISC-V 当前保持恒等 direct map，把 physical fact 转为 kernel address。
pub(crate) fn physical_to_virtual(address: usize) -> usize {
    assert_eq!(
        address,
        normalize_physical_address(address),
        "RISC-V direct-map physical address exceeds supported width"
    );
    DIRECT_MAP_BASE + normalize_physical_address(address)
}

/// 将 Sv39 恒等映射中的半开物理区间转换为 kernel virtual range。
///
/// # Parameters
///
/// - `range`: 已经按 platform fact 验证且不为空的 physical range。
///
/// # Returns
///
/// 与输入相同的 virtual range。
///
/// # Errors
///
/// physical range 反向或超出 backend physical width 时 fail-stop。
pub(crate) fn physical_range_to_virtual(range: Range<usize>) -> Range<usize> {
    assert!(
        range.start < range.end,
        "RISC-V physical range is empty or reversed"
    );
    let start = physical_to_virtual(range.start);
    let end = physical_to_virtual(range.end);
    start..end
}

/// RISC-V 当前恒等 direct map 的逆变换。
pub(crate) fn virtual_to_physical(address: usize) -> Option<usize> {
    let normalized = normalize_physical_address(address);
    (normalized == address).then_some(normalized)
}

/// Stable Sv39 user root/context capability; raw satp stays inside the backend.
#[derive(Debug, Clone)]
pub(crate) struct AddressSpaceToken {
    root_page: usize,
    context: Arc<AddressSpaceContext>,
}

/// Kernel root uses reserved ASID zero and never participates in user rollover.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KernelTrapToken(usize);

impl KernelTrapToken {
    pub(super) fn from_root_page(root_page: usize) -> Self {
        assert!(
            root_page < 1usize << 44,
            "Sv39 root PPN exceeds satp encoding"
        );
        Self(root_page | 8usize << 60)
    }
    pub(super) fn encoded(self) -> usize {
        self.0
    }
}

impl AddressSpaceToken {
    pub(super) fn from_root_page(root_page: usize, context: Arc<AddressSpaceContext>) -> Self {
        assert!(
            root_page < 1usize << 44,
            "Sv39 root PPN exceeds satp encoding"
        );
        Self { root_page, context }
    }

    pub(super) fn activate(&self, logical_cpu: usize) -> usize {
        let identifier = ADDRESS_SPACE_IDS.wait().activate(&self.context, logical_cpu, |flush| {
            match flush {
                Flush::All => flush_local(),
                Flush::Identifier(identifier) => {
                    // SAFETY: rs1=x0 selects all addresses of this nonzero local ASID. The fence
                    // completes before satp installation and publication of its CPU footprint.
                    unsafe { asm!("sfence.vma x0, {asid}", asid = in(reg) identifier, options(nostack)) };
                }
            }
        });
        self.root_page | identifier << SATP_ASID_SHIFT | 8usize << 60
    }
}

/// 探测并验证当前 hart 的 `satp.ASID` 宽度。
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

/// Publish the generation owner after logical CPU topology and before any user root.
pub(crate) fn initialize_asid_allocator(cpu_count: usize) {
    assert!(
        ADDRESS_SPACE_IDS.get().is_none(),
        "ASID allocator initialized twice"
    );
    let capacity = ADDRESS_SPACE_ID_CAPACITY.load(Ordering::Acquire);
    let allocator =
        Allocator::try_new(capacity, cpu_count).expect("ASID storage allocation failed");
    ADDRESS_SPACE_IDS.call_once(|| allocator);
    crate::info!("ASID generation allocator initialized: capacity={capacity} cpus={cpu_count}");
}

/// Install the permanent kernel root at reserved ASID zero and publish its page-table writes.
pub(crate) fn activate_kernel(token: KernelTrapToken) {
    // SAFETY: token belongs to the kernel-lifetime root; initial local fence publishes it before use.
    unsafe {
        riscv::register::satp::write(riscv::register::satp::Satp::from_bits(token.encoded()));
    }
    flush_local();
}

/// 失效当前 CPU 的全部 S-stage translations。
pub(crate) fn flush_local() {
    // SAFETY: kernel executes in S-mode and the instruction only affects local TLB state.
    unsafe { asm!("sfence.vma") };
}

/// 失效当前 CPU 上覆盖 `[start, start + size)` 的 S-stage translations。
///
/// # Parameters
///
/// - `start`: page-aligned canonical virtual address。
/// - `size`: 非零、page-aligned 字节数。
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
