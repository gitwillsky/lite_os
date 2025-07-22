use core::fmt;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::boxed::Box;
use super::bus::{Bus, BusError};
use super::interrupt::InterruptHandler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Block,
    Network,
    Console,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Uninitialized,
    Initializing,
    Ready,
    Error,
    Suspended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    NotSupported,
    InitializationFailed,
    InvalidState,
    BusError(BusError),
    ConfigurationError,
    OperationFailed,
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::NotSupported => write!(f, "Device not supported"),
            DeviceError::InitializationFailed => write!(f, "Device initialization failed"),
            DeviceError::InvalidState => write!(f, "Invalid device state"),
            DeviceError::BusError(e) => write!(f, "Bus error: {}", e),
            DeviceError::ConfigurationError => write!(f, "Configuration error"),
            DeviceError::OperationFailed => write!(f, "Operation failed"),
        }
    }
}

impl From<BusError> for DeviceError {
    fn from(error: BusError) -> Self {
        DeviceError::BusError(error)
    }
}

pub trait Device: Send + Sync {
    fn device_type(&self) -> DeviceType;
    fn device_id(&self) -> u32;
    fn vendor_id(&self) -> u32;
    fn device_name(&self) -> String;
    
    fn state(&self) -> DeviceState;
    
    fn probe(&self) -> Result<bool, DeviceError>;
    fn initialize(&mut self) -> Result<(), DeviceError>;
    fn reset(&mut self) -> Result<(), DeviceError>;
    fn suspend(&mut self) -> Result<(), DeviceError>;
    fn resume(&mut self) -> Result<(), DeviceError>;
    
    fn bus(&self) -> Arc<dyn Bus>;
    
    fn supports_interrupt(&self) -> bool {
        false
    }
    
    fn set_interrupt_handler(&mut self, _handler: Box<dyn InterruptHandler>) -> Result<(), DeviceError> {
        Err(DeviceError::NotSupported)
    }
}

pub struct GenericDevice {
    device_type: DeviceType,
    device_id: u32,
    vendor_id: u32,
    name: String,
    state: DeviceState,
    bus: Arc<dyn Bus>,
    interrupt_handler: Option<Box<dyn InterruptHandler>>,
}

impl GenericDevice {
    pub fn new(
        device_type: DeviceType,
        device_id: u32,
        vendor_id: u32,
        name: String,
        bus: Arc<dyn Bus>,
    ) -> Self {
        Self {
            device_type,
            device_id,
            vendor_id,
            name,
            state: DeviceState::Uninitialized,
            bus,
            interrupt_handler: None,
        }
    }
    
    pub fn set_state(&mut self, state: DeviceState) {
        self.state = state;
    }
}

impl Device for GenericDevice {
    fn device_type(&self) -> DeviceType {
        self.device_type
    }
    
    fn device_id(&self) -> u32 {
        self.device_id
    }
    
    fn vendor_id(&self) -> u32 {
        self.vendor_id
    }
    
    fn device_name(&self) -> String {
        self.name.clone()
    }
    
    fn state(&self) -> DeviceState {
        self.state
    }
    
    fn probe(&self) -> Result<bool, DeviceError> {
        Ok(self.bus.is_accessible())
    }
    
    fn initialize(&mut self) -> Result<(), DeviceError> {
        if self.state != DeviceState::Uninitialized {
            return Err(DeviceError::InvalidState);
        }
        
        self.state = DeviceState::Initializing;
        
        if !self.probe()? {
            self.state = DeviceState::Error;
            return Err(DeviceError::InitializationFailed);
        }
        
        self.state = DeviceState::Ready;
        Ok(())
    }
    
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.state = DeviceState::Uninitialized;
        self.initialize()
    }
    
    fn suspend(&mut self) -> Result<(), DeviceError> {
        if self.state != DeviceState::Ready {
            return Err(DeviceError::InvalidState);
        }
        
        self.state = DeviceState::Suspended;
        Ok(())
    }
    
    fn resume(&mut self) -> Result<(), DeviceError> {
        if self.state != DeviceState::Suspended {
            return Err(DeviceError::InvalidState);
        }
        
        self.state = DeviceState::Ready;
        Ok(())
    }
    
    fn bus(&self) -> Arc<dyn Bus> {
        self.bus.clone()
    }
    
    fn supports_interrupt(&self) -> bool {
        true
    }
    
    fn set_interrupt_handler(&mut self, handler: Box<dyn InterruptHandler>) -> Result<(), DeviceError> {
        self.interrupt_handler = Some(handler);
        Ok(())
    }
}