//! USB sysfs 只读发现来源；不另存连接状态。
use super::model::{self, UsbDevice};
use alloc::sync::Arc;
struct Source;
impl crate::fs::UsbSysfsSource for Source {
    fn count(&self) -> usize {
        model::usb_device_count()
    }
    fn device(&self, index: usize) -> Option<Arc<dyn UsbDevice>> {
        model::usb_device(index)
    }
}
/// 装配 USB sysfs 来源；重复装配或分配失败返回错误，不发布猜测设备。
pub(crate) fn initialize() -> Result<(), crate::fs::FileSystemError> {
    let source = Arc::try_new(Source).map_err(|_| crate::fs::FileSystemError::OutOfMemory)?;
    crate::fs::register_usb_sysfs(source)
}
