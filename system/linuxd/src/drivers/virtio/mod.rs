//! VirtIO 1.4 transport（MMIO/PCI）、virtqueue 与各设备类 adapter。
//!
//! 实现 devices::model 与 devices::block 的接口；平台提供发现事实，main 串接启动装配。
//! MMIO 只经 arch::mmio::MmioBus 访问，ISA 机制只调用静态 arch façade，不访问具体 backend。

mod binding;
mod blk;
mod completion_irq;
mod console;
mod gpu;
mod input;
mod net;
mod pci;
mod pci_discovery;
mod queue;
mod rng;
mod sound;
mod transport;

pub(crate) use binding::{EarlyDisplay, bind_display, bind_platform_devices};
pub(crate) use blk::VirtIOBlockDevice;
pub(crate) use console::VirtIOConsoleDevice;
pub(crate) use gpu::VirtIOGpuDevice;
pub(crate) use input::VirtIOInputDevice;
pub(crate) use net::VirtIONetworkDevice;
pub(crate) use pci::PciTransport;
pub(crate) use rng::VirtIORngDevice;
pub(crate) use sound::VirtIOSoundDevice;
use transport::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
};
