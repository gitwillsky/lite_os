//! 设备模型、设备级状态与用户态 ABI；硬件 adapter 实现本域接口，总线协议由 drivers 拥有。
pub(crate) mod audio;
pub(crate) mod block;
pub(crate) mod console;
pub(crate) mod drm;
pub(crate) mod input;
pub(crate) mod model;
pub(crate) mod registry;
pub(crate) mod tty;
pub(crate) mod virtio_port;
