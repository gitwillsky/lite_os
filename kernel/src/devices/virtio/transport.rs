use super::pci::PciTransport;
use crate::drivers::{BusError, MmioBus};

const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const CONFIG_GENERATION: usize = 0x0fc;
const CONFIG: usize = 0x100;

pub(in crate::virtio) const VIRTIO_CONFIG_S_ACKNOWLEDGE: u32 = 1;
pub(in crate::virtio) const VIRTIO_CONFIG_S_DRIVER: u32 = 2;
pub(in crate::virtio) const VIRTIO_CONFIG_S_DRIVER_OK: u32 = 4;
pub(in crate::virtio) const VIRTIO_CONFIG_S_FEATURES_OK: u32 = 8;
pub(in crate::virtio) const VIRTIO_F_VERSION_1: u64 = 1 << 32;
pub(in crate::virtio) const VIRTIO_MMIO_INT_VRING: u32 = 1;
pub(in crate::virtio) const VIRTIO_MMIO_INT_CONFIG: u32 = 2;

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;

/// VirtIO MMIO v2 split-queue 的三段物理地址。
#[derive(Clone, Copy)]
pub(in crate::virtio) struct VirtQueueAddresses {
    pub(in crate::virtio) descriptor: u64,
    pub(in crate::virtio) driver: u64,
    pub(in crate::virtio) device: u64,
}

/// 为 VirtIO MMIO v2 设备提供 feature、queue 与 config 事务接口。
pub(in crate::virtio) struct VirtIODevice {
    transport: Transport,
    device_id: u32,
}

enum Transport {
    Mmio(MmioBus),
    #[allow(
        dead_code,
        reason = "PCI transport is constructed only by platform backends that discover PCI"
    )]
    Pci(PciTransport),
}

impl VirtIODevice {
    /// 创建并识别一个 VirtIO MMIO 设备。
    ///
    /// # Parameters
    ///
    /// - `base_addr`: MMIO 基址。
    /// - `size`: MMIO 窗口长度。
    ///
    /// # Returns
    ///
    /// 成功时返回包含 device ID 的寄存访问器。
    ///
    /// # Errors
    ///
    /// MMIO 区间无效或读取 device ID 失败时返回 `BusError`。
    pub(in crate::virtio) fn new(base_addr: usize, size: usize) -> Result<Self, BusError> {
        let bus = MmioBus::new(base_addr, size)?;
        let device_id = bus.read_u32(DEVICE_ID)?;
        Ok(Self {
            transport: Transport::Mmio(bus),
            device_id,
        })
    }

