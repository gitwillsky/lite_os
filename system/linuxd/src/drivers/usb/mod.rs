//! USB host 与标准 USB Audio Class 2 playback；不拥有 ALSA 或宿主音频策略。
mod audio;
mod dma;
mod xhci;
use alloc::{sync::Arc, vec::Vec};
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
impl From<crate::arch::mmio::BusError> for Error {
    fn from(_: crate::arch::mmio::BusError) -> Self {
        Self::Protocol
    }
}

/// 发现 PCI xHCI，返回由 composition root 启动的设备枚举工作者。
///
/// # Returns
/// 每个已初始化控制器一个永久 DMA owner；无控制器时返回空列表。
/// # Errors
/// 内存不足或控制器初始化失败返回错误，不发布半初始化的音频设备。
pub(crate) fn initialize() -> Result<Vec<Arc<Host>>> {
    let mut hosts = Vec::new();
    for function in crate::drivers::pci::functions() {
        if function.config.read_u32(8)? >> 8 != 0x0c0330 {
            continue;
        }
        hosts.try_reserve(1).map_err(|_| Error::Memory)?;
        hosts.push(Host::new(function, hosts.len() + 1)?);
    }
    Ok(hosts)
}
