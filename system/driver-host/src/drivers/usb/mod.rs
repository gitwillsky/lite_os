//! USB host 与标准 USB Audio Class 2 playback；不拥有 ALSA 或宿主音频策略。
mod audio;
mod dma;
mod xhci;
use alloc::sync::Arc;
pub(crate) use xhci::Host;

/// USB 初始化或事务错误；传输超时不会允许重用未停止的 DMA。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Memory,
    Protocol,
    Unsupported,
    Busy,
    Disconnected,
    Timeout,
    Controller(u8),
}
type Result<T> = core::result::Result<T, Error>;
impl From<mmio::BusError> for Error {
    fn from(_: mmio::BusError) -> Self {
        Self::Protocol
    }
}

/// 初始化本节点的 xHCI 主控，返回由 composition root 启动的设备枚举工作者。
///
/// # Errors
/// 节点不是 PCI function、内存不足或控制器初始化失败返回错误，不发布半初始化的音频设备。
pub(crate) fn initialize() -> Result<Arc<Host>> {
    let function = crate::drivers::pci::function().ok_or(Error::Unsupported)?;
    Host::new(&function, 1)
}
