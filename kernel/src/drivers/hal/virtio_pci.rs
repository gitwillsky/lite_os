//! @description VirtIO 1.4 modern PCI transport register codec.

use super::bus::{BusError, MmioBus};
use super::virtio::VirtQueueAddresses;

const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0c;
const CONFIG_MSIX_VECTOR: usize = 0x10;
const DEVICE_STATUS: usize = 0x14;
const CONFIG_GENERATION: usize = 0x15;
const QUEUE_SELECT: usize = 0x16;
const QUEUE_SIZE: usize = 0x18;
const QUEUE_MSIX_VECTOR: usize = 0x1a;
const QUEUE_ENABLE: usize = 0x1c;
const QUEUE_NOTIFY_OFF: usize = 0x1e;
const QUEUE_DESC: usize = 0x20;
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;
const NO_VECTOR: u16 = u16::MAX;

/// @description A validated modern VirtIO PCI capability set using legacy INTx.
pub(crate) struct PciTransport {
    common: MmioBus,
    notify: MmioBus,
    isr: MmioBus,
    device: Option<MmioBus>,
    notify_multiplier: u32,
}

impl PciTransport {
    /// @description Build one transport from capability-derived MMIO windows.
    /// @param common `VIRTIO_PCI_CAP_COMMON_CFG` window.
    /// @param notify `VIRTIO_PCI_CAP_NOTIFY_CFG` window.
    /// @param isr `VIRTIO_PCI_CAP_ISR_CFG` window.
    /// @param device Optional device-specific configuration window.
    /// @param notify_multiplier Capability-defined notification stride.
    /// @return A transport whose accesses stay inside capability windows.
    pub(crate) fn new(
        common: MmioBus,
        notify: MmioBus,
        isr: MmioBus,
        device: Option<MmioBus>,
        notify_multiplier: u32,
    ) -> Self {
        Self {
            common,
            notify,
            isr,
            device,
            notify_multiplier,
        }
    }

    pub(crate) fn initialize(&self) -> Result<(), BusError> {
        self.common.write_u16(CONFIG_MSIX_VECTOR, NO_VECTOR)
    }

    pub(crate) fn set_driver_features(&self, features: u64) -> Result<(), BusError> {
        self.common.write_u32(DRIVER_FEATURE_SELECT, 0)?;
        self.common.write_u32(DRIVER_FEATURE, features as u32)?;
        self.common.write_u32(DRIVER_FEATURE_SELECT, 1)?;
        self.common
            .write_u32(DRIVER_FEATURE, (features >> 32) as u32)
    }

    pub(crate) fn device_features(&self) -> Result<u64, BusError> {
        self.common.write_u32(DEVICE_FEATURE_SELECT, 0)?;
        let low = self.common.read_u32(DEVICE_FEATURE)?;
        self.common.write_u32(DEVICE_FEATURE_SELECT, 1)?;
        let high = self.common.read_u32(DEVICE_FEATURE)?;
        Ok(u64::from(low) | u64::from(high) << 32)
    }

    pub(crate) fn set_status(&self, status: u32) -> Result<(), BusError> {
        self.common.write_u8(DEVICE_STATUS, status as u8)
    }

    pub(crate) fn get_status(&self) -> Result<u32, BusError> {
        self.common.read_u8(DEVICE_STATUS).map(u32::from)
    }

    pub(crate) fn queue_max_size(&self, index: u32) -> Result<u16, BusError> {
        let index = u16::try_from(index).map_err(|_| BusError::InvalidAddress)?;
        self.common.write_u16(QUEUE_SELECT, index)?;
        let size = self.common.read_u16(QUEUE_SIZE)?;
        if size == 0 || self.common.read_u16(QUEUE_ENABLE)? != 0 {
            return Err(BusError::InvalidAddress);
        }
        Ok(size)
    }

    pub(in crate::drivers) fn configure_queue(
        &self,
        index: u32,
        requested: u16,
        addresses: VirtQueueAddresses,
    ) -> Result<(), BusError> {
        let index = u16::try_from(index).map_err(|_| BusError::InvalidAddress)?;
        self.common.write_u16(QUEUE_SELECT, index)?;
        let maximum = self.common.read_u16(QUEUE_SIZE)?;
        if maximum == 0
            || requested > maximum
            || !requested.is_power_of_two()
            || self.common.read_u16(QUEUE_ENABLE)? != 0
        {
            return Err(BusError::InvalidAddress);
        }
        self.common.write_u16(QUEUE_SIZE, requested)?;
        self.common.write_u16(QUEUE_MSIX_VECTOR, NO_VECTOR)?;
        self.write_u64(QUEUE_DESC, addresses.descriptor)?;
        self.write_u64(QUEUE_DRIVER, addresses.driver)?;
        self.write_u64(QUEUE_DEVICE, addresses.device)?;
        self.common.write_u16(QUEUE_ENABLE, 1)
    }

    pub(crate) fn notify_queue(&self, index: u32) -> Result<(), BusError> {
        let index = u16::try_from(index).map_err(|_| BusError::InvalidAddress)?;
        self.common.write_u16(QUEUE_SELECT, index)?;
        let queue_offset = u32::from(self.common.read_u16(QUEUE_NOTIFY_OFF)?);
        let offset = queue_offset
            .checked_mul(self.notify_multiplier)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(BusError::InvalidAddress)?;
        crate::arch::before_mmio_write();
        self.notify.write_u16(offset, index)
    }

    pub(crate) fn interrupt_status(&self) -> Result<u32, BusError> {
        self.isr.read_u8(0).map(u32::from)
    }

    pub(crate) fn read_config_u32(&self, offset: usize) -> Result<u32, BusError> {
        self.device
            .as_ref()
            .ok_or(BusError::InvalidAddress)?
            .read_u32(offset)
    }

    pub(crate) fn write_config_u32(&self, offset: usize, value: u32) -> Result<(), BusError> {
        self.device
            .as_ref()
            .ok_or(BusError::InvalidAddress)?
            .write_u32(offset, value)
    }

    pub(crate) fn read_config_u8(&self, offset: usize) -> Result<u8, BusError> {
        self.device
            .as_ref()
            .ok_or(BusError::InvalidAddress)?
            .read_u8(offset)
    }

    pub(crate) fn write_config_u8(&self, offset: usize, value: u8) -> Result<(), BusError> {
        self.device
            .as_ref()
            .ok_or(BusError::InvalidAddress)?
            .write_u8(offset, value)
    }

    pub(crate) fn config_generation(&self) -> Result<u32, BusError> {
        self.common.read_u8(CONFIG_GENERATION).map(u32::from)
    }

    fn write_u64(&self, offset: usize, value: u64) -> Result<(), BusError> {
        self.common.write_u32(offset, value as u32)?;
        self.common.write_u32(offset + 4, (value >> 32) as u32)
    }
}
