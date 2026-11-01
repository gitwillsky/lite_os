//! 块层：块设备 trait、设备注册表、分区表解析与分区视图。
//!
//! 硬件驱动实现 BlockDevice 并注册，fs 只消费块 I/O 与分区视图。
//! 分区表与字节范围属于块设备层；inode、目录、挂载及文件系统格式属于 fs。

use alloc::sync::Arc;

use crate::devices::registry::AppendOnlyRegistry;

mod device;
pub(crate) mod identity;
pub(crate) mod partition_device;
pub(crate) mod partition_table;
pub(crate) mod range;

pub(crate) use device::{BLOCK_SIZE, BlockDevice, BlockError, CompletionNotify, PendingRead};

// OWNER: 块设备 adapter 的唯一发布点，只追加；index 是 adapter 的稳定 identity，根文件系统与 `fs`
// 的 `/dev` 发布都按它选择。缺失时第二块盘只能 panic 或被丢弃。
static DEVICES: AppendOnlyRegistry<dyn BlockDevice> = AppendOnlyRegistry::new();

/// 按发现顺序发布一个块设备。
///
/// # Returns
///
/// 设备的稳定 index。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register(device: Arc<dyn BlockDevice>) -> Result<usize, Arc<dyn BlockDevice>> {
    DEVICES.register(device)
}

/// 第 `index` 个已发布的块设备。
pub(crate) fn device(index: usize) -> Option<Arc<dyn BlockDevice>> {
    DEVICES.get(index)
}
