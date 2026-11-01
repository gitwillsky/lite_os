//! linuxd 使用的平台能力：内核串口 console、整机电源与启动参数。
//!
//! 设备发现、MMIO、中断与 DMA 属于 devmgr；linuxd 只经设备目录使用远端设备。

mod serial;

use native_runtime::native_abi;

pub(crate) use serial::register_console;

/// Linux command line：根服务转交的启动参数（内核已取走其自身选项）。
pub(crate) fn kernel_command_line() -> &'static [u8] {
    crate::handles::command_line()
}

/// 关机或重启的目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetKind {
    Shutdown,
    ColdReboot,
}

/// 关机或重启失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResetError(pub(crate) native_abi::Status);

impl core::fmt::Display for ResetError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "system power request failed: {}", self.0)
    }
}

/// 请求整机关机或重启。
///
/// # Errors
///
/// 内核拒绝请求。
pub(crate) fn reset_system(kind: ResetKind) -> Result<(), ResetError> {
    let action = match kind {
        ResetKind::Shutdown => native_abi::system::POWER_OFF,
        ResetKind::ColdReboot => native_abi::system::POWER_REBOOT,
    };
    native_runtime::system::power(crate::handles::resource(), action).map_err(ResetError)
}
