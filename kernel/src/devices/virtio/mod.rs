//! VirtIO 1.4 transport（MMIO/PCI）、virtqueue 与各设备类 adapter。
//!
//! 实现 `drivers` 定义的设备类 seam（block、console、display、input、network、PCM output、
//! entropy、named port）与 `block::BlockDevice`；platform 在设备发现时构造 adapter 并发布到对应的
//! 注册表。通用内核只经 seam 使用设备，不感知本模块；本模块不依赖 `arch`，MMIO 访问与中断等待经
//! `drivers` 的 hal façade。

mod blk;
mod completion_irq;
mod console;
mod gpu;
mod input;
mod net;
mod pci;
mod queue;
mod rng;
mod sound;
mod transport;

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
