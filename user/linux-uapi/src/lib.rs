//! Safe, product-scoped Linux interfaces absent from [`std`].
//!
//! The public modules expose owned resources and [`std::io::Result`]. Raw musl
//! declarations and Linux 7.1 UAPI layouts remain private to this crate.

pub mod alsa;
pub mod drm;
pub mod input;
pub mod process;
pub mod pty;
mod raw;
pub mod shared_memory;
pub mod unix;
pub mod virtio_port;

pub use process::{Pid, Signal};
