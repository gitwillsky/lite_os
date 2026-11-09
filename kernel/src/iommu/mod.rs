//! IOMMU：为每个 DMA stream 维护独立设备地址空间。
//!
//! # 分工
//!
//! - 本模块：domain（stream → 设备页表 + IOVA 分配）与 `IommuBackend` 语义；
//! - [`crate::arch::dma::Translation`] 实现：把设备页表绑定到 stream，并完成设备侧 TLB 失效。AArch64 使用 Arm SMMUv3
//!   stage-1（与 CPU 相同的 VMSAv8-64 页表），RISC-V 使用 RISC-V IOMMU first-stage（Sv39）。
//!
//! # 映射与撤销
//!
//! 1. `map`：首次使用某 stream 时创建 domain 并绑定；在 IOVA 空间中首次适配一段连续区间，逐页映射。
//! 2. `unmap`：撤销页表项，硬件失效完成后才释放页表页与 IOVA 区间；调用者（PMT）随后才解除页固定，
//!    设备因此不可能访问已释放的物理页。
//!
//! # 故障记录
//!
//! 硬件把被中止的设备访问写入故障队列（不接中断）。`map`、`unmap` 与 [`IommuBackend::faults`] 在 domain 锁内
//! 排空队列，按 stream 累计；日志只在某 stream 的第 1、2、4、8… 次故障时输出一行，持续越界的设备不会刷屏。
//! 计数只用于诊断，不改变设备或映射状态。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use native_abi::Status;

use crate::execution::sync::TaskMutex;
use crate::memory::{PAGE_SIZE, PhysicalPageNumber};
use crate::vm::{CachePolicy, DeviceAddressSpace};
use fallible_tree::FallibleMap;

/// IOMMU 硬件后端：为一个 stream 建立与撤销设备地址翻译。
pub(crate) trait IommuBackend: Send + Sync {
    /// 把 `pages` 以 `cache` 内存属性映射到该 stream，返回逐页设备地址。
    /// MMIO 消息 doorbell 必须为 Device，不能误设为普通 WB 内存而允许合并/重排。
    ///
    /// # Errors
    /// 设备地址空间耗尽或分配失败。
    fn map(
        &self,
        stream: u32,
        pages: &[PhysicalPageNumber],
        writable: bool,
        cache: CachePolicy,
        output: &mut Vec<u64>,
    ) -> Result<(), Status>;
    /// 撤销映射并在返回前完成设备侧 TLB 失效。
    fn unmap(&self, stream: u32, addresses: &[u64]);
    /// 排空故障队列后返回该 stream 累计的设备访问故障数。
    fn faults(&self, stream: u32) -> u64;
}

/// 无 IOMMU：设备地址等于物理地址。
struct IdentityIommu;

impl IommuBackend for IdentityIommu {
    fn map(
        &self,
        _stream: u32,
        pages: &[PhysicalPageNumber],
        _writable: bool,
        _cache: CachePolicy,
        output: &mut Vec<u64>,
    ) -> Result<(), Status> {
        output
            .try_reserve_exact(pages.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        output.extend(
            pages
                .iter()
                .map(|page| (page.as_usize() * native_abi::PAGE_SIZE) as u64),
        );
        Ok(())
    }

    fn unmap(&self, _stream: u32, _addresses: &[u64]) {}

    fn faults(&self, _stream: u32) -> u64 {
        0
    }
}

use crate::arch::dma::{DmaFrame, Translation as Hardware};

impl DmaFrame for crate::memory::FrameTracker {
    fn allocate_zeroed(pages: usize) -> Option<Self> {
        crate::memory::alloc_contiguous(pages, crate::memory::FrameAllocationClass::Reclaimable)
    }
    fn physical_page(&self) -> usize {
        self.ppn.as_usize()
    }
}

struct Domain {
    id: u16,
    space: DeviceAddressSpace,
    // OWNER: 已分配的 IOVA 区间（起始页 → 页数）；首次适配查找空隙。
    allocated: FallibleMap<usize, usize>,
    // 本 stream 被硬件中止的设备访问数；只在 domain 锁内由排空累加。
    faults: u64,
}

/// 设备地址空间的页范围：零页保留（空设备地址总是故障）。
const FIRST_PAGE: usize = 1;
const END_PAGE: usize = crate::arch::mmu::USER_ADDRESS_END / PAGE_SIZE;

struct Iommu<H> {
    // 字段按声明顺序退休：硬件先停止遍历并释放设备表，domain 根表随后才能释放。
    hardware: H,
    // OWNER: stream → domain 的唯一索引；映射与撤销在同一把锁内修改页表与 IOVA 记录。
    domains: TaskMutex<FallibleMap<u32, Domain>>,
    // OWNER: 单调 domain tag，不回绕或复用；耗尽保持错误，避免旧设备翻译命中新 domain。
    last_domain: AtomicU16,
    // 无法归属 domain 的故障与队列溢出次数；只在 domain 锁内累加，按 2 的幂限速日志。
    unattributed: AtomicU64,
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

