//! VirtIO 1.4 modern PCI transport register codec.

mod msix;
use super::transport::VirtQueueAddresses;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use mmio::BusError;
use mmio::MmioBus;
pub(in crate::drivers::virtio) use msix::Msix;

const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0c;
const CONFIG_MSIX_VECTOR: usize = 0x10;
const NUM_QUEUES: usize = 0x12;
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

/// A validated modern VirtIO PCI capability set; the network adapter may own an MSI-X table.
pub(crate) struct PciTransport {
    common: MmioBus,
    notify: MmioBus,
    isr: MmioBus,
    device: Option<MmioBus>,
    notify_multiplier: u32,
    msix: Option<Msix>,
    // OWNER: configure_queue 在独占初始化事务内读取 queue_notify_off 并校验通知窗口后发表。
    // 运行时每队列只有原子读取与 doorbell write，不访问共享 queue_select；否则并发通知会串到别的队列。
    // usize::MAX 表示尚未配置，偏移 0 是合法通知位置（multiplier 可以是 0）。
    queue_notifications: Vec<AtomicUsize>,
}

impl PciTransport {
    /// Build one transport from capability-derived MMIO windows.
    ///
    /// # Parameters
    ///
    /// - `common`: `VIRTIO_PCI_CAP_COMMON_CFG` window.
    /// - `notify`: `VIRTIO_PCI_CAP_NOTIFY_CFG` window.
    /// - `isr`: `VIRTIO_PCI_CAP_ISR_CFG` window.
    /// - `device`: Optional device-specific configuration window.
    /// - `notify_multiplier`: Capability-defined notification stride.
    ///
    /// # Returns
    ///
    /// A transport whose accesses stay inside capability windows; None if the queue count
    /// cannot be read, is zero, or notification storage cannot be allocated.
    pub(in crate::drivers::virtio) fn new(
        common: MmioBus,
        notify: MmioBus,
        isr: MmioBus,
        device: Option<MmioBus>,
        notify_multiplier: u32,
        msix: Option<Msix>,
    ) -> Option<Self> {
        let queues = usize::from(common.read_u16(NUM_QUEUES).ok()?);
        if queues == 0 {
            return None;
        }
        let mut queue_notifications = Vec::new();
        queue_notifications.try_reserve_exact(queues).ok()?;
        queue_notifications.resize_with(queues, || AtomicUsize::new(usize::MAX));
        Some(Self {
            common,
            notify,
            isr,
            device,
            notify_multiplier,
            msix,
            queue_notifications,
        })
    }

    pub(crate) fn initialize(&self) -> Result<(), BusError> {
        if let Some(msix) = &self.msix {
            msix.initialize()?;
        }
        for notification in &self.queue_notifications {
            notification.store(usize::MAX, Ordering::Release);
        }
        self.common.write_u16(CONFIG_MSIX_VECTOR, NO_VECTOR)
    }

    /// 本 transport 实际拥有的消息表；没有平台 MSI authority 或 adapter 支持时为 None。
    pub(in crate::drivers::virtio) fn message_interrupts(&self) -> Option<&Msix> {
        self.msix.as_ref()
    }

    /// 配置变化使用同组的专用表索引；设备拒绝时不能启用消息源。
    pub(in crate::drivers::virtio) fn assign_config_interrupt(
        &self,
        vector: u16,
    ) -> Result<(), BusError> {
        if self
            .msix
            .as_ref()
            .is_none_or(|msix| vector >= msix.capacity())
        {
            return Err(BusError::InvalidAddress);
        }
        self.common.write_u16(CONFIG_MSIX_VECTOR, vector)?;
        if self.common.read_u16(CONFIG_MSIX_VECTOR)? != vector {
            return Err(BusError::InvalidAddress);
        }
        Ok(())
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
        self.queue_notifications
            .get(usize::from(index))
            .ok_or(BusError::InvalidAddress)?;
        self.common.write_u16(QUEUE_SELECT, index)?;
        let size = self.common.read_u16(QUEUE_SIZE)?;
        if size == 0 || self.common.read_u16(QUEUE_ENABLE)? != 0 {
            return Err(BusError::InvalidAddress);
        }
        Ok(size)
    }

