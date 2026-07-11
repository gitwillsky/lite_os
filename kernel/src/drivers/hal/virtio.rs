use super::bus::{BusError, MmioBus};

const MAGIC: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const DEVICE_FEATURES: usize = 0x010;
const DRIVER_FEATURES: usize = 0x020;
const GUEST_PAGE_SIZE: usize = 0x028;
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_ALIGN: usize = 0x03c;
const QUEUE_PFN: usize = 0x040;
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const CONFIG: usize = 0x100;

pub(crate) const VIRTIO_CONFIG_S_ACKNOWLEDGE: u32 = 1;
pub(crate) const VIRTIO_CONFIG_S_DRIVER: u32 = 2;
pub(crate) const VIRTIO_CONFIG_S_DRIVER_OK: u32 = 4;
pub(crate) const VIRTIO_CONFIG_S_FEATURES_OK: u32 = 8;
pub(crate) const VIRTIO_MMIO_INT_VRING: u32 = 1;
pub(crate) const VIRTIO_MMIO_INT_CONFIG: u32 = 2;

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;

/// @description 为 VirtIO MMIO legacy 设备提供启动所需的最小寄存接口。
pub(crate) struct VirtIODevice {
    bus: MmioBus,
    device_id: u32,
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
    pub(crate) fn new(base_addr: usize, size: usize) -> Result<Self, BusError> {
        let bus = MmioBus::new(base_addr, size)?;
        let device_id = bus.read_u32(DEVICE_ID)?;
        Ok(Self { bus, device_id })
    }

    pub(crate) fn device_id(&self) -> u32 {
        self.device_id
    }

    pub(crate) fn initialize(&mut self) -> Result<(), BusError> {
        let magic = self.bus.read_u32(MAGIC)?;
        let version = self.bus.read_u32(VERSION)?;
        if magic != VIRTIO_MMIO_MAGIC || (version != 1 && version != 2) {
            return Err(BusError::InvalidAddress);
        }
        self.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE)?;
        self.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER)
    }

    pub(crate) fn set_driver_features(&self, features: u32) -> Result<(), BusError> {
        self.bus.write_u32(DRIVER_FEATURES, features)
    }

    pub(crate) fn device_features(&self) -> Result<u32, BusError> {
        self.bus.read_u32(DEVICE_FEATURES)
    }

    pub(crate) fn set_status(&self, status: u32) -> Result<(), BusError> {
        self.bus.write_u32(STATUS, status)
    }

    pub(crate) fn get_status(&self) -> Result<u32, BusError> {
        self.bus.read_u32(STATUS)
    }

    pub(crate) fn set_guest_page_size(&self, size: u32) -> Result<(), BusError> {
        self.bus.write_u32(GUEST_PAGE_SIZE, size)
    }

    pub(crate) fn select_queue(&self, queue: u32) -> Result<(), BusError> {
        self.bus.write_u32(QUEUE_SEL, queue)
    }

    pub(crate) fn queue_max_size(&self) -> Result<u32, BusError> {
        self.bus.read_u32(QUEUE_NUM_MAX)
    }

    pub(crate) fn set_queue_size(&self, size: u32) -> Result<(), BusError> {
        self.bus.write_u32(QUEUE_NUM, size)
    }

    pub(crate) fn set_queue_align(&self, align: u32) -> Result<(), BusError> {
        self.bus.write_u32(QUEUE_ALIGN, align)
    }

    pub(crate) fn set_queue_pfn(&self, pfn: u32) -> Result<(), BusError> {
        self.bus.write_u32(QUEUE_PFN, pfn)
    }

    pub(crate) fn set_queue_ready(&self, ready: u32) -> Result<(), BusError> {
        self.bus.write_u32(QUEUE_READY, ready)
    }

    pub(crate) fn notify_queue(&self, queue: u32) -> Result<(), BusError> {
        self.bus.write_u32(QUEUE_NOTIFY, queue)
    }

    pub(crate) fn interrupt_status(&self) -> Result<u32, BusError> {
        self.bus.read_u32(INTERRUPT_STATUS)
    }

    pub(crate) fn interrupt_ack(&self, interrupt: u32) -> Result<(), BusError> {
        self.bus.write_u32(INTERRUPT_ACK, interrupt)
    }

    pub(crate) fn read_config_u64(&self, offset: usize) -> Result<u64, BusError> {
        let low = self.bus.read_u32(CONFIG + offset)?;
        let high = self.bus.read_u32(CONFIG + offset + 4)?;
        Ok(((high as u64) << 32) | low as u64)
    }
}
