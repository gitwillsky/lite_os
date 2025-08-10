use alloc::sync::Arc;
use alloc::string::{String, ToString};
use alloc::boxed::Box;
use alloc::format;
use super::bus::{Bus, MmioBus, BusError};
use super::device::{Device, DeviceType, DeviceState, DeviceError, GenericDevice};
use super::interrupt::{InterruptHandler, InterruptVector, InterruptError};

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

pub const VIRTIO_CONFIG_S_ACKNOWLEDGE: u32 = 1;
pub const VIRTIO_CONFIG_S_DRIVER: u32 = 2;
pub const VIRTIO_CONFIG_S_DRIVER_OK: u32 = 4;
pub const VIRTIO_CONFIG_S_FEATURES_OK: u32 = 8;
pub const VIRTIO_CONFIG_S_FAILED: u32 = 128;

pub const VIRTIO_ID_BLOCK: u32 = 2;
pub const VIRTIO_ID_CONSOLE: u32 = 3;

pub const VIRTIO_MMIO_INT_VRING: u32 = 1;
pub const VIRTIO_MMIO_INT_CONFIG: u32 = 2;

pub const VIRTIO_MMIO_MAGIC: u32 = 0x74726976;
pub const VIRTIO_VERSION: u32 = 1;

pub struct VirtIODevice {
    inner: GenericDevice,
    config_space_offset: usize,
}

impl VirtIODevice {
    pub fn new(base_addr: usize, size: usize) -> Result<Self, DeviceError> {
        let bus = Arc::new(MmioBus::new(base_addr, size).map_err(DeviceError::from)?);
        
        let device_id = bus.read_u32(VIRTIO_MMIO_DEVICE_ID).map_err(DeviceError::from)?;
        let vendor_id = bus.read_u32(VIRTIO_MMIO_VENDOR_ID).map_err(DeviceError::from)?;
        
        let device_type = match device_id {
            VIRTIO_ID_BLOCK => DeviceType::Block,
            VIRTIO_ID_CONSOLE => DeviceType::Console,
            _ => DeviceType::Generic,
        };
        
        let name = format!("VirtIO-{}", device_id);
        
        let driver_name = "VirtIO Driver".to_string();
        let inner = GenericDevice::new(device_type, device_id, vendor_id, name, driver_name, bus);
        
        Ok(Self {
            inner,
            config_space_offset: VIRTIO_MMIO_CONFIG,
        })
    }
    
    pub fn probe_virtio(&self) -> Result<bool, DeviceError> {
        let bus = self.inner.bus();
        
        let magic = bus.read_u32(VIRTIO_MMIO_MAGIC_VALUE).map_err(DeviceError::from)?;
        let version = bus.read_u32(VIRTIO_MMIO_VERSION).map_err(DeviceError::from)?;
        
        Ok(magic == VIRTIO_MMIO_MAGIC && (version == 1 || version == 2))
    }
    