    pub(in crate::drivers::virtio) fn configure_queue(
        &self,
        index: u32,
        requested: u16,
        addresses: VirtQueueAddresses,
        interrupt: Option<u16>,
    ) -> Result<(), BusError> {
        let index = u16::try_from(index).map_err(|_| BusError::InvalidAddress)?;
        let notification = self
            .queue_notifications
            .get(usize::from(index))
            .ok_or(BusError::InvalidAddress)?;
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
        let vector = interrupt.unwrap_or(NO_VECTOR);
        if interrupt.is_some_and(|vector| {
            self.msix
                .as_ref()
                .is_none_or(|msix| vector >= msix.capacity())
        }) {
            return Err(BusError::InvalidAddress);
        }
        self.common.write_u16(QUEUE_MSIX_VECTOR, vector)?;
        if self.common.read_u16(QUEUE_MSIX_VECTOR)? != vector {
            return Err(BusError::InvalidAddress);
        }
        self.write_u64(QUEUE_DESC, addresses.descriptor)?;
        self.write_u64(QUEUE_DRIVER, addresses.driver)?;
        self.write_u64(QUEUE_DEVICE, addresses.device)?;
        let offset = usize::from(self.common.read_u16(QUEUE_NOTIFY_OFF)?)
            .checked_mul(self.notify_multiplier as usize)
            .ok_or(BusError::InvalidAddress)?;
        // VirtIO 1.4 §4.1.4.4：偏移必须能容纳未协商 NOTIFICATION_DATA 的 16-bit queue index。
        self.notify.subwindow(offset, size_of::<u16>())?;
        self.common.write_u16(QUEUE_ENABLE, 1)?;
        notification.store(offset, Ordering::Release);
        Ok(())
    }

    pub(crate) fn notify_queue(&self, index: u32) -> Result<(), BusError> {
        let index = u16::try_from(index).map_err(|_| BusError::InvalidAddress)?;
        let offset = self
            .queue_notifications
            .get(usize::from(index))
            .ok_or(BusError::InvalidAddress)?
            .load(Ordering::Acquire);
        if offset == usize::MAX {
            return Err(BusError::InvalidAddress);
        }
        mmio::before_write();
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

    pub(crate) fn read_config_u16(&self, offset: usize) -> Result<u16, BusError> {
        self.device
            .as_ref()
            .ok_or(BusError::InvalidAddress)?
            .read_u16(offset)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_keep_their_configured_queue_address_without_selecting_a_queue() {
        let mut common_bytes = [0u64; 8];
        let mut notify_bytes = [u16::MAX; 2];
        let mut isr_byte = 0u8;
        let common = MmioBus::new(
            common_bytes.as_mut_ptr() as usize,
            size_of_val(&common_bytes),
        )
        .unwrap();
        let notify = MmioBus::new(
            notify_bytes.as_mut_ptr() as usize,
            size_of_val(&notify_bytes),
        )
        .unwrap();
        let isr = MmioBus::new(&raw mut isr_byte as usize, 1).unwrap();
        common.write_u16(NUM_QUEUES, 2).unwrap();
        let device = PciTransport::new(common, notify, isr, None, 2, None).unwrap();
        device.initialize().unwrap();
        assert_eq!(device.notify_queue(0), Err(BusError::InvalidAddress));
        for queue in 0..2 {
            // 用 RAM 寄存器窗口提供设备事实：每次切换到尚未启用的 queue bank。
            common.write_u16(QUEUE_ENABLE, 0).unwrap();
            common.write_u16(QUEUE_SIZE, 16).unwrap();
            common.write_u16(QUEUE_NOTIFY_OFF, queue).unwrap();
            device
                .configure_queue(
                    u32::from(queue),
                    16,
                    VirtQueueAddresses {
                        descriptor: 0x1000,
                        driver: 0x2000,
                        device: 0x3000,
                    },
                    None,
                )
                .unwrap();
        }
        // 配置停在 queue 1；通知 queue 0 不得改写 selector 或再读 queue 1 的偏移。
        device.notify_queue(0).unwrap();
        assert_eq!(common.read_u16(QUEUE_SELECT).unwrap(), 1);
        assert_eq!(notify.read_u16(0).unwrap(), 0);
        assert_eq!(notify.read_u16(2).unwrap(), u16::MAX);
        device.notify_queue(1).unwrap();
        assert_eq!(notify.read_u16(2).unwrap(), 1);
        assert_eq!(device.notify_queue(2), Err(BusError::InvalidAddress));
    }
}
