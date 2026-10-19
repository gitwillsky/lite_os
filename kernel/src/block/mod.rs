//! 块层：块设备 trait、设备注册表、分区表解析与分区视图。
//!
//! 位于 `drivers` 之上、`fs` 之下：驱动（virtio-blk 等）实现 [`BlockDevice`] 并在 platform 装配时发布；
//! `fs` 只依赖本模块读写块与解析分区，不感知任何具体驱动。分区表解析、字节范围 I/O 与身份文本都是
//! 纯函数，可在 host 上单测。

use alloc::sync::Arc;

use crate::hal::registry::AppendOnlyRegistry;

mod device;
pub(crate) mod identity;
pub(crate) mod partition_device;
pub(crate) mod partition_table;
pub(crate) mod range;

pub(crate) use device::{BLOCK_SIZE, BlockDevice, BlockError};

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