    /// Wrap a capability-validated modern VirtIO PCI function.
    ///
    /// # Parameters
    ///
    /// - `device_id`: VirtIO device type derived from the PCI identity.
    /// - `transport`: Common/notify/ISR capability owner.
    ///
    /// # Returns
    ///
    /// A transport-neutral device consumed by the existing adapter.
    #[allow(
        dead_code,
        reason = "PCI transport is constructed only by platform backends that discover PCI"
    )]
    pub(crate) fn from_pci(device_id: u32, transport: PciTransport) -> Self {
        Self {
            transport: Transport::Pci(transport),
            device_id,
        }
    }

    pub(in crate::virtio) fn device_id(&self) -> u32 {
        self.device_id
    }

    pub(in crate::virtio) fn initialize(&mut self) -> Result<(), BusError> {
        match &self.transport {
            Transport::Mmio(bus) => {
                let magic = bus.read_u32(MAGIC)?;
                let version = bus.read_u32(VERSION)?;
                if magic != VIRTIO_MMIO_MAGIC || version != 2 || self.device_id == 0 {
                    return Err(BusError::InvalidAddress);
                }
            }
            Transport::Pci(pci) => pci.initialize()?,
        }
        self.reset()?;
        self.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE)?;
        self.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER)
    }

    /// 发起 device reset，并等待 transport 读回完成状态。
    ///
    /// # Returns
    ///
    /// device status 已为 0、queue 不再 live 时返回 unit。
    ///
    /// # Errors
    ///
    /// MMIO window 无效时返回 `InvalidAddress`；device 不完成 reset 时保活 DMA 并等待。
    pub(in crate::virtio) fn reset(&self) -> Result<(), BusError> {
        self.set_status(0)?;
        while self.get_status()? != 0 {
            core::hint::spin_loop();
        }
        Ok(())
    }

    /// 以 low/high selector 发布完整 64-bit driver feature set。
    ///
    /// # Parameters
    ///
    /// - `features`: 已与 device feature 相交且必须含 `VIRTIO_F_VERSION_1`。
    ///
    /// # Returns
    ///
    /// 两个 feature word 全部写入后返回 unit。
    ///
    /// # Errors
    ///
    /// 缺少 version feature 或 MMIO 访问失败返回 `InvalidAddress`。
    pub(in crate::virtio) fn set_driver_features(&self, features: u64) -> Result<(), BusError> {
        if features & VIRTIO_F_VERSION_1 == 0 {
            return Err(BusError::InvalidAddress);
        }
        match &self.transport {
            Transport::Mmio(bus) => {
                bus.write_u32(DRIVER_FEATURES_SEL, 0)?;
                bus.write_u32(DRIVER_FEATURES, features as u32)?;
                bus.write_u32(DRIVER_FEATURES_SEL, 1)?;
                bus.write_u32(DRIVER_FEATURES, (features >> 32) as u32)
            }
            Transport::Pci(pci) => pci.set_driver_features(features),
        }
    }

    /// 以 low/high selector 读取完整 64-bit device feature set。
    ///
    /// # Returns
    ///
    /// device 发布的全部 feature bits。
    ///
    /// # Errors
    ///
    /// MMIO 访问失败返回 `InvalidAddress`。
    pub(in crate::virtio) fn device_features(&self) -> Result<u64, BusError> {
        match &self.transport {
            Transport::Mmio(bus) => {
                bus.write_u32(DEVICE_FEATURES_SEL, 0)?;
                let low = bus.read_u32(DEVICE_FEATURES)?;
                bus.write_u32(DEVICE_FEATURES_SEL, 1)?;
                let high = bus.read_u32(DEVICE_FEATURES)?;
                Ok(u64::from(low) | u64::from(high) << 32)
            }
            Transport::Pci(pci) => pci.device_features(),
        }
    }

    pub(in crate::virtio) fn set_status(&self, status: u32) -> Result<(), BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.write_u32(STATUS, status),
            Transport::Pci(pci) => pci.set_status(status),
        }
    }

    pub(in crate::virtio) fn get_status(&self) -> Result<u32, BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.read_u32(STATUS),
            Transport::Pci(pci) => pci.get_status(),
        }
    }

    /// 读取一个尚未发布 queue 的最大长度。
    ///
    /// # Parameters
    ///
    /// - `index`: device-defined queue index。
    ///
    /// # Returns
    ///
    /// 非零且可由 `u16` 表达的最大 descriptor 数。
    ///
    /// # Errors
    ///
    /// queue 不存在、已 ready 或 MMIO 失败返回 `InvalidAddress`。
    pub(in crate::virtio) fn queue_max_size(&self, index: u32) -> Result<u16, BusError> {
        match &self.transport {
            Transport::Pci(pci) => pci.queue_max_size(index),
            Transport::Mmio(bus) => {
                bus.write_u32(QUEUE_SEL, index)?;
                let maximum = bus.read_u32(QUEUE_NUM_MAX)?;
                if maximum == 0 || bus.read_u32(QUEUE_READY)? != 0 {
                    return Err(BusError::InvalidAddress);
                }
                u16::try_from(maximum).map_err(|_| BusError::InvalidAddress)
            }
        }
    }

    /// 选择并发布一个 MMIO v2 split virtqueue。
    ///
    /// # Parameters
    ///
    /// - `index`: device-defined queue index。
    /// - `requested`: driver 选择的二次幂 queue size。
    /// - `addresses`: descriptor、available 和 used ring 的物理基址。
    ///
    /// # Returns
    ///
    /// device 接受 queue size 并完成 ready publication 后返回 unit。
    ///
    /// # Errors
    ///
    /// queue 不存在、已 ready、size 无效或 MMIO 失败返回 `InvalidAddress`。
    pub(in crate::virtio) fn configure_queue(
        &self,
        index: u32,
        requested: u16,
        addresses: VirtQueueAddresses,
    ) -> Result<(), BusError> {
        if let Transport::Pci(pci) = &self.transport {
            return pci.configure_queue(index, requested, addresses);
        }
        let bus = match &self.transport {
            Transport::Mmio(bus) => bus,
            Transport::Pci(_) => unreachable!(),
        };
        bus.write_u32(QUEUE_SEL, index)?;
        let maximum = bus.read_u32(QUEUE_NUM_MAX)?;
        if maximum == 0
            || u32::from(requested) > maximum
            || !requested.is_power_of_two()
            || bus.read_u32(QUEUE_READY)? != 0
        {
            return Err(BusError::InvalidAddress);
        }
        bus.write_u32(QUEUE_NUM, u32::from(requested))?;
        self.write_address(QUEUE_DESC_LOW, addresses.descriptor)?;
        self.write_address(QUEUE_DRIVER_LOW, addresses.driver)?;
        self.write_address(QUEUE_DEVICE_LOW, addresses.device)?;
        bus.write_u32(QUEUE_READY, 1)
    }

    pub(in crate::virtio) fn notify_queue(&self, queue: u32) -> Result<(), BusError> {
        // RISC-V does not order normal memory against MMIO.  The available-index Release store
        // publishes descriptors to memory; this edge makes that index visible before the
        // device observes its queue doorbell.
        match &self.transport {
            Transport::Mmio(bus) => {
                crate::drivers::before_mmio_write();
                bus.write_u32(QUEUE_NOTIFY, queue)
            }
            Transport::Pci(pci) => pci.notify_queue(queue),
        }
    }

    pub(in crate::virtio) fn interrupt_status(&self) -> Result<u32, BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.read_u32(INTERRUPT_STATUS),
            Transport::Pci(pci) => pci.interrupt_status(),
        }
    }

    pub(in crate::virtio) fn interrupt_ack(&self, interrupt: u32) -> Result<(), BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.write_u32(INTERRUPT_ACK, interrupt),
            // Reading the PCI ISR capability atomically consumes the INTx reason.
            Transport::Pci(_) => Ok(()),
        }
    }

    pub(in crate::virtio) fn read_config_u64(&self, offset: usize) -> Result<u64, BusError> {
        for _ in 0..4 {
            let before = self.config_generation()?;
            let low = self.read_config_u32(offset)?;
            let high = self.read_config_u32(offset + 4)?;
            let after = self.config_generation()?;
            if before == after {
                return Ok(u64::from(low) | u64::from(high) << 32);
            }
        }
        Err(BusError::InvalidAddress)
    }

    /// 读取 device-specific config 的单个 little-endian u32。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 device config 起点的 byte offset。
    ///
    /// # Returns
    ///
    /// volatile 读取值。
    ///
    /// # Errors
    ///
    /// offset 超出 MMIO window 返回 `InvalidAddress`。
    pub(in crate::virtio) fn read_config_u32(&self, offset: usize) -> Result<u32, BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.read_u32(CONFIG + offset),
            Transport::Pci(pci) => pci.read_config_u32(offset),
        }
    }

    /// 写入 device-specific config 的单个 little-endian u32。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 device config 起点的 byte offset。
    /// - `value`: 由具体 device protocol 定义的值。
    ///
    /// # Returns
    ///
    /// 写入成功返回 unit。
    ///
    /// # Errors
    ///
    /// offset 超出 MMIO window 返回 `InvalidAddress`。
    pub(in crate::virtio) fn write_config_u32(
        &self,
        offset: usize,
        value: u32,
    ) -> Result<(), BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.write_u32(CONFIG + offset, value),
            Transport::Pci(pci) => pci.write_config_u32(offset, value),
        }
    }

    /// 读取 device-specific config 的单个 byte。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 device config 起点的 byte offset。
    ///
    /// # Returns
    ///
    /// volatile 读取值。
    ///
    /// # Errors
    ///
    /// offset 超出 MMIO window 返回 `InvalidAddress`。
    pub(in crate::virtio) fn read_config_u8(&self, offset: usize) -> Result<u8, BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.read_u8(CONFIG + offset),
            Transport::Pci(pci) => pci.read_config_u8(offset),
        }
    }

    /// 写入 device-specific config 的单个 byte。
    ///
    /// # Parameters
    ///
    /// - `offset`: 相对 device config 起点的 byte offset。
    /// - `value`: 由具体 device protocol 定义的值。
    ///
    /// # Returns
    ///
    /// 写入成功返回 unit。
    ///
    /// # Errors
    ///
    /// offset 超出 MMIO window 返回 `InvalidAddress`。
    pub(in crate::virtio) fn write_config_u8(
        &self,
        offset: usize,
        value: u8,
    ) -> Result<(), BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.write_u8(CONFIG + offset, value),
            Transport::Pci(pci) => pci.write_config_u8(offset, value),
        }
    }

    /// 读取 device config 的原子性 generation。
    ///
    /// # Returns
    ///
    /// 当前 generation value。
    ///
    /// # Errors
    ///
    /// MMIO 访问失败返回 `InvalidAddress`。
    pub(in crate::virtio) fn config_generation(&self) -> Result<u32, BusError> {
        match &self.transport {
            Transport::Mmio(bus) => bus.read_u32(CONFIG_GENERATION),
            Transport::Pci(pci) => pci.config_generation(),
        }
    }

    fn write_address(&self, low_register: usize, address: u64) -> Result<(), BusError> {
        let bus = match &self.transport {
            Transport::Mmio(bus) => bus,
            Transport::Pci(_) => return Err(BusError::InvalidAddress),
        };
        bus.write_u32(low_register, address as u32)?;
        bus.write_u32(
            low_register + core::mem::size_of::<u32>(),
            (address >> 32) as u32,
        )
    }
}
