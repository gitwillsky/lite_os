use core::fmt;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::boxed::Box;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use spin::Mutex;
use super::bus::{Bus, BusError};
use super::interrupt::{InterruptHandler, InterruptController, InterruptVector};
use super::power::{PowerManagement, PowerState, PowerError};
use super::resource::{Resource, ResourceManager, ResourceError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceType {
    Block,
    Network,
    Console,
    Storage,
    Input,
    Display,
    Audio,
    Usb,
    Pci,
    Platform,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceState {
    Uninitialized,
    Probing,
    Initializing,
    Ready,
    Suspended,
    Removing,
    Error,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    NotSupported,
    InitializationFailed,
    InvalidState,
    BusError(BusError),
    ConfigurationError,
    OperationFailed,
    ResourceError(ResourceError),
    PowerError(PowerError),
    DriverNotFound,
    DeviceNotFound,
    TimeoutError,
    HardwareError,
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
            DeviceError::ResourceError(e) => write!(f, "Resource error: {}", e),
            DeviceError::PowerError(e) => write!(f, "Power error: {}", e),
            DeviceError::DriverNotFound => write!(f, "Driver not found"),
            DeviceError::DeviceNotFound => write!(f, "Device not found"),
            DeviceError::TimeoutError => write!(f, "Device operation timeout"),
            DeviceError::HardwareError => write!(f, "Hardware error"),
        }
    }
}

impl From<BusError> for DeviceError {
    fn from(error: BusError) -> Self {
        DeviceError::BusError(error)
    }
}

impl From<ResourceError> for DeviceError {
    fn from(error: ResourceError) -> Self {
        DeviceError::ResourceError(error)
    }
}

impl From<PowerError> for DeviceError {
    fn from(error: PowerError) -> Self {
        DeviceError::PowerError(error)
    }
}

impl From<super::interrupt::InterruptError> for DeviceError {
    fn from(error: super::interrupt::InterruptError) -> Self {
        match error {
            super::interrupt::InterruptError::VectorNotFound => DeviceError::HardwareError,
            super::interrupt::InterruptError::HandlerNotSet => DeviceError::ConfigurationError,
            super::interrupt::InterruptError::InvalidVector => DeviceError::ConfigurationError,
            super::interrupt::InterruptError::ControllerError => DeviceError::HardwareError,
            super::interrupt::InterruptError::ResourceConflict => DeviceError::OperationFailed,
            super::interrupt::InterruptError::HardwareError => DeviceError::HardwareError,
            super::interrupt::InterruptError::TimeoutError => DeviceError::TimeoutError,
            super::interrupt::InterruptError::InvalidPriority => DeviceError::ConfigurationError,
            super::interrupt::InterruptError::CpuAffinityError => DeviceError::ConfigurationError,
        }
    }
}

pub trait Device: Send + Sync {
    fn device_type(&self) -> DeviceType;
    fn device_id(&self) -> u32;
    fn vendor_id(&self) -> u32;
    fn device_name(&self) -> String;
    fn driver_name(&self) -> String;

    fn state(&self) -> DeviceState;

    fn probe(&mut self) -> Result<bool, DeviceError>;
    fn initialize(&mut self) -> Result<(), DeviceError>;
    fn reset(&mut self) -> Result<(), DeviceError>;
    fn shutdown(&mut self) -> Result<(), DeviceError>;
    fn remove(&mut self) -> Result<(), DeviceError>;

    fn suspend(&mut self) -> Result<(), DeviceError>;
    fn resume(&mut self) -> Result<(), DeviceError>;

    fn bus(&self) -> Arc<dyn Bus>;