    /// 排空硬件故障队列并按 stream 累计；调用者持有 domain 锁。
    fn drain(&self, domains: &mut FallibleMap<u32, Domain>) {
        let unattributed = |what: &str, stream: u32, address: u64| {
            let count = self.unattributed.fetch_add(1, Ordering::Relaxed) + 1;
            if count.is_power_of_two() {
                error!("IOMMU {what}: stream {stream:#x} address {address:#x} (#{count})");
            }
        };
        let overflowed = self.hardware.drain_faults(&mut |stream, address| {
            let Some(domain) = domains.get_mut(&stream) else {
                unattributed("fault on unattached stream", stream, address);
                return;
            };
            domain.faults += 1;
            if domain.faults.is_power_of_two() {
                error!(
                    "IOMMU fault: stream {stream:#x} device address {address:#x} (#{})",
                    domain.faults
                );
            }
        });
        if overflowed {
            unattributed("fault queue overflow", 0, 0);
        }
    }
}

impl<H: Hardware> IommuBackend for Iommu<H> {
    fn map(
        &self,
        stream: u32,
        pages: &[PhysicalPageNumber],
        writable: bool,
        cache: CachePolicy,
        output: &mut Vec<u64>,
    ) -> Result<(), Status> {
        output
            .try_reserve_exact(pages.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut domains = self.domains.lock().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        self.drain(&mut domains);
        if !domains.contains_key(&stream) {
            let id = self
                .last_domain
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                    last.checked_add(1)
                })
                .map_err(|_| native_abi::ERR_NO_RESOURCES)?
                + 1;
            let space = DeviceAddressSpace::create().map_err(|()| native_abi::ERR_NO_MEMORY)?;
            // 所有 fallible storage 必须在硬件取得根表指针前准备；失败不能留下悬空 DC/STE。
            let prepared = FallibleMap::try_prepare(
                stream,
                Domain {
                    id,
                    space,
                    allocated: FallibleMap::new(),
                    faults: 0,
                },
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            self.hardware
                .attach(stream, id, prepared.value().space.root().as_usize())?;
            domains.commit_vacant(prepared);
        }

        let domain = domains.get_mut(&stream).expect("domain inserted above");
        let first = Self::allocate(domain, pages.len()).ok_or(native_abi::ERR_NO_RESOURCES)?;
        let record =
            FallibleMap::try_prepare(first, pages.len()).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        if let Err(retired) = domain.space.map(first, pages, writable, cache) {
            // 已绑定的设备可以遍历任意 IOVA：失败前的部分发布也必须先失效再释放表页/固定页。
            self.hardware.invalidate(domain.id, first, pages.len());
            retired.release();
            return Err(native_abi::ERR_NO_MEMORY);
        }
        // 新映射无需无效→有效的设备 TLB shootdown；先发布 coherent 表写入，再交付地址。
        crate::arch::dma::publish_table_changes();
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
        self.drain(&mut domains);
        let domain = domains
            .get_mut(&stream)
            .expect("unmapped stream has no domain");
        let retired = domain.space.unmap(first, addresses.len());
        self.hardware.invalidate(domain.id, first, addresses.len());
        retired.release();
        domain.allocated.remove(&first);
    }

    fn faults(&self, stream: u32) -> u64 {
        let Ok(mut domains) = self.domains.lock() else {
            return 0;
        };
        self.drain(&mut domains);
        domains.get(&stream).map_or(0, |domain| domain.faults)
    }
}

/// 按描述符创建硬件后端。
///
/// # Errors
/// 类型与当前架构不符、描述符非法或硬件初始化失败。
pub(crate) fn create(kind: u32, descriptor: &[u8]) -> Result<Arc<dyn IommuBackend>, Status> {
    if kind == native_abi::iommu::IDENTITY {
        return Arc::try_new(IdentityIommu)
            .map(|backend| backend as Arc<dyn IommuBackend>)
            .map_err(|_| native_abi::ERR_NO_MEMORY);
    }

    let (base, size) = descriptor_window(descriptor)?;
    let virtual_base = map_registers(base, size)?;
    let hardware =
        crate::arch::dma::Backend::<crate::memory::FrameTracker>::create(kind, virtual_base, size)?;
    let iommu = Arc::try_new(Iommu {
        hardware,
        domains: TaskMutex::new(FallibleMap::new()),
        last_domain: AtomicU16::new(0),
        unattributed: AtomicU64::new(0),
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
        .map_device(base..base.checked_add(size).ok_or(native_abi::ERR_INVALID_ARGS)?)
        .map_err(|()| native_abi::ERR_INVALID_ARGS)
}

#[cfg(test)]
mod tests;
