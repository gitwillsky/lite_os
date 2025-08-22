use alloc::boxed::Box;
use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusType {
    MMIO,
    PCI,
    PCIe,
    Platform,
    I2C,
    SPI,
    USB,
    VirtIO,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    InvalidAddress,
    AccessDenied,
    DeviceNotFound,
    BusUnavailable,
    InvalidOperation,
    ConfigurationError,
    TimeoutError,
    AlignmentError,
    OutOfBounds,
    NotSupported,
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::InvalidAddress => write!(f, "Invalid address"),
            BusError::AccessDenied => write!(f, "Access denied"),
            BusError::DeviceNotFound => write!(f, "Device not found"),
            BusError::BusUnavailable => write!(f, "Bus unavailable"),
            BusError::InvalidOperation => write!(f, "Invalid operation"),
            BusError::ConfigurationError => write!(f, "Configuration error"),
            BusError::TimeoutError => write!(f, "Operation timeout"),
            BusError::AlignmentError => write!(f, "Alignment error"),
            BusError::OutOfBounds => write!(f, "Access out of bounds"),
            BusError::NotSupported => write!(f, "Operation not supported"),
        }
    }
}

pub trait Bus: Send + Sync {
    fn bus_type(&self) -> BusType;

    fn read_u8(&self, offset: usize) -> Result<u8, BusError>;
    fn read_u16(&self, offset: usize) -> Result<u16, BusError>;
    fn read_u32(&self, offset: usize) -> Result<u32, BusError>;
    fn read_u64(&self, offset: usize) -> Result<u64, BusError>;

    fn write_u8(&self, offset: usize, value: u8) -> Result<(), BusError>;
    fn write_u16(&self, offset: usize, value: u16) -> Result<(), BusError>;
    fn write_u32(&self, offset: usize, value: u32) -> Result<(), BusError>;
    fn write_u64(&self, offset: usize, value: u64) -> Result<(), BusError>;

    fn read_buffer(&self, offset: usize, buffer: &mut [u8]) -> Result<usize, BusError> {
        for (i, byte) in buffer.iter_mut().enumerate() {
            *byte = self.read_u8(offset + i)?;
        }
        Ok(buffer.len())
    }

    fn write_buffer(&self, offset: usize, buffer: &[u8]) -> Result<usize, BusError> {
        for (i, &byte) in buffer.iter().enumerate() {
            self.write_u8(offset + i, byte)?;
        }
        Ok(buffer.len())
    }

    fn base_address(&self) -> usize;
    fn size(&self) -> usize;

    fn is_accessible(&self) -> bool;
    fn supports_dma(&self) -> bool {
        false
    }
    fn alignment_requirement(&self) -> usize {
        1
    }

    fn set_power_state(&self, _enabled: bool) -> Result<(), BusError> {
        Err(BusError::NotSupported)
    }

    fn enumerate_devices(&self) -> Result<alloc::vec::Vec<(u32, u32)>, BusError> {
        Err(BusError::NotSupported)
    }
}

pub struct MmioBus {
    base_addr: usize,
    size: usize,
}

impl MmioBus {
    pub fn new(base_addr: usize, size: usize) -> Result<Self, BusError> {
        if base_addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        Ok(Self { base_addr, size })
    }

    fn validate_access(&self, offset: usize, size: usize) -> Result<(), BusError> {
        if offset + size > self.size {
            return Err(BusError::InvalidAddress);
        }
        Ok(())
    }
}

impl Bus for MmioBus {
    fn bus_type(&self) -> BusType {
        BusType::MMIO
    }

