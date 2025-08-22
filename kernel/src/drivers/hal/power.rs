use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    D0,     // Fully On
    D1,     // Low Power, device context retained
    D2,     // Lower Power, device context may be lost
    D3Hot,  // Lowest power, device context lost, power still supplied
    D3Cold, // Off, no power supplied
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerError {
    InvalidState,
    TransitionFailed,
    NotSupported,
    DeviceError,
    TimeoutError,
}

impl fmt::Display for PowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PowerError::InvalidState => write!(f, "Invalid power state"),
            PowerError::TransitionFailed => write!(f, "Power state transition failed"),
            PowerError::NotSupported => write!(f, "Power management not supported"),
            PowerError::DeviceError => write!(f, "Device power error"),
            PowerError::TimeoutError => write!(f, "Power operation timeout"),
        }
    }
}

pub trait PowerManagement: Send + Sync {
    fn current_state(&self) -> PowerState;

    fn transition_to(&mut self, state: PowerState) -> Result<(), PowerError>;

    fn can_wakeup(&self) -> bool {
        false
    }

    fn enable_wakeup(&mut self) -> Result<(), PowerError> {
        Err(PowerError::NotSupported)
    }

    fn disable_wakeup(&mut self) -> Result<(), PowerError> {
        Err(PowerError::NotSupported)
    }

    fn suspend(&mut self) -> Result<(), PowerError> {
        self.transition_to(PowerState::D3Hot)
    }

    fn resume(&mut self) -> Result<(), PowerError> {
        self.transition_to(PowerState::D0)
    }

    fn power_consumption(&self) -> u32 {
        0
    }

    fn supported_states(&self) -> Vec<PowerState> {
        vec![PowerState::D0, PowerState::D3Hot]
    }
}

pub struct DevicePowerManager {
    current_state: Mutex<PowerState>,
    supported_states: Vec<PowerState>,
    wakeup_enabled: Mutex<bool>,
    power_consumption: [u32; 5], // Power consumption for each D-state
}

impl DevicePowerManager {
    pub fn new() -> Self {
        Self {
            current_state: Mutex::new(PowerState::D3Cold),
            supported_states: vec![
                PowerState::D0,
                PowerState::D1,
                PowerState::D2,
                PowerState::D3Hot,
                PowerState::D3Cold,
            ],
            wakeup_enabled: Mutex::new(false),
            power_consumption: [100, 50, 25, 5, 0], // Example values in mW
        }
    }

    pub fn with_supported_states(supported_states: Vec<PowerState>) -> Self {
        Self {
            current_state: Mutex::new(PowerState::D3Cold),
            supported_states,
            wakeup_enabled: Mutex::new(false),
            power_consumption: [100, 50, 25, 5, 0],
        }
    }

    fn state_to_index(state: PowerState) -> usize {
        match state {
            PowerState::D0 => 0,
            PowerState::D1 => 1,
            PowerState::D2 => 2,
            PowerState::D3Hot => 3,
            PowerState::D3Cold => 4,
        }
    }

    fn is_valid_transition(&self, from: PowerState, to: PowerState) -> bool {
        use PowerState::*;

        matches!(
            (from, to),
            (D3Cold, D0)
                | (D0, D1)
                | (D1, D0)
                | (D1, D2)
                | (D2, D1)
                | (D2, D3Hot)
                | (D3Hot, D2)
                | (D3Hot, D3Cold)
                | (D0, D3Hot)
                | (D0, D3Cold)
                | (D3Cold, D3Hot)
        )
    }

    fn perform_transition(&self, to_state: PowerState) -> Result<(), PowerError> {
        use PowerState::*;

        match to_state {
            D0 => {
                // Power on device, restore context if needed
                // This would involve hardware-specific operations
            }
            D1 | D2 => {
                // Enter low power state, may retain some context
            }
            D3Hot => {
                // Enter lowest power state while still supplying power
            }
            D3Cold => {
                // Turn off power completely
            }
        }

        Ok(())
    }
}

impl PowerManagement for DevicePowerManager {
    fn current_state(&self) -> PowerState {
        *self.current_state.lock()
    }

    fn transition_to(&mut self, state: PowerState) -> Result<(), PowerError> {
        let current = *self.current_state.lock();

        if current == state {
            return Ok(());
        }

        if !self.supported_states.contains(&state) {
            return Err(PowerError::NotSupported);
        }

        if !self.is_valid_transition(current, state) {
            return Err(PowerError::InvalidState);
        }

        self.perform_transition(state)?;

        *self.current_state.lock() = state;
        Ok(())
    }

    fn can_wakeup(&self) -> bool {
        true
    }

    fn enable_wakeup(&mut self) -> Result<(), PowerError> {
        *self.wakeup_enabled.lock() = true;
        Ok(())
    }

    fn disable_wakeup(&mut self) -> Result<(), PowerError> {
        *self.wakeup_enabled.lock() = false;
        Ok(())
    }

    fn power_consumption(&self) -> u32 {
        let state = *self.current_state.lock();
        let index = Self::state_to_index(state);
        self.power_consumption[index]
    }

    fn supported_states(&self) -> Vec<PowerState> {
        self.supported_states.clone()
    }
}

pub trait PowerDomain: Send + Sync {
    fn add_device(&mut self, device_id: u32) -> Result<(), PowerError>;
    fn remove_device(&mut self, device_id: u32) -> Result<(), PowerError>;
    fn power_on(&mut self) -> Result<(), PowerError>;
    fn power_off(&mut self) -> Result<(), PowerError>;
    fn is_powered(&self) -> bool;
}

pub struct SimplePowerDomain {
    devices: Mutex<Vec<u32>>,
    powered: Mutex<bool>,
}

impl SimplePowerDomain {
    pub fn new() -> Self {
        Self {
            devices: Mutex::new(Vec::new()),
            powered: Mutex::new(false),
        }
    }
}

impl PowerDomain for SimplePowerDomain {
    fn add_device(&mut self, device_id: u32) -> Result<(), PowerError> {
        let mut devices = self.devices.lock();
        if !devices.contains(&device_id) {
            devices.push(device_id);
        }
        Ok(())
    }

    fn remove_device(&mut self, device_id: u32) -> Result<(), PowerError> {
        let mut devices = self.devices.lock();
        devices.retain(|&id| id != device_id);
        Ok(())
    }

    fn power_on(&mut self) -> Result<(), PowerError> {
        // Hardware-specific power domain control would go here
        *self.powered.lock() = true;
        Ok(())
    }

    fn power_off(&mut self) -> Result<(), PowerError> {
        // Check if all devices in domain can be powered off
        // Hardware-specific power domain control would go here
        *self.powered.lock() = false;
        Ok(())
    }

    fn is_powered(&self) -> bool {
        *self.powered.lock()
    }
}
