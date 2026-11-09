//! Device translation mechanism; allocation and mapping policy stay with the caller.
use native_abi::Status;

#[cfg(target_arch = "aarch64")]
#[path = "aarch64/iommu.rs"]
mod backend;
#[cfg(target_arch = "riscv64")]
#[path = "riscv64/iommu.rs"]
mod backend;
pub(crate) use backend::Backend;

/// A zeroed, physically contiguous allocation retained for hardware table/queue access.
/// Implementations must keep every page resident and stable until the frame owner is dropped.
pub(crate) trait DmaFrame: Send + Sync {
    /// Allocate `pages` zeroed pages; returns `None` when allocation fails.
    fn allocate_zeroed(pages: usize) -> Option<Self>
    where
        Self: Sized;
    /// First physical page number, stable for this allocation's entire lifetime.
    fn physical_page(&self) -> usize;
}

/// Device page-table binding and synchronous translation invalidation.
pub(crate) trait Translation: Send + Sync {
    /// Bind `stream` to `root` (physical page number), tagged by `domain`.
    /// Returns an error before publication for unsupported IDs or allocation failure.
    /// After the hardware accepts the root pointer, failure must be fail-stop to retain its owner.
    fn attach(&self, stream: u32, domain: u16, root: usize) -> Result<(), Status>;
    /// Invalidate the page range; completion precedes return and frame retirement.
    fn invalidate(&self, domain: u16, first_page: usize, pages: usize);
    /// Drain recorded translation faults, calling `record(stream, device_address)` for each.
    /// Returns `true` when the hardware queue overflowed since the previous drain (records lost).
    fn drain_faults(&self, record: &mut dyn FnMut(u32, u64)) -> bool;
}

/// Publish coherent device page-table stores before returning IOVAs to a DMA producer.
/// No invalid-to-valid TLB invalidation is needed; the architecture still requires store ordering.
pub(crate) fn publish_table_changes() {
    mmio::before_write();
}

/// Arm 消息中断表在发出 cache invalidation 前需要 coherent store completion。
#[cfg(target_arch = "aarch64")]
pub(crate) fn complete_table_writes() {
    backend::complete_table_writes();
}