    fn read_u8(&self, offset: usize) -> Result<u8, BusError> {
        self.validate_access(offset, 1)?;
        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u8,
            ))
        }
    }

    fn read_u16(&self, offset: usize) -> Result<u16, BusError> {
        self.validate_access(offset, 2)?;
        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u16,
            ))
        }
    }

    fn read_u32(&self, offset: usize) -> Result<u32, BusError> {
        self.validate_access(offset, 4)?;
        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u32,
            ))
        }
    }

    fn read_u64(&self, offset: usize) -> Result<u64, BusError> {
        self.validate_access(offset, 8)?;
        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u64,
            ))
        }
    }

    fn write_u8(&self, offset: usize, value: u8) -> Result<(), BusError> {
        self.validate_access(offset, 1)?;
        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u8, value);
        }
        Ok(())
    }

    fn write_u16(&self, offset: usize, value: u16) -> Result<(), BusError> {
        self.validate_access(offset, 2)?;
        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u16, value);
        }
        Ok(())
    }

    fn write_u32(&self, offset: usize, value: u32) -> Result<(), BusError> {
        self.validate_access(offset, 4)?;
        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u32, value);
        }
        Ok(())
    }

    fn write_u64(&self, offset: usize, value: u64) -> Result<(), BusError> {
        self.validate_access(offset, 8)?;
        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u64, value);
        }
        Ok(())
    }

    fn base_address(&self) -> usize {
        self.base_addr
    }

    fn size(&self) -> usize {
        self.size
    }

    fn is_accessible(&self) -> bool {
        self.base_addr != 0
    }

    fn supports_dma(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
pub struct PciConfigSpace {
    pub vendor_id: u16,
    pub device_id: u16,
    pub command: u16,
    pub status: u16,
    pub revision_id: u8,
    pub class_code: [u8; 3],
    pub header_type: u8,
    pub bars: [u32; 6],
    pub interrupt_line: u8,
    pub interrupt_pin: u8,
}

pub struct PciBus {
    bus_number: u8,
    device_number: u8,
    function_number: u8,
    config_base: usize,
    devices: alloc::vec::Vec<(u8, u8, u8)>, // (bus, device, function)
}

impl PciBus {
    pub fn new(
        bus_number: u8,
        device_number: u8,
        function_number: u8,
        config_base: usize,
    ) -> Result<Self, BusError> {
        Ok(Self {
            bus_number,
            device_number,
            function_number,
            config_base,
            devices: alloc::vec::Vec::new(),
        })
    }

    fn config_address(&self, offset: usize) -> usize {
        if offset >= 256 {
            return 0;
        }

        self.config_base
            + ((self.bus_number as usize) << 20)
            + ((self.device_number as usize) << 15)
            + ((self.function_number as usize) << 12)
            + offset
    }

    pub fn read_config_space(&self) -> Result<PciConfigSpace, BusError> {
        let vendor_id = self.read_u16(0)?;
        if vendor_id == 0xFFFF {
            return Err(BusError::DeviceNotFound);
        }

        Ok(PciConfigSpace {
            vendor_id,
            device_id: self.read_u16(2)?,
            command: self.read_u16(4)?,
            status: self.read_u16(6)?,
            revision_id: self.read_u8(8)?,
            class_code: [self.read_u8(9)?, self.read_u8(10)?, self.read_u8(11)?],
            header_type: self.read_u8(14)?,
            bars: [
                self.read_u32(16)?,
                self.read_u32(20)?,
                self.read_u32(24)?,
                self.read_u32(28)?,
                self.read_u32(32)?,
                self.read_u32(36)?,
            ],
            interrupt_line: self.read_u8(60)?,
            interrupt_pin: self.read_u8(61)?,
        })
    }

    pub fn scan_devices(&mut self) -> Result<(), BusError> {
        self.devices.clear();

        for bus in 0..=255u8 {
            for device in 0..32u8 {
                for function in 0..8u8 {
                    let temp_bus = PciBus::new(bus, device, function, self.config_base)?;
                    if temp_bus.read_u16(0).is_ok() {
                        let vendor_id = temp_bus.read_u16(0)?;
                        if vendor_id != 0xFFFF {
                            self.devices.push((bus, device, function));
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl Bus for PciBus {
    fn bus_type(&self) -> BusType {
        BusType::PCI
    }

    fn read_u8(&self, offset: usize) -> Result<u8, BusError> {
        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe { Ok(core::ptr::read_volatile(addr as *const u8)) }
    }

    fn read_u16(&self, offset: usize) -> Result<u16, BusError> {
        if offset % 2 != 0 {
            return Err(BusError::AlignmentError);
        }

        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe { Ok(core::ptr::read_volatile(addr as *const u16)) }
    }

    fn read_u32(&self, offset: usize) -> Result<u32, BusError> {
        if offset % 4 != 0 {
            return Err(BusError::AlignmentError);
        }

        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe { Ok(core::ptr::read_volatile(addr as *const u32)) }
    }

    fn read_u64(&self, offset: usize) -> Result<u64, BusError> {
        if offset % 8 != 0 {
            return Err(BusError::AlignmentError);
        }

        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe { Ok(core::ptr::read_volatile(addr as *const u64)) }
    }

    fn write_u8(&self, offset: usize, value: u8) -> Result<(), BusError> {
        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe {
            core::ptr::write_volatile(addr as *mut u8, value);
        }
        Ok(())
    }

    fn write_u16(&self, offset: usize, value: u16) -> Result<(), BusError> {
        if offset % 2 != 0 {
            return Err(BusError::AlignmentError);
        }

        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe {
            core::ptr::write_volatile(addr as *mut u16, value);
        }
        Ok(())
    }

    fn write_u32(&self, offset: usize, value: u32) -> Result<(), BusError> {
        if offset % 4 != 0 {
            return Err(BusError::AlignmentError);
        }

        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe {
            core::ptr::write_volatile(addr as *mut u32, value);
        }
        Ok(())
    }

    fn write_u64(&self, offset: usize, value: u64) -> Result<(), BusError> {
        if offset % 8 != 0 {
            return Err(BusError::AlignmentError);
        }

        let addr = self.config_address(offset);
        if addr == 0 {
            return Err(BusError::InvalidAddress);
        }

        unsafe {
            core::ptr::write_volatile(addr as *mut u64, value);
        }
        Ok(())
    }

    fn base_address(&self) -> usize {
        self.config_base
    }

    fn size(&self) -> usize {
        256 // PCI configuration space size
    }

    fn is_accessible(&self) -> bool {
        self.config_base != 0
    }

    fn supports_dma(&self) -> bool {
        true
    }

    fn alignment_requirement(&self) -> usize {
        4
    }

    fn enumerate_devices(&self) -> Result<alloc::vec::Vec<(u32, u32)>, BusError> {
        let mut devices = alloc::vec::Vec::new();

        for &(bus, device, function) in &self.devices {
            let temp_bus = PciBus::new(bus, device, function, self.config_base)?;
            let vendor_id = temp_bus.read_u16(0)? as u32;
            let device_id = temp_bus.read_u16(2)? as u32;
            devices.push((vendor_id, device_id));
        }

        Ok(devices)
    }
}

pub struct PlatformBus {
    name: alloc::string::String,
    base_addr: usize,
    size: usize,
    devices: alloc::vec::Vec<PlatformDevice>,
}

#[derive(Debug, Clone)]
pub struct PlatformDevice {
    pub name: alloc::string::String,
    pub compatible: alloc::vec::Vec<alloc::string::String>,
    pub reg: alloc::vec::Vec<(usize, usize)>, // (address, size) pairs
    pub interrupts: alloc::vec::Vec<u32>,
    pub properties: alloc::collections::BTreeMap<alloc::string::String, alloc::string::String>,
}

impl PlatformBus {
    pub fn new(name: alloc::string::String, base_addr: usize, size: usize) -> Self {
        Self {
            name,
            base_addr,
            size,
            devices: alloc::vec::Vec::new(),
        }
    }

    pub fn add_device(&mut self, device: PlatformDevice) {
        self.devices.push(device);
    }

    pub fn find_device(&self, compatible: &str) -> Option<&PlatformDevice> {
        self.devices
            .iter()
            .find(|dev| dev.compatible.iter().any(|c| c == compatible))
    }

    pub fn get_devices(&self) -> &[PlatformDevice] {
        &self.devices
    }
}

impl Bus for PlatformBus {
    fn bus_type(&self) -> BusType {
        BusType::Platform
    }

    fn read_u8(&self, offset: usize) -> Result<u8, BusError> {
        if offset >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u8,
            ))
        }
    }

    fn read_u16(&self, offset: usize) -> Result<u16, BusError> {
        if offset + 1 >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u16,
            ))
        }
    }

    fn read_u32(&self, offset: usize) -> Result<u32, BusError> {
        if offset + 3 >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u32,
            ))
        }
    }

    fn read_u64(&self, offset: usize) -> Result<u64, BusError> {
        if offset + 7 >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            Ok(core::ptr::read_volatile(
                (self.base_addr + offset) as *const u64,
            ))
        }
    }

    fn write_u8(&self, offset: usize, value: u8) -> Result<(), BusError> {
        if offset >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u8, value);
        }
        Ok(())
    }

    fn write_u16(&self, offset: usize, value: u16) -> Result<(), BusError> {
        if offset + 1 >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u16, value);
        }
        Ok(())
    }

    fn write_u32(&self, offset: usize, value: u32) -> Result<(), BusError> {
        if offset + 3 >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u32, value);
        }
        Ok(())
    }

    fn write_u64(&self, offset: usize, value: u64) -> Result<(), BusError> {
        if offset + 7 >= self.size {
            return Err(BusError::OutOfBounds);
        }

        unsafe {
            core::ptr::write_volatile((self.base_addr + offset) as *mut u64, value);
        }
        Ok(())
    }

    fn base_address(&self) -> usize {
        self.base_addr
    }

    fn size(&self) -> usize {
        self.size
    }

    fn is_accessible(&self) -> bool {
        self.base_addr != 0
    }

    fn supports_dma(&self) -> bool {
        true
    }

    fn enumerate_devices(&self) -> Result<alloc::vec::Vec<(u32, u32)>, BusError> {
        // Platform devices don't have traditional vendor/device IDs
        // Return a hash of device names instead
        let mut devices = alloc::vec::Vec::new();

        for device in &self.devices {
            let name_hash = device
                .name
                .bytes()
                .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
            let compat_hash = device
                .compatible
                .get(0)
                .map(|s| {
                    s.bytes()
                        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32))
                })
                .unwrap_or(0);
            devices.push((name_hash, compat_hash));
        }

        Ok(devices)
    }
}