    fn resources(&self) -> Vec<Resource>;
    fn request_resources(&mut self, resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError>;
    fn release_resources(&mut self, resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError>;

    fn supports_interrupt(&self) -> bool {
        false
    }

    fn interrupt_vectors(&self) -> Vec<InterruptVector> {
        Vec::new()
    }

    fn set_interrupt_handler(&mut self, _vector: InterruptVector, _handler: Arc<dyn InterruptHandler>) -> Result<(), DeviceError> {
        Err(DeviceError::NotSupported)
    }

    fn supports_power_management(&self) -> bool {
        false
    }

    fn power_manager(&self) -> Option<Arc<dyn PowerManagement>> {
        None
    }

    fn supports_hotplug(&self) -> bool {
        false
    }

    fn get_property(&self, _name: &str) -> Option<String> {
        None
    }

    fn set_property(&mut self, _name: &str, _value: &str) -> Result<(), DeviceError> {
        Err(DeviceError::NotSupported)
    }

    fn as_any(&self) -> &dyn core::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any;
}

pub struct GenericDevice {
    device_type: DeviceType,
    device_id: u32,
    vendor_id: u32,
    name: String,
    driver_name: String,
    state: Mutex<DeviceState>,
    bus: Arc<dyn Bus>,
    resources: Vec<Resource>,
    interrupt_handlers: Mutex<BTreeMap<InterruptVector, Arc<dyn InterruptHandler>>>,
    power_manager: Option<Arc<dyn PowerManagement>>,
    properties: Mutex<BTreeMap<String, String>>,
}

impl GenericDevice {
    pub fn new(
        device_type: DeviceType,
        device_id: u32,
        vendor_id: u32,
        name: String,
        driver_name: String,
        bus: Arc<dyn Bus>,
    ) -> Self {
        Self {
            device_type,
            device_id,
            vendor_id,
            name,
            driver_name,
            state: Mutex::new(DeviceState::Uninitialized),
            bus,
            resources: Vec::new(),
            interrupt_handlers: Mutex::new(BTreeMap::new()),
            power_manager: None,
            properties: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_resources(mut self, resources: Vec<Resource>) -> Self {
        self.resources = resources;
        self
    }

    pub fn with_power_manager(mut self, power_manager: Arc<dyn PowerManagement>) -> Self {
        self.power_manager = Some(power_manager);
        self
    }

    pub fn set_state(&self, state: DeviceState) {
        *self.state.lock() = state;
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

    fn driver_name(&self) -> String {
        self.driver_name.clone()
    }

    fn state(&self) -> DeviceState {
        *self.state.lock()
    }

    fn probe(&mut self) -> Result<bool, DeviceError> {
        self.set_state(DeviceState::Probing);
        let result = self.bus.is_accessible();
        if !result {
            self.set_state(DeviceState::Failed);
        }
        Ok(result)
    }

    fn initialize(&mut self) -> Result<(), DeviceError> {
        let current_state = *self.state.lock();
        if current_state != DeviceState::Uninitialized && current_state != DeviceState::Failed {
            return Err(DeviceError::InvalidState);
        }

        self.set_state(DeviceState::Initializing);

        if !self.probe()? {
            self.set_state(DeviceState::Failed);
            return Err(DeviceError::InitializationFailed);
        }

        self.set_state(DeviceState::Ready);
        Ok(())
    }

    fn reset(&mut self) -> Result<(), DeviceError> {
        self.set_state(DeviceState::Uninitialized);
        self.initialize()
    }

    fn shutdown(&mut self) -> Result<(), DeviceError> {
        match self.state() {
            DeviceState::Ready | DeviceState::Suspended => {
                // Perform device-specific shutdown
                self.set_state(DeviceState::Uninitialized);
                Ok(())
            }
            _ => Err(DeviceError::InvalidState),
        }
    }

    fn remove(&mut self) -> Result<(), DeviceError> {
        self.set_state(DeviceState::Removing);
        // Device-specific cleanup would go here
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), DeviceError> {
        if self.state() != DeviceState::Ready {
            return Err(DeviceError::InvalidState);
        }

        // Use power manager if available
        if let Some(pm) = &self.power_manager {
            // Power manager is behind Arc, so we can't call mutable methods directly
            // In a real implementation, we'd use interior mutability or different design
        }

        self.set_state(DeviceState::Suspended);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DeviceError> {
        if self.state() != DeviceState::Suspended {
            return Err(DeviceError::InvalidState);
        }

        // Use power manager if available
        if let Some(pm) = &self.power_manager {
            // Power manager resume logic would go here
        }

        self.set_state(DeviceState::Ready);
        Ok(())
    }

    fn bus(&self) -> Arc<dyn Bus> {
        self.bus.clone()
    }

    fn resources(&self) -> Vec<Resource> {
        self.resources.clone()
    }

    fn request_resources(&mut self, resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        for resource in &self.resources {
            resource_manager.request_resource(resource.clone(), &self.device_name())?;
        }
        Ok(())
    }

    fn release_resources(&mut self, resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        for resource in &self.resources {
            resource_manager.release_resource(resource, &self.device_name())?;
        }
        Ok(())
    }

    fn supports_interrupt(&self) -> bool {
        !self.resources.iter().all(|r| !matches!(r, Resource::Interrupt(_)))
    }

    fn interrupt_vectors(&self) -> Vec<InterruptVector> {
        self.resources.iter()
            .filter_map(|r| match r {
                Resource::Interrupt(irq) => Some(irq.irq_num),
                _ => None,
            })
            .collect()
    }

    fn set_interrupt_handler(&mut self, vector: InterruptVector, handler: Arc<dyn InterruptHandler>) -> Result<(), DeviceError> {
        let mut handlers = self.interrupt_handlers.lock();
        handlers.insert(vector, handler);
        Ok(())
    }

    fn supports_power_management(&self) -> bool {
        self.power_manager.is_some()
    }

    fn power_manager(&self) -> Option<Arc<dyn PowerManagement>> {
        self.power_manager.clone()
    }

    fn supports_hotplug(&self) -> bool {
        false
    }

    fn get_property(&self, name: &str) -> Option<String> {
        let properties = self.properties.lock();
        properties.get(name).cloned()
    }

    fn set_property(&mut self, name: &str, value: &str) -> Result<(), DeviceError> {
        let mut properties = self.properties.lock();
        properties.insert(name.into(), value.into());
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

pub trait DeviceDriver: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn compatible_devices(&self) -> Vec<(u32, u32)>; // (vendor_id, device_id) pairs
    fn probe(&self, device: &mut dyn Device) -> Result<bool, DeviceError>;
    fn bind(&self, device: &mut dyn Device) -> Result<(), DeviceError>;
    fn unbind(&self, device: &mut dyn Device) -> Result<(), DeviceError>;
    fn supports_hotplug(&self) -> bool { false }
}

pub struct DeviceManager {
    devices: Mutex<BTreeMap<u32, Arc<Mutex<Box<dyn Device>>>>>,
    drivers: Mutex<Vec<Arc<dyn DeviceDriver>>>,
    resource_manager: Mutex<Box<dyn ResourceManager>>,
    interrupt_controller: Option<Arc<Mutex<dyn InterruptController>>>,
    next_device_id: Mutex<u32>,
    device_tree: Mutex<BTreeMap<u32, Vec<u32>>>, // parent -> children mapping
}

impl DeviceManager {
    pub fn new(resource_manager: Box<dyn ResourceManager>) -> Self {
        Self {
            devices: Mutex::new(BTreeMap::new()),
            drivers: Mutex::new(Vec::new()),
            resource_manager: Mutex::new(resource_manager),
            interrupt_controller: None,
            next_device_id: Mutex::new(1),
            device_tree: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_interrupt_controller(mut self, controller: Arc<Mutex<dyn InterruptController>>) -> Self {
        self.interrupt_controller = Some(controller);
        self
    }

    pub fn get_interrupt_controller(&self) -> Option<Arc<Mutex<dyn InterruptController>>> {
        self.interrupt_controller.as_ref().cloned()
    }

    pub fn register_driver(&self, driver: Arc<dyn DeviceDriver>) -> Result<(), DeviceError> {
        let mut drivers = self.drivers.lock();
        drivers.push(driver);
        Ok(())
    }

    pub fn unregister_driver(&self, driver_name: &str) -> Result<(), DeviceError> {
        let mut drivers = self.drivers.lock();
        let initial_len = drivers.len();
        drivers.retain(|d| d.name() != driver_name);

        if drivers.len() == initial_len {
            return Err(DeviceError::DriverNotFound);
        }

        Ok(())
    }

    pub fn add_device(&self, mut device: Box<dyn Device>) -> Result<u32, DeviceError> {
        let device_id = {
            let mut next_id = self.next_device_id.lock();
            let id = *next_id;
            *next_id += 1;
            id
        };

        // Try to find a compatible driver
        let drivers = self.drivers.lock();
        for driver in drivers.iter() {
            let compatible = driver.compatible_devices();
            if compatible.iter().any(|&(vid, did)|
                vid == device.vendor_id() && did == device.device_id()
            ) {
                if driver.probe(device.as_mut())? {
                    // Request resources for the device
                    {
                        let mut resource_manager = self.resource_manager.lock();
                        device.request_resources(resource_manager.as_mut())?;
                    }

                    // Initialize device
                    device.initialize()?;

                    // Bind driver to device
                    driver.bind(device.as_mut())?;

                    // Setup interrupt handling if supported
                    if device.supports_interrupt() {
                        if let Some(ref interrupt_controller) = self.interrupt_controller {
                            for vector in device.interrupt_vectors() {
                                // Create a device-specific interrupt handler
                                // In a real implementation, this would be more sophisticated
                            }
                        }
                    }

                    break;
                }
            }
        }

        // Store the device
        let mut devices = self.devices.lock();
        devices.insert(device_id, Arc::new(Mutex::new(device)));

        Ok(device_id)
    }

    pub fn remove_device(&self, device_id: u32) -> Result<(), DeviceError> {
        let device = {
            let mut devices = self.devices.lock();
            devices.remove(&device_id).ok_or(DeviceError::DeviceNotFound)?
        };

        let mut device = device.lock();

        // Find and unbind driver
        let drivers = self.drivers.lock();
        for driver in drivers.iter() {
            let compatible = driver.compatible_devices();
            if compatible.iter().any(|&(vid, did)|
                vid == device.vendor_id() && did == device.device_id()
            ) {
                driver.unbind(&mut **device)?;
                break;
            }
        }

        // Remove device and release resources
        (**device).remove()?;
        {
            let mut resource_manager = self.resource_manager.lock();
            (**device).release_resources(resource_manager.as_mut())?;
        }

        // Remove from device tree
        let mut device_tree = self.device_tree.lock();
        device_tree.remove(&device_id);

        Ok(())
    }

    pub fn get_device(&self, device_id: u32) -> Option<Arc<Mutex<Box<dyn Device>>>> {
        let devices = self.devices.lock();
        devices.get(&device_id).cloned()
    }

    pub fn find_devices_by_type(&self, device_type: DeviceType) -> Vec<u32> {
        let devices = self.devices.lock();
        devices.iter()
            .filter_map(|(&id, device)| {
                let dev = device.lock();
                if dev.device_type() == device_type {
                    Some(id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn find_devices_by_driver(&self, driver_name: &str) -> Vec<u32> {
        let devices = self.devices.lock();
        devices.iter()
            .filter_map(|(&id, device)| {
                let dev = device.lock();
                if dev.driver_name() == driver_name {
                    Some(id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn suspend_all_devices(&self) -> Result<(), DeviceError> {
        let devices = self.devices.lock();
        for device in devices.values() {
            let mut dev = device.lock();
            if dev.state() == DeviceState::Ready {
                dev.suspend()?;
            }
        }
        Ok(())
    }

    pub fn resume_all_devices(&self) -> Result<(), DeviceError> {
        let devices = self.devices.lock();
        for device in devices.values() {
            let mut dev = device.lock();
            if dev.state() == DeviceState::Suspended {
                dev.resume()?;
            }
        }
        Ok(())
    }

    pub fn enumerate_devices(&self) -> Vec<(u32, DeviceType, String, DeviceState)> {
        let devices = self.devices.lock();
        devices.iter()
            .map(|(&id, device)| {
                let dev = device.lock();
                (id, dev.device_type(), dev.device_name(), dev.state())
            })
            .collect()
    }

    pub fn get_device_stats(&self) -> BTreeMap<DeviceState, usize> {
        let devices = self.devices.lock();
        let mut stats = BTreeMap::new();

        for device in devices.values() {
            let dev = device.lock();
            let state = dev.state();
            *stats.entry(state).or_insert(0) += 1;
        }

        stats
    }

    pub fn handle_device_interrupt(&self, vector: InterruptVector) -> Result<(), DeviceError> {
        if let Some(ref interrupt_controller) = self.interrupt_controller {
            let controller = interrupt_controller.lock();
            controller.handle_interrupt(vector)?;
        }
        Ok(())
    }
}