//! 设备的 Linux 用户态 ABI 与设备级状态；设备本身由 devmgr 驱动，经设备目录以远端 client 接入。
pub(crate) mod audio;
pub(crate) mod block;
pub(crate) mod console;
pub(crate) mod directory;
pub(crate) mod drm;
pub(crate) mod input;
pub(crate) mod tty;
pub(crate) mod usb;
pub(crate) mod virtio_port;
