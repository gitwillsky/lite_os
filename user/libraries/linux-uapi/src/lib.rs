//! Safe, product-scoped Linux interfaces absent from [`std`].
//!
//! The public modules expose owned resources and [`std::io::Result`]. Raw musl
//! declarations and Linux 7.1 UAPI layouts remain private to this crate.

extern crate liteos_runtime as _;
pub mod alsa;
pub mod credentials;
pub mod drm;
pub mod dynamic_library;
pub mod filesystem;
pub mod input;
pub mod kobject;
pub mod process;
pub mod pty;
/// Linux 链路报文接收。
#[cfg(target_os = "linux")]
pub mod packet;
/// Linux 路由套接字的只读接口查询。
#[cfg(target_os = "linux")]
pub mod rtnetlink;
mod raw;
pub mod shared_memory;
pub mod time;
pub mod unix;
pub mod usb;
pub mod uts;
pub mod virtio_port;

pub use process::{Pid, Signal};
