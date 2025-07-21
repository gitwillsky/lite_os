pub mod block;
pub mod device_manager;
pub mod virtio_mmio;
pub mod virtio_blk;
pub mod virtio_console;
pub mod virtio_queue;

pub use block::BlockDevice;
pub use device_manager::{init_devices, handle_external_interrupt};
pub use virtio_blk::VirtIOBlockDevice;
pub use virtio_console::{
    init_virtio_console, virtio_console_write, virtio_console_read, virtio_console_has_input,
    is_virtio_console_available,
};