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
    /// 一个设备的消息中断分配；子中断保活底层路由。
    Msi
);
object!(
    /// IOMMU 实例。
    Iommu
);
object!(
    /// 一个 DMA 发起者的总线事务发起者。
    Bti
);
/// 固定记录：析构时显式解除固定并撤销设备地址。
///
/// 只有进程终止等未经析构的关闭会让页进入 BTI 隔离区，直到新驱动复位设备后
/// [`Bti::release_quarantine`]。
#[derive(Debug)]
pub struct Pmt(Handle);

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
    /// - `kind`: MMIO、IRQ、MSI 或 SYSTEM；根 resource 可创建任一种，其余父对象只能细分同类范围。
    /// - `base`/`size`: 非空授权区间；SYSTEM 使用操作编号，MSI 使用消息设备 ID，MMIO 使用页对齐的物理字节。
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
    /// 将本对象的中断路由到一个在线逻辑 CPU，不改变当前屏蔽与 ack 状态。
    ///
    /// # Parameters
    /// - `cpu`: 逻辑 CPU 编号，不是硬件 hart ID 或 MPIDR。
    ///
    /// # Returns
    /// 控制器完成路由配置后返回 unit；已经投递的事件仍属于原中断对象。
    ///
    /// # Errors
    /// 句柄缺少 WRITE、CPU 不存在/未在线、源未绑定或目标资源准备失败。
    /// NO_MEMORY/NO_RESOURCES 不改变旧目标或确认状态；消息迁移提交的硬件超时由内核 fail-stop。
    pub fn set_affinity(&self, cpu: u32) -> Result<(), Status> {
        // SAFETY: 对象句柄保持存活，仅传递标量；权限与在线 CPU 由内核验证。
        check(unsafe {
            invoke(
                call::INTERRUPT_SET_AFFINITY,
                &[self.raw() as usize, cpu as usize],
            )
        })
    }

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
    /// 句柄无效、port 模式尚无待确认通知，或重新预留失败；分配失败保留待确认状态，可重试。
    pub fn ack(&self) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe { invoke(call::INTERRUPT_ACK, &[self.raw() as usize]) })
    }

    /// 绑定到 port：触发合并为一个 `packet::INTERRUPT`；成功接收后必须 ack 才能再次投递。
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

impl Bti {
    /// 释放隔离区中的固定页；调用者已复位设备，设备不再访问上一驱动实例的缓冲。
    ///
    /// # Errors
    ///
    /// 句柄缺少写权限。
    pub fn release_quarantine(&self) -> Result<(), Status> {
        // SAFETY: 无指针参数。
        check(unsafe { invoke(call::BTI_RELEASE_QUARANTINE, &[self.raw() as usize]) })
    }
}

impl Drop for Pmt {
    fn drop(&mut self) {
        let handle = core::mem::replace(&mut self.0, Handle::invalid());
        // SAFETY: 无指针参数；内核随之消费该句柄。失败只可能是句柄无效，没有可恢复动作。
        let _ = unsafe { invoke(call::PMT_UNPIN, &[handle.into_raw() as usize]) };
    }
}

impl Msi {
    /// 为一个设备创建消息中断组，消息地址经该设备 BTI 映射。
    ///
    /// # Parameters
    /// - `resource`: 覆盖 `device` 消息设备 ID 的 WRITE authority。
    /// - `bti`: 同一设备的 DMA 发起者，必须具备 READ | WRITE。
    /// - `device`: 由平台消息映射解析的 ID，不假定等于 IOMMU stream ID。
    /// - `count`: 非零槽数；一个组内每个槽只允许一个存活 Interrupt。
    ///
    /// # Returns
    /// 分配句柄；子 Interrupt 存活期间关闭该句柄不会撤销路由。
    ///
    /// # Errors
    /// 平台不支持、authority 不足、设备已有分配或资源/内存不足。
    pub fn allocate(
        resource: &Resource,
        bti: &Bti,
        device: u32,
        count: u32,
    ) -> Result<Self, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出指针有效；内核验证句柄、范围及设备映射。
        let status = unsafe {
            invoke(
                call::MSI_ALLOCATE,
                &[
                    resource.raw() as usize,
                    bti.raw() as usize,
                    device as usize,
                    count as usize,
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Self)
    }
    /// 从组中创建第 `index` 个消息中断，保活本分配。
    ///
    /// # Parameters
    /// - `table`: MSI-X 表所属的物理 Device VMO，须有 READ | WRITE | MAP。
    /// - `offset`: 表相对 VMO 的字节起点；内核从 index 计算唯一表项。
    ///
    /// # Returns
    /// 初始路由至 boot CPU 的 Interrupt；内核拥有该表项的消息值和 mask，驱动不得再写已绑定表项。
    ///
    /// # Errors
    /// 槽/表区间越界、非物理 Device VMO、已有子中断、权限不足，或内存/目标资源不足。
    pub fn create_interrupt(
        &self,
        index: u32,
        table: &Vmo,
        offset: usize,
    ) -> Result<Interrupt, Status> {
        let mut out = native_abi::HANDLE_INVALID;
        // SAFETY: 输出指针有效；组句柄在调用期间存活。
        let status = unsafe {
            invoke(
                call::MSI_CREATE_INTERRUPT,
                &[
                    self.raw() as usize,
                    index as usize,
                    table.raw() as usize,
                    offset,
                    (&raw mut out) as usize,
                ],
            )
        };
        new_handle(status, out).map(Interrupt)
    }
}
