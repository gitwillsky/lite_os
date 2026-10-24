//! Wayland 服务端：每个连接一个 [`Client`] 状态机，只处理字节与 fd，不拥有 socket。

/// 唯一输出的几何；宣告给 `wl_output`，并决定窗口放大倍数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    pub width: i32,
    pub height: i32,
    pub scale: i32,
}

mod client;
mod server;
mod shm;
mod surface;
mod xdg;

pub use client::Client;
#[cfg(test)]
pub use client::Disconnected;
pub use server::{ClientId, Server};
pub use surface::Content;
pub use xdg::{Window, WindowRequest};

#[cfg(test)]
mod tests;
