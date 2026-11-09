//! 消息中断组与子 Interrupt 的共同寿命；底层路由和硬件表属于 platform，设备 doorbell 地址经 BTI 映射。
use super::{Bti, Interrupt};
use crate::execution::sync::TaskMutex;
use crate::object::{KernelObject, next_koid};
use alloc::{sync::Arc, vec::Vec};
use native_abi::{ObjectType, Status, rights};
use spin::Mutex;

mod source;
pub(super) use source::Source;

/// 原生消息中断分配；子中断保活本对象，关闭分配句柄不提前释放任何路由或 IOVA。
pub(crate) struct Msi {
    koid: u64,
    group: crate::platform::MessageInterruptGroup,
    bti: Arc<Bti>,
    // OWNER: 本组访问过的物理 doorbell 页→唯一 BTI 映射；CPU 专属地址不复制成固定组地址。
    // 冷锁串行首次映射与命中查找，硬 IRQ 不访问；先预留 Vec/映射，再允许源事务修改硬件。
    // 映射保留至 group 退休，迁回旧 CPU 直接重用，避免每次 affinity 变化增减 IOMMU 页表。
    // 同组多个子中断共享这些 owner；缺失时一次配置返回就可能撤销设备消息仍引用的 IOVA。
    doorbells: TaskMutex<Vec<Doorbell>>,
    // OWNER: 每槽唯一子对象的登记；同锁 claim/rollback，避免两个 Interrupt 同时绑定一条消息。
    issued: Mutex<Vec<bool>>,
}
impl Msi {
    /// 按设备 authority 分配 count 个消息槽；doorbell 按实际目标页惰性映射，错误不交付半完成对象。
    /// # Errors
    /// 不支持、请求非法、已有组、资源/内存不足或 IOMMU 映射失败。
    pub(crate) fn create(bti: Arc<Bti>, device: u32, count: u32) -> Result<Self, Status> {
        if count == 0 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let group =
            crate::platform::MessageInterruptGroup::allocate(device, count, allocate_table)?;
        let mut issued = Vec::new();
        issued
            .try_reserve_exact(group.count() as usize)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        issued.resize(group.count() as usize, false);
        Ok(Self {
            koid: next_koid(),
            group,
            bti,
            doorbells: TaskMutex::new(Vec::new()),
            issued: Mutex::new(issued),
        })
    }

    /// 在任何设备表项修改之前取得目标设备地址；OOM 不改变当前消息或路由。
    fn map_doorbell(&self, physical: u64) -> Result<u64, Status> {
        let page = (physical / crate::memory::PAGE_SIZE as u64) as usize;
        let offset = physical % crate::memory::PAGE_SIZE as u64;
        let mut mappings = self
            .doorbells
            .lock()
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        if let Some(mapping) = mappings.iter().find(|mapping| mapping.page == page) {
            return Ok(mapping.addresses[0] + offset);
        }
        mappings
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut addresses = Vec::new();
        self.bti.backend.map(
            self.bti.stream,
            &[crate::memory::PhysicalPageNumber::from(page)],
            true,
            crate::vm::CachePolicy::Device,
            &mut addresses,
        )?;
        let address = addresses[0] + offset;
        mappings.push(Doorbell {
            bti: self.bti.clone(),
            page,
            addresses,
        });
        Ok(address)
    }
    /// 返回分配容量；目标仅供内核编程，驱动不能复制路由状态。
    pub(crate) fn info(&self) -> native_abi::MsiInfo {
        native_abi::MsiInfo {
            count: self.group.count(),
        }
    }
    /// 创建一个独占 index 的 Interrupt。失败回滚该槽，成功由子对象保活整个分配。
    pub(crate) fn interrupt(
        self: Arc<Self>,
        index: u32,
        table: &crate::vm::Vmo,
        offset: usize,
    ) -> Result<Interrupt, Status> {
        let vector = self
            .group
            .vector(index)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let mut issued = self.issued.lock();
        let slot = &mut issued[index as usize];
        if *slot {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        *slot = true;
        drop(issued);
        // 先独占登记，再接触表项；失败用同一 guard 归还，不能由重复创建者屏蔽存活的源。
        let registration = Registration { owner: self, index };
        let offset = (index as usize)
            .checked_mul(pci_config::msix_table::ENTRY_BYTES)
            .and_then(|index| offset.checked_add(index))
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let source = {
            // 重用同组槽也必须恢复工厂的 boot CPU 初始契约，不能继承上一子对象的 affinity。
            // 新源尚未发表：准备/映射/Source 构造均可失败，提交时设备表项仍被 mask。
            let mut route = registration
                .owner
                .group
                .prepare_affinity(index, crate::execution::cpu::boot_id())?;
            let (physical, data) = route.message();
            let address = registration.owner.map_doorbell(physical)?;
            let source = Source::prepare(table, offset, address, data)?;
            route.commit(crate::execution::barrier::complete_pending);
            source
        };
        // 释放平台冷令牌后才绑定控制器；不引入 cold state → GIC 的锁序。
        Interrupt::create_message(vector, registration, source)
    }
}
/// 子 Interrupt 的一次性登记；必须在 platform unbind 后才归还 index。
pub(super) struct Registration {
    owner: Arc<Msi>,
    index: u32,
}
impl Registration {
    /// 准备新控制器目标与 BTI 映射后，再执行唯一的源/路由提交；所有可恢复失败在屏蔽源之前。
    pub(super) fn set_affinity(
        &self,
        source: &Source,
        target: crate::execution::cpu::CpuId,
    ) -> Result<(), Status> {
        {
            // route 的冷锁持续覆盖 Source 恢复；不能让下一事务看见仍 Updating 的同一源。
            let mut route: crate::platform::MessageInterruptRoute<'_> =
                self.owner.group.prepare_affinity(self.index, target)?;
            let (physical, data) = route.message();
            let address = self.owner.map_doorbell(physical)?;
            source.retarget(address, data, || {
                route.commit(crate::execution::barrier::complete_pending)
            });
        }
        Ok(())
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.owner.issued.lock()[self.index as usize] = false;
    }
}
// OWNER: 独占的 BTI 映射；声明在 group 后，Rust 字段析构顺序保证路由撤销后才撤销 IOVA。
// 驱动必须先停止设备消息源；子 Interrupt 保活 Msi，故 group 析构时已没有绑定。
struct Doorbell {
    bti: Arc<Bti>,
    page: usize,
    addresses: Vec<u64>,
}
impl Drop for Doorbell {
    fn drop(&mut self) {
        self.bti.backend.unmap(self.bti.stream, &self.addresses);
    }
}
impl KernelObject for Msi {
    fn object_type(&self) -> ObjectType {
        ObjectType::Msi
    }
    fn koid(&self) -> u64 {
        self.koid
    }
    fn default_rights(&self) -> u32 {
        rights::DUPLICATE | rights::TRANSFER | rights::INSPECT | rights::WRITE
    }
}

/// 分配策略与真实页 owner 留在对象领域；平台只持有 trait object 以执行最终析构。
fn allocate_table(pages: usize) -> Option<crate::platform::MessageInterruptMemory> {
    let frame =
        crate::memory::alloc_contiguous(pages, crate::memory::FrameAllocationClass::Reclaimable)?;
    alloc::boxed::Box::try_new(frame)
        .ok()
        .map(|frame| frame as crate::platform::MessageInterruptMemory)
}
