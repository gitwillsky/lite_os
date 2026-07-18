use alloc::sync::Arc;
use spin::{Mutex, Once};

use super::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VirtIODevice,
    virtio_queue::VirtQueue,
};

/// OWNER: virtio-rng driver owns the only kernel entropy device and serializes its queue.
static ENTROPY_DEVICE: Once<Arc<VirtIORngDevice>> = Once::new();

pub(crate) struct VirtIORngDevice {
    device: VirtIODevice,
    queue: Mutex<VirtQueue>,
}

impl VirtIORngDevice {
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 4 {
            return None;
        }
        device.initialize().ok()?;
        if device.device_features().ok()? & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        device.set_driver_features(VIRTIO_F_VERSION_1).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }
        let size = device.queue_max_size(0).ok()?;
        let queue = VirtQueue::new(size)?;
        device.configure_queue(0, size, queue.addresses()).ok()?;
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        Arc::try_new(Self {
            device,
            queue: Mutex::new(queue),
        })
        .ok()
    }

    fn fill(&self, bytes: &mut [u8]) -> Result<(), ()> {
        let mut queue = self.queue.lock();
        let expected = bytes.len();
        let mut outputs = [bytes];
        let descriptor = queue.add_buffer(&[], &mut outputs).ok_or(())?;
        queue.add_to_avail(descriptor);
        self.device.notify_queue(0).map_err(|_| ())?;
        loop {
            if self.device.interrupt_status().unwrap_or(0) & 1 != 0 {
                let _ = self.device.interrupt_ack(1);
            }
            match queue.used() {
                Ok(Some((id, length))) if id == descriptor && length as usize == expected => {
                    return Ok(());
                }
                Ok(Some(_)) => return Err(()),
                Ok(None) => core::hint::spin_loop(),
                Err(()) => return Err(()),
            }
        }
    }
}

pub(super) fn register(device: Arc<VirtIORngDevice>) -> Result<(), ()> {
    if ENTROPY_DEVICE.get().is_some() {
        return Err(());
    }
    ENTROPY_DEVICE.call_once(|| device);
    Ok(())
}

/// @description 从唯一 virtio-rng entropy source 同步填满缓冲区。
pub(crate) fn fill_entropy(bytes: &mut [u8]) -> Result<(), ()> {
    ENTROPY_DEVICE.get().ok_or(())?.fill(bytes)
}