    pub fn device_features(&self) -> Result<u32, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u32(VIRTIO_MMIO_DEVICE_FEATURES).map_err(DeviceError::from)
    }
    
    pub fn set_driver_features(&self, features: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_DRIVER_FEATURES, features).map_err(DeviceError::from)
    }
    
    pub fn set_status(&self, status: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_STATUS, status).map_err(DeviceError::from)
    }
    
    pub fn get_status(&self) -> Result<u32, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u32(VIRTIO_MMIO_STATUS).map_err(DeviceError::from)
    }
    
    pub fn set_guest_page_size(&self, size: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_GUEST_PAGE_SIZE, size).map_err(DeviceError::from)
    }
    
    pub fn select_queue(&self, queue: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_QUEUE_SEL, queue).map_err(DeviceError::from)
    }
    
    pub fn queue_max_size(&self) -> Result<u32, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u32(VIRTIO_MMIO_QUEUE_NUM_MAX).map_err(DeviceError::from)
    }
    
    pub fn set_queue_size(&self, size: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_QUEUE_NUM, size).map_err(DeviceError::from)
    }
    
    pub fn set_queue_align(&self, align: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_QUEUE_ALIGN, align).map_err(DeviceError::from)
    }
    
    pub fn set_queue_pfn(&self, pfn: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_QUEUE_PFN, pfn).map_err(DeviceError::from)
    }
    
    pub fn set_queue_ready(&self, ready: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_QUEUE_READY, ready).map_err(DeviceError::from)
    }
    
    pub fn notify_queue(&self, queue: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_QUEUE_NOTIFY, queue).map_err(DeviceError::from)
    }
    
    pub fn interrupt_status(&self) -> Result<u32, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u32(VIRTIO_MMIO_INTERRUPT_STATUS).map_err(DeviceError::from)
    }
    
    pub fn interrupt_ack(&self, interrupt: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(VIRTIO_MMIO_INTERRUPT_ACK, interrupt).map_err(DeviceError::from)
    }
    
    pub fn read_config_u8(&self, offset: usize) -> Result<u8, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u8(self.config_space_offset + offset).map_err(DeviceError::from)
    }
    
    pub fn read_config_u16(&self, offset: usize) -> Result<u16, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u16(self.config_space_offset + offset).map_err(DeviceError::from)
    }
    
    pub fn read_config_u32(&self, offset: usize) -> Result<u32, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u32(self.config_space_offset + offset).map_err(DeviceError::from)
    }
    
    pub fn read_config_u64(&self, offset: usize) -> Result<u64, DeviceError> {
        let bus = self.inner.bus();
        bus.read_u64(self.config_space_offset + offset).map_err(DeviceError::from)
    }
    
    pub fn write_config_u8(&self, offset: usize, value: u8) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u8(self.config_space_offset + offset, value).map_err(DeviceError::from)
    }
    
    pub fn write_config_u16(&self, offset: usize, value: u16) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u16(self.config_space_offset + offset, value).map_err(DeviceError::from)
    }
    
    pub fn write_config_u32(&self, offset: usize, value: u32) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u32(self.config_space_offset + offset, value).map_err(DeviceError::from)
    }
    
    pub fn write_config_u64(&self, offset: usize, value: u64) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        bus.write_u64(self.config_space_offset + offset, value).map_err(DeviceError::from)
    }
}

impl Device for VirtIODevice {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }
    
    fn device_id(&self) -> u32 {
        self.inner.device_id()
    }
    
    fn vendor_id(&self) -> u32 {
        self.inner.vendor_id()
    }
    
    fn device_name(&self) -> String {
        self.inner.device_name()
    }
    
    fn driver_name(&self) -> String {
        self.inner.driver_name()
    }
    
    fn state(&self) -> DeviceState {
        self.inner.state()
    }
    
    fn probe(&mut self) -> Result<bool, DeviceError> {
        self.probe_virtio()
    }
    
    fn initialize(&mut self) -> Result<(), DeviceError> {
        if !self.probe()? {
            return Err(DeviceError::InitializationFailed);
        }
        
        self.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE)?;
        self.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER)?;
        
        self.inner.set_state(DeviceState::Ready);
        Ok(())
    }
    
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.set_status(0)?;
        self.inner.set_state(DeviceState::Uninitialized);
        self.initialize()
    }
    
    fn shutdown(&mut self) -> Result<(), DeviceError> {
        self.inner.shutdown()
    }
    
    fn remove(&mut self) -> Result<(), DeviceError> {
        self.inner.remove()
    }
    
    fn suspend(&mut self) -> Result<(), DeviceError> {
        self.inner.suspend()
    }
    
    fn resume(&mut self) -> Result<(), DeviceError> {
        self.inner.resume()
    }
    
    fn bus(&self) -> Arc<dyn Bus> {
        self.inner.bus()
    }
    
    fn resources(&self) -> alloc::vec::Vec<super::resource::Resource> {
        self.inner.resources()
    }
    
    fn request_resources(&mut self, resource_manager: &mut dyn super::resource::ResourceManager) -> Result<(), DeviceError> {
        self.inner.request_resources(resource_manager)
    }
    
    fn release_resources(&mut self, resource_manager: &mut dyn super::resource::ResourceManager) -> Result<(), DeviceError> {
        self.inner.release_resources(resource_manager)
    }
    
    fn supports_interrupt(&self) -> bool {
        true
    }
    
    fn set_interrupt_handler(&mut self, vector: InterruptVector, handler: Arc<dyn InterruptHandler>) -> Result<(), DeviceError> {
        self.inner.set_interrupt_handler(vector, handler)
    }
    
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}