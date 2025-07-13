pub mod block;
pub mod device_manager;
pub mod virtio_mmio;
pub mod virtio_blk;
pub mod virtio_queue;

pub use block::BlockDevice;
pub use device_manager::{init_devices, handle_external_interrupt};
pub use virtio_blk::VirtIOBlockDevice;