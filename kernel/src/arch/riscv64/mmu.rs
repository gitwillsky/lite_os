use core::arch::asm;

const PHYSICAL_ADDRESS_WIDTH: usize = 56;
const VIRTUAL_ADDRESS_WIDTH: usize = 39;
const PAGE_SHIFT: usize = 12;

pub(crate) const PAGE_SIZE: usize = 1 << PAGE_SHIFT;
pub(crate) const USER_ADDRESS_END: usize = 1 << (VIRTUAL_ADDRESS_WIDTH - 1);
pub(crate) const TRAMPOLINE_ADDRESS: usize = usize::MAX - PAGE_SIZE + 1;
pub(crate) const TRAP_CONTEXT_ADDRESS: usize = TRAMPOLINE_ADDRESS - PAGE_SIZE;
pub(crate) const SIGNAL_TRAMPOLINE_ADDRESS: usize = TRAP_CONTEXT_ADDRESS - PAGE_SIZE;

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

/// @description RISC-V Sv39 address-space token；raw `satp` encoding 不跨越 arch seam。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AddressSpaceToken(usize);

impl AddressSpaceToken {
    /// @description 从 Sv39 root physical page number 构造 address-space token。
    pub(crate) fn from_root_page(root_page: usize) -> Self {
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

/// @description 激活当前 CPU 的 Sv39 address space 并失效本地 translation cache。
///
/// @param token 由 live page-table root 构造的 token。
pub(crate) fn activate(token: AddressSpaceToken) {
    // SAFETY: token encodes a live Sv39 root and this CPU executes in S-mode; the following
    // fence prevents translations from the previous root surviving activation.
    unsafe {
        riscv::register::satp::write(riscv::register::satp::Satp::from_bits(token.encoded()));
        asm!("sfence.vma");
    }
}

/// @description 失效当前 CPU 的全部 S-stage translations。
pub(crate) fn flush_local() {
    // SAFETY: kernel executes in S-mode and the instruction only affects local TLB state.
    unsafe { asm!("sfence.vma") };
}
