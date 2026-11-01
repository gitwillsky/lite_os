//! IOMMU：为每个 DMA stream 维护独立设备地址空间。
//!
//! # 分工
//!
//! - 本模块：domain（stream → 设备页表 + IOVA 分配）与 `IommuBackend` 语义；
//! - [`Hardware`] 实现：把设备页表绑定到 stream，并完成设备侧 TLB 失效。AArch64 使用 Arm SMMUv3
//!   stage-1（与 CPU 相同的 VMSAv8-64 页表），RISC-V 使用 RISC-V IOMMU first-stage（Sv39）。
//!
//! # 映射与撤销
//!
//! 1. `map`：首次使用某 stream 时创建 domain 并绑定；在 IOVA 空间中首次适配一段连续区间，逐页映射。
//! 2. `unmap`：撤销页表项，硬件失效完成后才释放页表页与 IOVA 区间；调用者（PMT）随后才解除页固定，
//!    设备因此不可能访问已释放的物理页。

#[cfg(target_arch = "riscv64")]
mod riscv;
#[cfg(target_arch = "aarch64")]
mod smmu_v3;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};

use native_abi::Status;

use crate::execution::sync::TaskMutex;
use crate::fallible_tree::FallibleMap;
use crate::memory::{PAGE_SIZE, PhysicalPageNumber};
use crate::object::IommuBackend;
use crate::vm::DeviceAddressSpace;

/// IOMMU 硬件：绑定 stream 与失效翻译。
trait Hardware: Send + Sync {
    /// 把以 `root` 为根的设备页表绑定到 `stream`，`domain` 是硬件 TLB 标签（ASID/PSCID）。
    ///
    /// # Errors
    /// stream 超出硬件支持范围或分配失败。
    fn attach(&self, stream: u32, domain: u16, root: PhysicalPageNumber) -> Result<(), Status>;
    /// 失效 `domain` 中 `[first_page, first_page + pages)` 的翻译，返回前完成。
    fn invalidate(&self, domain: u16, first_page: usize, pages: usize);
}

struct Domain {
    id: u16,
    space: DeviceAddressSpace,
    // OWNER: 已分配的 IOVA 区间（起始页 → 页数）；首次适配查找空隙。
    allocated: FallibleMap<usize, usize>,
}

/// 设备地址空间的页范围：零页保留（空设备地址总是故障）。
const FIRST_PAGE: usize = 1;
const END_PAGE: usize = crate::arch::mmu::USER_ADDRESS_END / PAGE_SIZE;

struct Iommu<H> {
    hardware: H,
    // OWNER: stream → domain 的唯一索引；映射与撤销在同一把锁内修改页表与 IOVA 记录。
    domains: TaskMutex<FallibleMap<u32, Domain>>,
    next_domain: AtomicU16,
}

impl<H: Hardware> Iommu<H> {
    fn allocate(domain: &Domain, count: usize) -> Option<usize> {
        let mut cursor = FIRST_PAGE;
        for (&start, &length) in domain.allocated.iter() {
            if start - cursor >= count {
                return Some(cursor);
            }
            cursor = start + length;
        }
        (END_PAGE - cursor >= count).then_some(cursor)
    }
}

impl<H: Hardware> IommuBackend for Iommu<H> {
    fn map(&self, stream: u32, pages: &[PhysicalPageNumber], writable: bool, output: &mut Vec<u64>) -> Result<(), Status> {
        output.try_reserve_exact(pages.len()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut domains = self.domains.lock().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        if !domains.contains_key(&stream) {
            let id = self.next_domain.fetch_add(1, Ordering::Relaxed);
            if id == 0 {
                return Err(native_abi::ERR_NO_RESOURCES);
            }
            let space = DeviceAddressSpace::create().map_err(crate::object::vm_status)?;
            self.hardware.attach(stream, id, space.root())?;
            domains
                .try_insert(
                    stream,
                    Domain {
                        id,
                        space,
                        allocated: FallibleMap::new(),
                    },
                )
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        }
        let domain = domains.get_mut(&stream).expect("domain inserted above");
        let first = Self::allocate(domain, pages.len()).ok_or(native_abi::ERR_NO_RESOURCES)?;
        let record = FallibleMap::try_prepare(first, pages.len()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        domain
            .space
            .map(first, pages, writable)
            .map_err(crate::object::vm_status)?;
        domain.allocated.commit_vacant(record);
        output.extend((first..first + pages.len()).map(|page| (page * PAGE_SIZE) as u64));
        Ok(())
    }

    fn unmap(&self, stream: u32, addresses: &[u64]) {
        let Some(&first) = addresses.first() else {
            return;
        };
        let first = first as usize / PAGE_SIZE;
        let mut domains = self
            .domains
            .lock()
            .unwrap_or_else(|_| panic!("IOMMU unmap cannot allocate a waiter"));
        let domain = domains.get_mut(&stream).expect("unmapped stream has no domain");
        let retired = domain.space.unmap(first, addresses.len());
        self.hardware.invalidate(domain.id, first, addresses.len());
        retired.release();
        domain.allocated.remove(&first);
    }
}

/// 按描述符创建硬件后端。
///
/// # Errors
/// 类型与当前架构不符、描述符非法或硬件初始化失败。
pub(crate) fn create(kind: u32, descriptor: &[u8]) -> Result<Arc<dyn IommuBackend>, Status> {
    #[cfg(target_arch = "aarch64")]
    let hardware = match kind {
        native_abi::iommu::ARM_SMMU_V3 => smmu_v3::SmmuV3::create(descriptor)?,
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    };
    #[cfg(target_arch = "riscv64")]
    let hardware = match kind {
        native_abi::iommu::RISCV => riscv::RiscvIommu::create(descriptor)?,
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    };
    let iommu = Arc::try_new(Iommu {
        hardware,
        domains: TaskMutex::new(FallibleMap::new()),
        next_domain: AtomicU16::new(1),
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    Ok(iommu)
}

/// 从描述符字节读出 `(base, size)`。
fn descriptor_window(descriptor: &[u8]) -> Result<(usize, usize), Status> {
    if descriptor.len() != 16 {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let base = u64::from_le_bytes(descriptor[..8].try_into().expect("eight bytes")) as usize;
    let size = u64::from_le_bytes(descriptor[8..].try_into().expect("eight bytes")) as usize;
    if size == 0 || !base.is_multiple_of(PAGE_SIZE) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok((base, size))
}

/// 映射寄存器窗口并返回其内核虚拟地址。
fn map_registers(base: usize, size: usize) -> Result<usize, Status> {
    crate::vm::KERNEL_SPACE
        .wait()
        .lock()
        .map_device(base..base + size)
        .map_err(|()| native_abi::ERR_INVALID_ARGS)
}
