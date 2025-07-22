use core::fmt;
use alloc::boxed::Box;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusType {
    MMIO,
    PCI,
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusError {
    InvalidAddress,
    AccessDenied,
    DeviceNotFound,
    BusUnavailable,
    InvalidOperation,
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::InvalidAddress => write!(f, "Invalid address"),
            BusError::AccessDenied => write!(f, "Access denied"),
            BusError::DeviceNotFound => write!(f, "Device not found"),
            BusError::BusUnavailable => write!(f, "Bus unavailable"),
            BusError::InvalidOperation => write!(f, "Invalid operation"),
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
    
    fn base_address(&self) -> usize;
    fn size(&self) -> usize;
    
    fn is_accessible(&self) -> bool;
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
        
        Ok(Self {
            base_addr,
            size,
        })
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
            Ok(core::ptr::read_volatile((self.base_addr + offset) as *const u8))
        }
    }
    
    fn read_u16(&self, offset: usize) -> Result<u16, BusError> {
        self.validate_access(offset, 2)?;
        unsafe {
            Ok(core::ptr::read_volatile((self.base_addr + offset) as *const u16))
        }
    }
    
    fn read_u32(&self, offset: usize) -> Result<u32, BusError> {
        self.validate_access(offset, 4)?;
        unsafe {
            Ok(core::ptr::read_volatile((self.base_addr + offset) as *const u32))
        }
    }
    
    fn read_u64(&self, offset: usize) -> Result<u64, BusError> {
        self.validate_access(offset, 8)?;
        unsafe {
            Ok(core::ptr::read_volatile((self.base_addr + offset) as *const u64))
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
}