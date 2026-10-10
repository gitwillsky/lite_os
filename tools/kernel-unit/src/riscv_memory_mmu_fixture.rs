// 两个 memory integration consumer 共用的静态 host MMU capability adapter。
// 实际 ASID generation/锁/fence 决策由 address_space_id integration test 单独执行。
pub(crate) use super::{
    page_table::{
        AddressSpaceKind, PageTable as ArchitecturePageTable,
        PageTableEntry as ArchitecturePageTableEntry, PageTableError, TablePage,
    },
    pte::PagePermissions,
};
#[derive(Clone, Copy)]
pub(crate) struct AddressSpaceToken;
#[derive(Clone, Copy)]
pub(crate) struct KernelTrapToken;
impl KernelTrapToken {
    pub(crate) fn from_root_page(_: usize) -> Self {
        Self
    }
}
impl AddressSpaceToken {
    pub(crate) fn from_root_page(_: usize, _: std::sync::Arc<AddressSpaceContext>) -> Self {
        Self
    }
}
pub(crate) struct AddressSpaceContext;
impl AddressSpaceContext {
    pub(crate) fn try_new() -> Result<std::sync::Arc<Self>, ()> {
        Ok(std::sync::Arc::new(Self))
    }
    pub(crate) fn retire_after_global_fence(&self) {}
}
pub(crate) fn activate_kernel(_: KernelTrapToken) {}
pub(crate) fn normalize_physical_address(value: usize) -> usize {
    value
}
pub(crate) fn normalize_physical_page(value: usize) -> usize {
    value
}
pub(crate) fn normalize_virtual_page(value: usize) -> usize {
    value
}
pub(crate) fn canonicalize_virtual_address(value: usize) -> usize {
    value
}
pub(crate) fn virtual_to_physical(value: usize) -> Option<usize> {
    Some(value)
}
pub(crate) fn physical_to_virtual(value: usize) -> usize {
    value
}

// 此 adapter 复用实际 Sv39 Thread context high-half placement，不替代生产 VMA 分配决策。
pub(crate) const THREAD_TRAP_CONTEXT_RANGE: Option<core::ops::Range<usize>> = Some(
    (usize::MAX - (1usize << 38) + 1)..(usize::MAX - 3 * 4096 + 1)
);
