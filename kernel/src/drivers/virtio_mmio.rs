use core::ptr::{read_volatile, write_volatile};

// VirtIO MMIO 寄存器偏移
pub const VIRTIO_MMIO_MAGIC_VALUE: usize = 0x000;
pub const VIRTIO_MMIO_VERSION: usize = 0x004;
pub const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
pub const VIRTIO_MMIO_VENDOR_ID: usize = 0x00c;
pub const VIRTIO_MMIO_DEVICE_FEATURES: usize = 0x010;
pub const VIRTIO_MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
pub const VIRTIO_MMIO_DRIVER_FEATURES: usize = 0x020;
pub const VIRTIO_MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
pub const VIRTIO_MMIO_GUEST_PAGE_SIZE: usize = 0x028;
pub const VIRTIO_MMIO_QUEUE_SEL: usize = 0x030;
pub const VIRTIO_MMIO_QUEUE_NUM_MAX: usize = 0x034;
pub const VIRTIO_MMIO_QUEUE_NUM: usize = 0x038;
pub const VIRTIO_MMIO_QUEUE_ALIGN: usize = 0x03c;
pub const VIRTIO_MMIO_QUEUE_PFN: usize = 0x040;
pub const VIRTIO_MMIO_QUEUE_READY: usize = 0x044;
pub const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;
pub const VIRTIO_MMIO_INTERRUPT_STATUS: usize = 0x060;
pub const VIRTIO_MMIO_INTERRUPT_ACK: usize = 0x064;
pub const VIRTIO_MMIO_STATUS: usize = 0x070;
pub const VIRTIO_MMIO_CONFIG: usize = 0x100;

// VirtIO 状态标志
pub const VIRTIO_CONFIG_S_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_CONFIG_S_DRIVER: u32 = 2;
pub const VIRTIO_CONFIG_S_DRIVER_OK: u32 = 4;
pub const VIRTIO_CONFIG_S_FEATURES_OK: u32 = 8;
pub const VIRTIO_CONFIG_S_FAILED: u32 = 128;

// 设备类型
pub const VIRTIO_ID_BLOCK: u32 = 2;

// 常量
pub const VIRTIO_MMIO_MAGIC: u32 = 0x74726976;
pub const VIRTIO_VERSION: u32 = 1;

pub struct VirtIOMMIO {
    base_addr: usize,
}

impl VirtIOMMIO {
    pub fn new(base_addr: usize) -> Self {
        Self { base_addr }
    }

    pub fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base_addr + offset) as *const u32) }
    }

    pub fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base_addr + offset) as *mut u32, value) };
    }

    pub fn probe(&self) -> bool {
        let magic = self.read_reg(VIRTIO_MMIO_MAGIC_VALUE);
        let version = self.read_reg(VIRTIO_MMIO_VERSION);
        magic == VIRTIO_MMIO_MAGIC && (version == 1 || version == 2)
    }

    pub fn device_id(&self) -> u32 {
        self.read_reg(VIRTIO_MMIO_DEVICE_ID)
    }

    pub fn vendor_id(&self) -> u32 {
        self.read_reg(VIRTIO_MMIO_VENDOR_ID)
    }

    pub fn device_features(&self) -> u32 {
        self.read_reg(VIRTIO_MMIO_DEVICE_FEATURES)
    }

    pub fn set_driver_features(&self, features: u32) {
        self.write_reg(VIRTIO_MMIO_DRIVER_FEATURES, features);
    }

    pub fn set_status(&self, status: u32) {
        self.write_reg(VIRTIO_MMIO_STATUS, status);
    }

    pub fn get_status(&self) -> u32 {
        self.read_reg(VIRTIO_MMIO_STATUS)
    }

    pub fn set_guest_page_size(&self, size: u32) {
        self.write_reg(VIRTIO_MMIO_GUEST_PAGE_SIZE, size);
    }

    pub fn select_queue(&self, queue: u32) {
        self.write_reg(VIRTIO_MMIO_QUEUE_SEL, queue);
    }

    pub fn queue_max_size(&self) -> u32 {
        self.read_reg(VIRTIO_MMIO_QUEUE_NUM_MAX)
    }

    pub fn set_queue_size(&self, size: u32) {
        self.write_reg(VIRTIO_MMIO_QUEUE_NUM, size);
    }

    pub fn set_queue_align(&self, align: u32) {
        self.write_reg(VIRTIO_MMIO_QUEUE_ALIGN, align);
    }

    pub fn set_queue_pfn(&self, pfn: u32) {
        self.write_reg(VIRTIO_MMIO_QUEUE_PFN, pfn);
    }

    pub fn set_queue_ready(&self, ready: u32) {
        self.write_reg(VIRTIO_MMIO_QUEUE_READY, ready);
    }

    pub fn notify_queue(&self, queue: u32) {
        // let int_status_before = self.read_reg(VIRTIO_MMIO_INTERRUPT_STATUS);

        // 执行通知
        self.write_reg(VIRTIO_MMIO_QUEUE_NOTIFY, queue);

        // 读取通知后的中断状态
        // let int_status_after = self.read_reg(VIRTIO_MMIO_INTERRUPT_STATUS);

        // 读取设备状态
        // let device_status = self.read_reg(VIRTIO_MMIO_STATUS);
    }

    pub fn interrupt_status(&self) -> u32 {
        self.read_reg(VIRTIO_MMIO_INTERRUPT_STATUS)
    }

    pub fn interrupt_ack(&self, interrupt: u32) {
        self.write_reg(VIRTIO_MMIO_INTERRUPT_ACK, interrupt);
    }
}
