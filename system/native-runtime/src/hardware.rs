//! 驱动原语：resource、中断、IOMMU、BTI 与固定记录。

use alloc::vec::Vec;

use native_abi::{Status, call};

use crate::handle::object;
use crate::syscall::{check, invoke};
use crate::{Handle, Port, Vmo};

object!(
    /// 不可变访问权（根、MMIO、中断向量或系统操作区间）。
    Resource
);
object!(
    /// 中断对象。
    Interrupt
);
object!(
    /// IOMMU 实例。
    Iommu
);
object!(
    /// 一个 DMA 发起者的总线事务发起者。
    Bti
);
object!(
    /// 固定记录：关闭或 `unpin` 时撤销设备地址。
    Pmt
);

fn new_handle(status: Status, out: native_abi::Handle) -> Result<Handle, Status> {
    check(status)?;
    // SAFETY: 调用成功时内核写入的新句柄由调用者独占。
    Ok(unsafe { Handle::from_raw(out) })
}

impl Resource {
    /// 从父 resource 的同类范围切出不可扩权的子 resource。
    ///
    /// # Parameters
    /// - `parent`: 授权来源，其句柄必须具有 WRITE。
    /// - `kind`: MMIO、IRQ 或 SYSTEM；根 resource 可创建任一种，其余父对象只能细分同类范围。
    /// - `base`/`size`: 非空授权区间；SYSTEM 使用操作编号，MMIO 使用页对齐的物理字节。
    ///
    /// # Returns
    /// 新的独立资源句柄；父对象的范围不变。
    ///
    /// # Errors
    /// 越过父范围、跨 kind 或覆盖 RAM 返回 ACCESS_DENIED；空/溢出/非法 kind 返回 INVALID_ARGS；
    /// 对象或句柄分配失败返回 NO_MEMORY。
    pub fn create(parent: &Self, kind: u32, base: u64, size: u64) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::RESOURCE_CREATE,
                &[
                    parent.raw() as usize,
                    kind as usize,
                    base as usize,
                    size as usize,
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Self)
    }
}

impl Interrupt {
    /// 创建中断对象。
    ///
    /// # Errors
    ///
    /// resource 不覆盖该向量或向量已被占用。
    pub fn create(resource: &Resource, vector: u32, options: u32) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::INTERRUPT_CREATE,
                &[
                    resource.raw() as usize,
                    vector as usize,
                    options as usize,
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Self)
    }

    /// 等待触发，返回时间戳。
    ///
    /// # Errors
    ///
    /// `ERR_TIMED_OUT`、`ERR_CANCELED`。
    pub fn wait(&self, deadline: i64) -> Result<i64, Status> {
        let mut timestamp = 0i64;
        // SAFETY: 输出位置有效。
        check(unsafe {
            invoke(
                call::INTERRUPT_WAIT,
                &[
                    self.raw() as usize,
                    deadline as usize,
                    (&raw mut timestamp) as usize,
                ],
            )
        })?;
        Ok(timestamp)
    }

    /// 确认并重新允许触发。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn ack(&self) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe { invoke(call::INTERRUPT_ACK, &[self.raw() as usize]) })
    }

    /// 绑定到 port：每次触发投递 `packet::INTERRUPT`。
    ///
    /// # Errors
    ///
    /// 已绑定。
    pub fn bind(&self, port: &Port, key: u64) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe {
            invoke(
                call::INTERRUPT_BIND,
                &[self.raw() as usize, port.raw() as usize, key as usize],
            )
        })
    }
}

impl Iommu {
    /// 创建 IOMMU 对象；`descriptor` 的布局由 `kind` 决定。
    ///
    /// # Errors
    ///
    /// 类型与架构不符或硬件初始化失败。
    pub fn create(resource: &Resource, kind: u32, descriptor: &[u8]) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 描述符与输出位置有效。
        let status = unsafe {
            invoke(
                call::IOMMU_CREATE,
                &[
                    resource.raw() as usize,
                    kind as usize,
                    descriptor.as_ptr() as usize,
                    descriptor.len(),
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Self)
    }
}

impl Bti {
    /// 为 `stream_id` 创建 BTI。
    ///
    /// # Errors
    ///
    /// 内存不足。
    pub fn create(iommu: &Iommu, stream_id: u32) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出位置有效。
        let status = unsafe {
            invoke(
                call::BTI_CREATE,
                &[
                    iommu.raw() as usize,
                    stream_id as usize,
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Self)
    }

    /// 固定 VMO 区间，返回每页设备地址与固定记录。
    ///
    /// # Errors
    ///
    /// 区间未对齐、越界或 IOVA 空间耗尽。
    pub fn pin(
        &self,
        options: u32,
        vmo: &Vmo,
        offset: u64,
        length: u64,
    ) -> Result<(Vec<u64>, Pmt), Status> {
        let pages = (length as usize).div_ceil(native_abi::PAGE_SIZE);
        let mut addresses = alloc::vec![0u64; pages];
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 地址数组容量为 `pages`，输出位置有效。
        let status = unsafe {
            crate::syscall::call(
                call::BTI_PIN,
                [
                    self.raw() as usize,
                    options as usize,
                    vmo.raw() as usize,
                    offset as usize,
                    length as usize,
                    addresses.as_mut_ptr() as usize,
                    (&raw mut out) as usize,
                ],
            )
        };
        let pmt = new_handle(status, out)?;
        Ok((addresses, Pmt(pmt)))
    }
}

impl Pmt {
    /// 解除固定并撤销设备地址。
    ///
    /// # Errors
    ///
    /// 句柄无效。
    pub fn unpin(self) -> Result<(), Status> {
        // SAFETY: 无指针参数；内核随之消费该句柄。
        check(unsafe { invoke(call::PMT_UNPIN, &[self.into_handle().into_raw() as usize]) })
    }
}
