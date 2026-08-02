mod bus;
mod interrupt;
mod virtio;
#[cfg(target_arch = "aarch64")]
mod virtio_pci;

pub(crate) use bus::MmioBus;
pub(crate) use interrupt::{InterruptError, InterruptHandler, InterruptVector};
pub(super) use virtio::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice, VirtQueueAddresses,
};
#[cfg(target_arch = "aarch64")]
pub(crate) use virtio_pci::PciTransport;
