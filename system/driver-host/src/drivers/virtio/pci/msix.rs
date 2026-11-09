//! PCI MSI-X 功能开关与未绑定表项初始化；已绑定表项由原生 Interrupt 独占。
use mmio::{BusError, MmioBus};
use native_runtime::{Interrupt, Msi, Vmo, native_abi};
use pci_config::msix_table::{CONTROL, ENTRY_BYTES, MASK};

const FUNCTION_MASK: u16 = 1 << 14;
const ENABLE: u16 = 1 << 15;
const INTX_DISABLE: u16 = 1 << 10;

/// 已验证的消息表与配置窗口；function-mask 后才可初始化未绑定表项。
pub(in crate::drivers::virtio) struct Msix {
    config: MmioBus,
    control: usize,
    table: MmioBus,
    table_vmo: Vmo,
    table_offset: usize,
    entries: u16,
}
impl Msix {
    /// 取得寄存器窗口后立即阻止遗留消息与 INTx；没有绑定 native 槽时不能向旧地址发消息。
    /// # Errors
    /// 窗口不完整、表大小不匹配或访问失败返回 BusError。
    pub(in crate::drivers::virtio) fn new(
        config: MmioBus,
        control: usize,
        table: MmioBus,
        table_vmo: Vmo,
        table_offset: usize,
        entries: u16,
    ) -> Result<Self, BusError> {
        config.subwindow(control, 2)?;
        if !(1..=2048).contains(&entries) {
            return Err(BusError::InvalidAddress);
        }
        table.subwindow(0, usize::from(entries) * ENTRY_BYTES)?;
        let msix = Self {
            config,
            control,
            table,
            table_vmo,
            table_offset,
            entries,
        };
        msix.disable()?;
        Ok(msix)
    }
    pub(in crate::drivers::virtio) fn capacity(&self) -> u16 {
        self.entries
    }
    /// 先禁 INTx、再 function-mask/disable 并读回，阻止旧源；设备 reset 后才可归还 native 路由。
    pub(in crate::drivers::virtio) fn disable(&self) -> Result<(), BusError> {
        self.config
            .write_u16(4, self.config.read_u16(4)? | INTX_DISABLE)?;
        self.config.write_u16(
            self.control,
            (self.config.read_u16(self.control)? | FUNCTION_MASK) & !ENABLE,
        )?;
        let control = self.config.read_u16(self.control)?;
        if control & (FUNCTION_MASK | ENABLE) != FUNCTION_MASK {
            return Err(BusError::InvalidAddress);
        }
        Ok(())
    }
    /// 每次新会话先屏蔽全部表项；本次未使用的旧槽不能在 enable 时重新发送 pending 消息。
    pub(in crate::drivers::virtio) fn initialize(&self) -> Result<(), BusError> {
        self.disable()?;
        for index in 0..self.entries {
            let offset = usize::from(index) * ENTRY_BYTES + CONTROL;
            self.table
                .write_u32(offset, self.table.read_u32(offset)? | MASK)?;
        }
        self.table.read_u32(CONTROL)?;
        Ok(())
    }
    /// 委托 native 对象安装独占表项；驱动不保留或重写消息目标，不跨层同步路由状态。
    pub(in crate::drivers::virtio) fn create_interrupt(
        &self,
        group: &Msi,
        index: u32,
    ) -> Result<Interrupt, native_abi::Status> {
        if index >= u32::from(self.entries) {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        group.create_interrupt(index, &self.table_vmo, self.table_offset)
    }
    /// 全部 native binding、队列向量和表项已准备，最后开启消息源；调用者随后才 DRIVER_OK。
    pub(in crate::drivers::virtio) fn enable(&self) -> Result<(), BusError> {
        self.config.write_u16(
            self.control,
            (self.config.read_u16(self.control)? | ENABLE) & !FUNCTION_MASK,
        )?;
        if self.config.read_u16(self.control)? & (ENABLE | FUNCTION_MASK) != ENABLE {
            return Err(BusError::InvalidAddress);
        }
        Ok(())
    }
}
